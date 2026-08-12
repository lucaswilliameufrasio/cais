use std::io::{self, stdout};
use std::time::Duration;

use anyhow::{Context, Result};
use cais::app::App;
use cais::ui;
use crossterm::{
    event::{self, Event},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 8080;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.first().is_some_and(|arg| arg == "serve") {
        return serve_from_args(&args[1..]);
    }
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_help();
        return Ok(());
    }
    if !args.is_empty() {
        print_help();
        return Ok(());
    }

    run_tui()
}

fn serve_from_args(args: &[String]) -> Result<()> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_help();
        return Ok(());
    }

    let mut host = DEFAULT_HOST.to_owned();
    let mut port = DEFAULT_PORT;
    let mut open_browser = true;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--host" => {
                host = iter.next().context("--host requires a value")?.to_owned();
            }
            "--port" => {
                port = iter
                    .next()
                    .context("--port requires a value")?
                    .parse()
                    .context("--port must be a number")?;
            }
            "--no-browser" => open_browser = false,
            other => anyhow::bail!("unknown serve argument '{other}'"),
        }
    }

    cais::web::serve(&host, port, open_browser)
}

fn print_help() {
    eprintln!(
        "cais — PostgreSQL database management\n\
         \n\
         USAGE:\n\
         \x20 cais                 Run the TUI\n\
         \x20 cais serve [OPTIONS]  Run the local web interface\n\
         \n\
         serve OPTIONS:\n\
         \x20 --host <HOST>       Bind address (default: 127.0.0.1)\n\
         \x20 --port <PORT>       Bind port (default: 8080)\n\
         \x20 --no-browser        Do not auto-open the browser"
    );
}

fn run_tui() -> Result<()> {
    let mut terminal = setup_terminal()?;
    let result = run(&mut terminal);
    restore_terminal(&mut terminal)?;
    result
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    let mut app = App::new()?;
    loop {
        if let Err(error) = app.poll_background_tasks() {
            app.set_status(format!("Error: {error:#}"));
        }

        terminal.draw(|frame| {
            let _ = ui::draw(frame, &mut app);
        })?;

        if app.should_quit {
            break;
        }

        if event::poll(Duration::from_millis(200))?
            && let Event::Key(key) = event::read()?
            && let Err(error) = cais::app::handle_key_event(&mut app, key)
        {
            app.set_status(format!("Error: {error:#}"));
        }
    }
    Ok(())
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}
