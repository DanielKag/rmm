mod engine;
mod progress;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use indicatif::HumanCount;

use engine::Engine;
use progress::{Stats, Ui};

/// Fast parallel `rm -rf` with a live progress bar.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Files or directories to remove.
    #[arg(required = true)]
    paths: Vec<PathBuf>,

    /// Accepted for `rm` compatibility (always recursive).
    #[arg(short = 'r', short_alias = 'R', long)]
    recursive: bool,

    /// Accepted for `rm` compatibility (missing paths are always ignored, never prompts).
    #[arg(short, long)]
    force: bool,

    /// No progress bar or summary; errors are still printed.
    #[arg(short, long)]
    quiet: bool,

    /// Worker threads (default: 4 on macOS, where APFS slows down beyond that; else number of CPUs).
    #[arg(short, long)]
    jobs: Option<usize>,

    /// Allow removing `/` or your home directory.
    #[arg(long)]
    no_preserve_root: bool,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let _ = (args.recursive, args.force);

    for p in &args.paths {
        if let Err(msg) = check_path(p, args.no_preserve_root) {
            eprintln!("rmm: {msg}");
            return ExitCode::from(2);
        }
    }

    let start = Instant::now();
    let stats = Arc::new(Stats::default());
    let engine = Arc::new(Engine::new(Arc::clone(&stats)));
    for p in &args.paths {
        engine.add_root(p);
    }

    let ui = Ui::start(Arc::clone(&stats), args.quiet);
    let visible = ui.is_visible();
    let threads = args
        .jobs
        .unwrap_or_else(default_threads)
        .max(1);
    engine.run(threads);
    ui.finish();

    let errors = stats.errors();
    for e in &errors {
        eprintln!("rmm: {e}");
    }
    let error_count = stats.error_count();
    if error_count > errors.len() as u64 {
        eprintln!("rmm: … and {} more errors", error_count - errors.len() as u64);
    }

    if visible || (!args.quiet && std::io::stderr().is_terminal()) {
        let elapsed = start.elapsed();
        let deleted = stats.deleted();
        let rate = deleted as f64 / elapsed.as_secs_f64().max(1e-9);
        eprintln!(
            "Removed {} items in {:.2}s ({}/s){}",
            HumanCount(deleted),
            elapsed.as_secs_f64(),
            HumanCount(rate as u64),
            if error_count > 0 { format!(", {} errors", HumanCount(error_count)) } else { String::new() },
        );
    }

    if error_count > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

fn default_threads() -> usize {
    let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
    // Measured: APFS unlink throughput peaks around 4 threads and degrades past it.
    if cfg!(target_os = "macos") { cpus.min(4) } else { cpus }
}

/// Refuse the same dangerous targets as `rm`, plus `$HOME`.
fn check_path(p: &Path, no_preserve_root: bool) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    let raw = p.as_os_str().as_bytes();
    let last = raw.rsplit(|&b| b == b'/').find(|s| !s.is_empty());
    if matches!(last, Some(b"." | b"..")) {
        return Err(format!("refusing to remove '.' or '..' directory: {}", p.display()));
    }
    if no_preserve_root {
        return Ok(());
    }
    let Ok(canon) = std::fs::canonicalize(p) else { return Ok(()) };
    if canon == Path::new("/") {
        return Err(format!("it is dangerous to operate recursively on '{}' (use --no-preserve-root)", p.display()));
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && std::fs::canonicalize(&home).is_ok_and(|h| h == canon)
    {
        return Err(format!("refusing to remove your home directory '{}' (use --no-preserve-root)", p.display()));
    }
    Ok(())
}
