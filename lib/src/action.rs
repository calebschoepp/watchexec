//! Processor responsible for receiving events, filtering them, and scheduling actions in response.

use std::{
	sync::Arc,
	time::{Duration, Instant},
};

use clearscreen::ClearScreen;
use tokio::{
	sync::{mpsc, watch},
	time::timeout,
};
use tracing::{debug, trace, warn};

use crate::{
	command::Supervisor,
	error::{CriticalError, RuntimeError},
	event::Event,
	handler::{rte, Handler},
};

#[doc(inline)]
pub use outcome::Outcome;
#[doc(inline)]
pub use workingdata::*;

mod outcome;
mod workingdata;

pub async fn worker(
	working: watch::Receiver<WorkingData>,
	errors: mpsc::Sender<RuntimeError>,
	events_tx: mpsc::Sender<Event>,
	mut events: mpsc::Receiver<Event>,
) -> Result<(), CriticalError> {
	let mut last = Instant::now();
	let mut set = Vec::new();
	let mut process: Option<Supervisor> = None;

	let mut action_handler =
		{ working.borrow().action_handler.take() }.ok_or(CriticalError::MissingHandler)?;
	let mut pre_spawn_handler =
		{ working.borrow().pre_spawn_handler.take() }.ok_or(CriticalError::MissingHandler)?;
	let mut post_spawn_handler =
		{ working.borrow().post_spawn_handler.take() }.ok_or(CriticalError::MissingHandler)?;

	loop {
		let maxtime = if set.is_empty() {
			trace!("nothing in set, waiting forever for next event");
			Duration::from_secs(u64::MAX)
		} else {
			working.borrow().throttle.saturating_sub(last.elapsed())
		};

		if maxtime.is_zero() {
			if set.is_empty() {
				trace!("out of throttle but nothing to do, resetting");
				last = Instant::now();
				continue;
			} else {
				trace!("out of throttle on recycle");
			}
		} else {
			trace!(?maxtime, "waiting for event");
			match timeout(maxtime, events.recv()).await {
				Err(_timeout) => {
					trace!("timed out, cycling");
					continue;
				}
				Ok(None) => break,
				Ok(Some(event)) => {
					trace!(?event, "got event");

					if event.is_empty() {
						trace!("empty event, by-passing filters");
					} else {
						let filtered = working.borrow().filterer.check_event(&event);
						match filtered {
							Err(err) => {
								trace!(%err, "filter errored on event");
								errors.send(err).await?;
								continue;
							}
							Ok(false) => {
								trace!("filter rejected event");
								continue;
							}
							Ok(true) => {
								trace!("filter passed event");
							}
						}
					}

					if set.is_empty() {
						trace!("event is the first, resetting throttle window");
						last = Instant::now();
					}

					set.push(event);

					let elapsed = last.elapsed();
					if elapsed < working.borrow().throttle {
						trace!(?elapsed, "still within throttle window, cycling");
						continue;
					}
				}
			}
		}

		trace!("out of throttle, starting action process");
		last = Instant::now();

		let action = Action::new(set.drain(..).collect());
		debug!(?action, "action constructed");

		if let Some(h) = working.borrow().action_handler.take() {
			trace!("action handler updated");
			action_handler = h;
		}

		if let Some(h) = working.borrow().pre_spawn_handler.take() {
			trace!("pre-spawn handler updated");
			pre_spawn_handler = h;
		}

		if let Some(h) = working.borrow().post_spawn_handler.take() {
			trace!("post-spawn handler updated");
			post_spawn_handler = h;
		}

		debug!("running action handler");
		let outcome = action.outcome.clone();
		let err = action_handler
			.handle(action)
			.map_err(|e| rte("action worker", e));
		if let Err(err) = err {
			errors.send(err).await?;
			debug!("action handler errored, skipping");
			continue;
		}

		let outcome = outcome.get().cloned().unwrap_or_default();
		debug!(?outcome, "handler finished");

		let is_running = process.as_ref().map(|p| p.is_running()).unwrap_or(false);
		let outcome = outcome.resolve(is_running);
		debug!(?outcome, "outcome resolved");

		let w = working.borrow().clone();
		let rerr = apply_outcome(
			outcome,
			w,
			&mut process,
			&mut pre_spawn_handler,
			&mut post_spawn_handler,
			errors.clone(),
			events_tx.clone(),
		)
		.await;
		if let Err(err) = rerr {
			errors.send(err).await?;
		}
	}

	debug!("action worker finished");
	Ok(())
}

#[async_recursion::async_recursion]
async fn apply_outcome(
	outcome: Outcome,
	working: WorkingData,
	process: &mut Option<Supervisor>,
	pre_spawn_handler: &mut Box<dyn Handler<PreSpawn> + Send>,
	post_spawn_handler: &mut Box<dyn Handler<PostSpawn> + Send>,
	errors: mpsc::Sender<RuntimeError>,
	events: mpsc::Sender<Event>,
) -> Result<(), RuntimeError> {
	trace!(?outcome, "applying outcome");
	match (process.as_mut(), outcome) {
		(_, Outcome::DoNothing) => {}
		(_, Outcome::Exit) => {
			return Err(RuntimeError::Exit);
		}
		(Some(p), Outcome::Stop) => {
			p.kill().await;
			p.wait().await?;
			*process = None;
		}
		(None, o @ Outcome::Stop) | (None, o @ Outcome::Wait) | (None, o @ Outcome::Signal(_)) => {
			debug!(outcome=?o, "meaningless without a process, not doing anything");
		}
		(_, Outcome::Start) => {
			if working.command.is_empty() {
				warn!("tried to start a command without anything to run");
			} else {
				let command = working.shell.to_command(&working.command);
				let (pre_spawn, command) = PreSpawn::new(command, working.command.clone());

				debug!("running pre-spawn handler");
				pre_spawn_handler
					.handle(pre_spawn)
					.map_err(|e| rte("action pre-spawn", e))?;

				let mut command = Arc::try_unwrap(command)
					.map_err(|_| RuntimeError::HandlerLockHeld("pre-spawn"))?
					.into_inner();

				trace!("spawing supervisor for command");
				let sup = Supervisor::spawn(
					errors.clone(),
					events.clone(),
					&mut command,
					working.grouped,
				)?;

				debug!("running post-spawn handler");
				let post_spawn = PostSpawn {
					command: working.command.clone(),
					id: sup.id(),
					grouped: working.grouped,
				};
				post_spawn_handler
					.handle(post_spawn)
					.map_err(|e| rte("action post-spawn", e))?;

				// TODO: consider what we want to do for (previous) process if it's still running here?
				*process = Some(sup);
			}
		}

		(Some(p), Outcome::Signal(sig)) => {
			#[cfg(unix)]
			if let Some(sig) = sig.to_nix() {
				p.signal(sig).await;
			}

			#[cfg(windows)]
			if let SubSignal::Terminate = sig {
				p.kill().await;
			}
			// else: https://github.com/watchexec/watchexec/issues/219
		}

		(Some(p), Outcome::Wait) => {
			p.wait().await?;
		}

		(_, Outcome::Clear) => {
			clearscreen::clear()?;
		}

		(_, Outcome::Reset) => {
			for cs in [
				ClearScreen::WindowsCooked,
				ClearScreen::WindowsVt,
				ClearScreen::VtLeaveAlt,
				ClearScreen::VtWellDone,
				ClearScreen::default(),
			] {
				cs.clear()?;
			}
		}

		(Some(_), Outcome::IfRunning(then, _)) => {
			apply_outcome(
				*then,
				working,
				process,
				pre_spawn_handler,
				post_spawn_handler,
				errors,
				events,
			)
			.await?;
		}
		(None, Outcome::IfRunning(_, otherwise)) => {
			apply_outcome(
				*otherwise,
				working,
				process,
				pre_spawn_handler,
				post_spawn_handler,
				errors,
				events,
			)
			.await?;
		}

		(_, Outcome::Both(one, two)) => {
			if let Err(err) = apply_outcome(
				*one,
				working.clone(),
				process,
				pre_spawn_handler,
				post_spawn_handler,
				errors.clone(),
				events.clone(),
			)
			.await
			{
				debug!(
					"first outcome failed, sending an error but proceeding to the second anyway"
				);
				errors.send(err).await.ok();
			}

			apply_outcome(
				*two,
				working,
				process,
				pre_spawn_handler,
				post_spawn_handler,
				errors,
				events,
			)
			.await?;
		}
	}

	Ok(())
}
