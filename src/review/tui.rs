use std::io::{self, IsTerminal};
use std::time::Duration;

use crossterm::event::{self, Event};
use eyre::Context;

use super::{
    ReviewSession,
    app::{App, Decision},
    discovery::ReviewQueue,
    input::{self, Action},
    queue::QueueState,
    sources::BaselineLoader,
    ui,
};

pub fn run(session: ReviewSession) -> eyre::Result<()> {
    eyre::ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "interactive review requires a terminal on stdin and stdout"
    );
    // Read sources before changing terminal modes, so file errors are ordinary
    // CLI diagnostics. The guard also covers partially successful initialization.
    let mut app = App::new(session)?;
    let mut loader = BaselineLoader::start(&app.session)?;
    let _restore = RestoreTerminal;
    // Ratatui installs a panic hook which restores before printing the panic.
    let mut terminal = ratatui::try_init().context("could not initialize review terminal")?;
    let mut dirty = true;
    loop {
        if let Some(baseline) = loader.as_ref().and_then(BaselineLoader::poll) {
            app.set_baseline(baseline)?;
            loader = None;
            dirty = true;
        }
        if dirty {
            terminal
                .draw(|frame| ui::render(frame, &mut app))
                .context("could not render review")?;
            dirty = false;
        }
        if !event::poll(Duration::from_millis(100)).context("could not poll terminal events")? {
            continue;
        }
        let event = event::read().context("could not read terminal event")?;
        if matches!(event, Event::Resize(..)) {
            dirty = true;
        }
        if let Event::Key(key) = event {
            dirty = true;
            let decision = match input::handle_key(&mut app, key) {
                Action::Quit => break,
                Action::Approve => Decision::Approve,
                Action::Reject => Decision::Reject,
                Action::Continue | Action::NextPackage | Action::PreviousPackage | Action::Skip => {
                    continue;
                }
            };
            match app.decide(decision) {
                Ok(()) => break,
                Err(error) => app.report_decision_error(error),
            }
        }
    }
    Ok(())
}

pub fn run_queue(queue: ReviewQueue) -> eyre::Result<()> {
    if queue.packages.is_empty() {
        println!("No build scripts found in the selected dependency graph.");
        return Ok(());
    }
    eyre::ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "interactive review requires a terminal on stdin and stdout"
    );
    let mut state = QueueState::new(queue);
    {
        let _restore = RestoreTerminal;
        let mut terminal = ratatui::try_init().context("could not initialize review terminal")?;
        let mut dirty = true;
        loop {
            dirty |= state.poll();
            if dirty {
                terminal
                    .draw(|frame| ui::render_queue(frame, &mut state))
                    .context("could not render review queue")?;
                dirty = false;
            }
            if !event::poll(Duration::from_millis(100)).context("could not poll terminal events")? {
                continue;
            }
            let event = event::read().context("could not read terminal event")?;
            if matches!(event, Event::Resize(..)) {
                dirty = true;
            }
            let Event::Key(key) = event else {
                continue;
            };
            dirty = true;
            match state.handle_key(key) {
                Action::Quit => break,
                Action::NextPackage => state.navigate(false),
                Action::PreviousPackage => state.navigate(true),
                Action::Skip => {
                    if state.skip() {
                        break;
                    }
                }
                action @ (Action::Approve | Action::Reject) => {
                    let decision = if action == Action::Approve {
                        Decision::Approve
                    } else {
                        Decision::Reject
                    };
                    match state.decide(decision) {
                        Ok(true) => break,
                        Ok(false) => {}
                        Err(error) => {
                            if let Some(app) = &mut state.app {
                                app.report_decision_error(error);
                            }
                        }
                    }
                }
                Action::Continue => {}
            }
        }
    }
    state.summary().print();
    Ok(())
}

struct RestoreTerminal;

impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        ratatui::restore();
    }
}
