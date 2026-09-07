use std::{path::Path, sync::mpsc, time::Duration};

use app::App;
use args::{Args, Command};
use clap::CommandFactory;
use clap_complete::generate;
use crossterm::{
    event::{self, Event},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use eyre::{Context, Result};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use poll::Poller;
use ui::input::{Action, handle_key};

use crate::util::cpu_count;

mod app;
mod args;
mod headless;
mod poll;
mod scan;
mod trace;
mod ui;
mod util;

fn main() -> Result<()> {
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(cpu_count())
        .build_global();
    let _trace_guard = trace::init();
    let args = Args::parse_args();
    // TUI is the default when no subcommand is given.
    match &args.command {
        None => {
            let host = Args::root_for(args.root.clone(), None);
            let roots = scan::resolve_scan_roots(&host, args.docker)?;
            check_root(&roots[0].path)?;
            let result = tui(&roots);
            if let Some(guard) = _trace_guard.as_ref() {
                eprintln!("Trace written to {}", guard.path.display());
            }
            return result;
        }
        Some(Command::Tui { root, docker }) => {
            let host = Args::root_for(root.clone(), args.root.clone());
            let roots = scan::resolve_scan_roots(&host, Args::docker_for(*docker, args.docker))?;
            check_root(&roots[0].path)?;
            let result = tui(&roots);
            if let Some(guard) = _trace_guard.as_ref() {
                eprintln!("Trace written to {}", guard.path.display());
            }
            return result;
        }
        Some(Command::List { root, docker }) => {
            let host = Args::root_for(root.clone(), args.root.clone());
            let roots = scan::resolve_scan_roots(&host, Args::docker_for(*docker, args.docker))?;
            check_root(&roots[0].path)?;
            let result = headless::run_list(&roots);
            if let Some(guard) = _trace_guard.as_ref() {
                eprintln!("Trace written to {}", guard.path.display());
            }
            return result;
        }
        Some(Command::Clean {
            root,
            docker,
            older_than,
            larger_than,
            yes,
        }) => {
            let host = Args::root_for(root.clone(), args.root.clone());
            let roots = scan::resolve_scan_roots(&host, Args::docker_for(*docker, args.docker))?;
            check_root(&roots[0].path)?;
            let result =
                headless::run_clean(&roots, older_than.as_deref(), larger_than.as_deref(), *yes);
            if let Some(guard) = _trace_guard.as_ref() {
                eprintln!("Trace written to {}", guard.path.display());
            }
            return result;
        }
        Some(Command::Completions { shell }) => {
            let mut cmd = Args::command();
            generate(*shell, &mut cmd, "cargo-storage", &mut std::io::stdout());
            return Ok(());
        }
    }
}

fn tui(roots: &[scan::ScanRoot]) -> Result<()> {
    enable_raw_mode().wrap_err("enabling terminal raw mode")?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).wrap_err("entering alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).wrap_err("creating terminal")?;

    let result = run(&mut terminal, roots);

    disable_raw_mode().wrap_err("disabling terminal raw mode")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).wrap_err("leaving alternate screen")?;
    terminal.show_cursor().wrap_err("restoring cursor")?;

    result
}

fn check_root(root: &Path) -> Result<()> {
    if !root.is_dir() {
        eyre::bail!("scan root is not a directory: {}", root.display());
    }
    Ok(())
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    roots: &[scan::ScanRoot],
) -> Result<()> {
    let roots: Vec<scan::ScanRoot> = roots.to_vec();
    let mut app = App::new(roots[0].path.clone());
    let mut scan_rx = spawn_scan(&roots);
    let mut poller = Poller::new();

    loop {
        let _frame = tracing::info_span!("frame").entered();
        terminal
            .draw(|frame| ui::render(frame, &mut app))
            .wrap_err("rendering frame")?;

        // Drain scan progress without blocking the UI. Discovery shows
        // rows at once; measurements fill sizes as they finish.
        if let Some(rx) = scan_rx.as_ref() {
            let mut done = false;
            while let Ok(event) = rx.try_recv() {
                match event {
                    scan::ScanEvent::Discovered(projects) => {
                        app.set_discovered(projects);
                    }
                    scan::ScanEvent::Measured(m) => {
                        app.apply_measurements(&[m]);
                    }
                    scan::ScanEvent::Done { build_cache } => {
                        app.finish_scan(build_cache);
                        poller.reset(&app);
                        done = true;
                    }
                }
            }
            if done {
                scan_rx = None;
            }
        }

        poller.poll(&mut app);
        // 60fps while loading, 10fps otherwise.
        let frame_budget = if app.scanning {
            Duration::from_millis(16)
        } else {
            Duration::from_millis(100)
        };
        if event::poll(frame_budget).wrap_err("polling terminal events")? {
            match event::read().wrap_err("reading terminal event")? {
                Event::Key(key) => match handle_key(&mut app, key) {
                    Action::Continue => {}
                    Action::Quit => return Ok(()),
                    Action::Rescan => {
                        app.begin_scan();
                        scan_rx = spawn_scan(&roots);
                    }
                },
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    }
}

/// Run discovery plus the size walk off the UI thread. Discovery ships
/// first so rows appear at once; sizes stream in after.
fn spawn_scan(roots: &[scan::ScanRoot]) -> Option<mpsc::Receiver<scan::ScanEvent>> {
    let roots = roots.to_vec();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        scan::scan_stream_roots(&roots, tx);
    });
    Some(rx)
}
