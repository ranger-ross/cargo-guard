use std::io::{self, IsTerminal};
use std::time::Duration;

use crossterm::event::{self, Event};
use eyre::Context;

use super::{
    ReviewSession,
    app::App,
    input::{self, Action},
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
    loop {
        if let Some(baseline) = loader.as_ref().and_then(BaselineLoader::poll) {
            app.set_baseline(baseline)?;
            loader = None;
        }
        terminal
            .draw(|frame| ui::render(frame, &mut app))
            .context("could not render review")?;
        if !event::poll(Duration::from_millis(100)).context("could not poll terminal events")? {
            continue;
        }
        match event::read().context("could not read terminal event")? {
            Event::Key(key) if input::handle_key(&mut app, key) == Action::Quit => break,
            _ => {}
        }
    }
    Ok(())
}

struct RestoreTerminal;

impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        ratatui::restore();
    }
}
