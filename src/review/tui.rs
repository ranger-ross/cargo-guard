use std::{
    io::{self, IsTerminal},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crossterm::{
    event::{self, Event},
    terminal::{EnterAlternateScreen, enable_raw_mode},
};
use eyre::Context;

use super::{discovery::ReviewQueue, input::Action, queue::QueueState, ui};

pub fn run(queue: ReviewQueue) -> eyre::Result<()> {
    run_review(queue, false)
}

pub fn run_queue(queue: ReviewQueue) -> eyre::Result<()> {
    run_review(queue, true)
}

fn run_review(queue: ReviewQueue, queue_mode: bool) -> eyre::Result<()> {
    if queue.packages.is_empty() {
        println!("No dependency build scripts to review in the selected dependency graph.");
        return Ok(());
    }
    eyre::ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "interactive review requires a terminal on stdin and stdout"
    );
    let mut state = QueueState::new(queue);
    set_mode(&mut state, queue_mode)?;
    {
        // Initialize without ratatui's restoring panic hook: unwinding must
        // stop and join the event reader before restoring terminal modes.
        // This guard also covers partially successful initialization.
        let _restore = RestoreTerminal;
        enable_raw_mode().context("could not enable review terminal raw mode")?;
        crossterm::execute!(io::stdout(), EnterAlternateScreen)
            .context("could not enter review alternate screen")?;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(io::stdout()))
                .context("could not initialize review terminal")?;
        // Declared last, so all exits join the reader before terminal cleanup.
        let events = TerminalEvents::start()?;
        let mut dirty = true;
        'review: loop {
            dirty |= state.poll();
            set_mode(&mut state, queue_mode)?;
            while let Some(event) = events.poll()? {
                match event {
                    Event::Resize(..) => dirty = true,
                    Event::Key(key) => {
                        dirty = true;
                        match state.handle_key(key) {
                            Action::Quit => break 'review,
                            Action::NextPackage => state.navigate(false),
                            Action::PreviousPackage => state.navigate(true),
                            Action::Skip => {
                                if state.skip() {
                                    break 'review;
                                }
                            }
                            Action::Decide(decision) => match state.decide(decision) {
                                Ok(true) => break 'review,
                                Ok(false) => {}
                                Err(error) => {
                                    if let Some(app) = &mut state.app {
                                        app.report_decision_error(error);
                                    }
                                }
                            },
                            Action::Continue => {}
                        }
                        set_mode(&mut state, queue_mode)?;
                    }
                    _ => {}
                }
            }
            if dirty {
                terminal
                    .draw(|frame| {
                        if queue_mode {
                            ui::render_queue(frame, &mut state);
                        } else if let Some(app) = &mut state.app {
                            ui::render(frame, app);
                        }
                    })
                    .context("could not render review")?;
                dirty = false;
            }
            // Events and background results unpark this thread immediately.
            // The timeout is only for maintenance and external-change checks.
            thread::park_timeout(Duration::from_secs(1));
        }
    }
    if queue_mode {
        state.summary().print();
    }
    Ok(())
}

fn set_mode(state: &mut QueueState, queue_mode: bool) -> eyre::Result<()> {
    if let Some(app) = &mut state.app {
        app.queue_mode = queue_mode;
    } else if !queue_mode {
        eyre::bail!(
            "{}",
            state.entries[state.selected]
                .error
                .as_deref()
                .unwrap_or("could not load reviewed package")
        );
    }
    Ok(())
}

struct TerminalEvents {
    receiver: mpsc::Receiver<eyre::Result<Event>>,
    cancelled: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl TerminalEvents {
    fn start() -> eyre::Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let owner = thread::current();
        let worker = thread::Builder::new()
            .name("review-events".into())
            .spawn(move || {
                while !worker_cancelled.load(Ordering::Relaxed) {
                    // Only cancellation uses this bounded poll timeout. Input
                    // is delivered as soon as available, independently of UI
                    // maintenance and background verification completion.
                    let result = match event::poll(Duration::from_millis(100))
                        .context("could not poll terminal events")
                    {
                        Ok(false) => continue,
                        Ok(true) => {
                            if worker_cancelled.load(Ordering::Relaxed) {
                                break;
                            }
                            event::read().context("could not read terminal event")
                        }
                        Err(error) => Err(error),
                    };
                    let failed = result.is_err();
                    if sender.send(result).is_err() {
                        break;
                    }
                    owner.unpark();
                    if failed {
                        break;
                    }
                }
            })
            .context("could not start terminal event reader")?;
        Ok(Self {
            receiver,
            cancelled,
            worker: Some(worker),
        })
    }

    fn poll(&self) -> eyre::Result<Option<Event>> {
        match self.receiver.try_recv() {
            Ok(result) => result.map(Some),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => {
                eyre::bail!("terminal event reader stopped unexpectedly")
            }
        }
    }
}

impl Drop for TerminalEvents {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct RestoreTerminal;

impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_reader_is_cancelled_and_joined_before_drop_returns() {
        let (_sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let finished = Arc::new(AtomicBool::new(false));
        let worker_finished = finished.clone();
        let worker = thread::spawn(move || {
            while !worker_cancelled.load(Ordering::Relaxed) {
                thread::yield_now();
            }
            worker_finished.store(true, Ordering::Relaxed);
        });
        drop(TerminalEvents {
            receiver,
            cancelled: cancelled.clone(),
            worker: Some(worker),
        });
        assert!(cancelled.load(Ordering::Relaxed));
        assert!(finished.load(Ordering::Relaxed));
    }
}
