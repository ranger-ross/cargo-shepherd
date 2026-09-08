use std::{
    path::{Path, PathBuf},
    sync::mpsc,
    time::Duration,
};

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
use scan::resolve_root;
use ui::input::{Action, handle_key};

use crate::util::rayon_threads;

mod app;
mod args;
mod config;
mod headless;
mod poll;
mod scan;
mod trace;
mod ui;
mod util;

fn main() -> Result<()> {
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(rayon_threads())
        .build_global();
    let _trace_guard = trace::init();
    let args = Args::parse_args();
    // TUI is the default when no subcommand is given.
    let root = match &args.command {
        None => Args::root_for(args.root.clone(), None),
        Some(Command::Tui { root }) => Args::root_for(root.clone(), args.root.clone()),
        Some(Command::List { root }) => {
            let root = resolve_root(&Args::root_for(root.clone(), args.root.clone()));
            check_root(&root)?;
            let result = headless::run_list(&root);
            if let Some(guard) = _trace_guard.as_ref() {
                eprintln!("Trace written to {}", guard.path.display());
            }
            return result;
        }
        Some(Command::Clean {
            root,
            older_than,
            larger_than,
            yes,
        }) => {
            let root = resolve_root(&Args::root_for(root.clone(), args.root.clone()));
            check_root(&root)?;
            let result =
                headless::run_clean(&root, older_than.as_deref(), larger_than.as_deref(), *yes);
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
    };
    let root = resolve_root(&root);
    check_root(&root)?;

    enable_raw_mode().wrap_err("enabling terminal raw mode")?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).wrap_err("entering alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).wrap_err("creating terminal")?;

    let result = run(&mut terminal, root);

    disable_raw_mode().wrap_err("disabling terminal raw mode")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).wrap_err("leaving alternate screen")?;
    terminal.show_cursor().wrap_err("restoring cursor")?;
    if let Some(guard) = _trace_guard.as_ref() {
        eprintln!("Trace written to {}", guard.path.display());
    }

    result
}

fn check_root(root: &Path) -> Result<()> {
    if !root.is_dir() {
        eyre::bail!("scan root is not a directory: {}", root.display());
    }
    Ok(())
}

fn run(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>, root: PathBuf) -> Result<()> {
    let mut app = App::new(root.clone());
    let mut scan_rx = spawn_scan(&root);
    let mut poller = Poller::new();
    let delete_worker = DeleteWorker::new();

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

        for done in delete_worker.drain() {
            app.finish_delete(done.target_dir, done.result);
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
                    Action::Delete => {
                        if let Some(target_dir) = app.begin_delete() {
                            delete_worker.enqueue(target_dir);
                        }
                    }
                    Action::Quit => return Ok(()),
                    Action::Rescan => {
                        app.begin_scan();
                        scan_rx = spawn_scan(&app.root);
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
fn spawn_scan(root: &Path) -> Option<mpsc::Receiver<scan::ScanEvent>> {
    let root = root.to_path_buf();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        scan::scan_stream(&root, tx);
    });
    Some(rx)
}

struct DeleteDone {
    target_dir: PathBuf,
    result: std::io::Result<()>,
}

struct DeleteWorker {
    tx: mpsc::Sender<PathBuf>,
    rx: mpsc::Receiver<DeleteDone>,
}

impl DeleteWorker {
    fn new() -> Self {
        let (tx, requests) = mpsc::channel();
        let (results, rx) = mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(target_dir) = requests.recv() {
                let result = std::fs::remove_dir_all(&target_dir);
                let _ = results.send(DeleteDone { target_dir, result });
            }
        });
        Self { tx, rx }
    }

    fn enqueue(&self, target_dir: PathBuf) {
        let _ = self.tx.send(target_dir);
    }

    fn drain(&self) -> impl Iterator<Item = DeleteDone> + '_ {
        std::iter::from_fn(|| self.rx.try_recv().ok())
    }
}
