//! Shared counters and the progress UI thread.

use std::io::IsTerminal;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use indicatif::{HumanCount, ProgressBar, ProgressDrawTarget, ProgressState, ProgressStyle};

const MAX_KEPT_ERRORS: usize = 1000;

#[derive(Default)]
pub struct Stats {
    found: AtomicU64,
    deleted: AtomicU64,
    /// Scan tasks queued or running; once it hits 0 the total is final.
    pub scan_pending: AtomicUsize,
    want_current: AtomicBool,
    current: Mutex<String>,
    error_count: AtomicU64,
    errors: Mutex<Vec<String>>,
}

impl Stats {
    pub fn add_found(&self, n: u64) {
        self.found.fetch_add(n, Ordering::Relaxed);
    }

    pub fn add_deleted(&self, n: u64) {
        self.deleted.fetch_add(n, Ordering::Relaxed);
    }

    pub fn deleted(&self) -> u64 {
        self.deleted.load(Ordering::Relaxed)
    }

    pub fn error_count(&self) -> u64 {
        self.error_count.load(Ordering::Relaxed)
    }

    pub fn errors(&self) -> Vec<String> {
        std::mem::take(&mut self.errors.lock().unwrap())
    }

    pub fn error(&self, path: &Path, err: std::io::Error) {
        self.error_count.fetch_add(1, Ordering::Relaxed);
        let mut errors = self.errors.lock().unwrap();
        if errors.len() < MAX_KEPT_ERRORS {
            errors.push(format!("{}: {err}", path.display()));
        }
    }

    /// True (once) when the UI wants a fresh "current file"; keeps the hot path lock-free.
    pub fn want_current(&self) -> bool {
        self.want_current.load(Ordering::Relaxed) && self.want_current.swap(false, Ordering::Relaxed)
    }

    pub fn set_current(&self, path: &Path) {
        *self.current.lock().unwrap() = path.display().to_string();
    }
}

pub struct Ui {
    done: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Ui {
    /// Start drawing to stderr; draws nothing when `quiet` or stderr isn't a terminal.
    pub fn start(stats: Arc<Stats>, quiet: bool) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        if quiet || !std::io::stderr().is_terminal() {
            return Self { done, handle: None };
        }
        let flag = Arc::clone(&done);
        let handle = thread::spawn(move || draw_loop(&stats, &flag));
        Self { done, handle: Some(handle) }
    }

    pub fn finish(mut self) {
        self.done.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }

    pub fn is_visible(&self) -> bool {
        self.handle.is_some()
    }
}

fn draw_loop(stats: &Stats, done: &AtomicBool) {
    let bar = ProgressBar::with_draw_target(None, ProgressDrawTarget::stderr_with_hz(15));
    bar.set_style(
        ProgressStyle::with_template(
            "{spinner:.cyan} Deleting… {human_pos} removed · {rate} · {prefix}\n  {msg:.dim}",
        )
        .unwrap()
        .with_key("rate", rate),
    );
    let mut counting = true;
    let mut stamp = Instant::now();

    while !done.load(Ordering::Acquire) {
        let found = stats.found.load(Ordering::Relaxed);
        let deleted = stats.deleted();

        if counting && stats.scan_pending.load(Ordering::Acquire) == 0 && found > 0 {
            counting = false;
            bar.set_length(found);
            bar.set_style(
                ProgressStyle::with_template(
                    "[{bar:30.cyan/blue}] {percent:>3}% · {human_pos}/{human_len} · {prefix} · {rate} · ETA {eta}\n  {msg:.dim}",
                )
                .unwrap()
                .with_key("rate", rate)
                .progress_chars("█▉▊▋▌▍▎▏ "),
            );
        }
        if counting {
            bar.set_prefix(format!("counting… {} found", HumanCount(found)));
        } else {
            bar.set_prefix(format!("{} left", HumanCount(found.saturating_sub(deleted))));
        }
        bar.set_position(deleted);

        if stamp.elapsed() >= Duration::from_millis(100) {
            stamp = Instant::now();
            let width = console_width().unwrap_or(80).saturating_sub(3).max(20);
            bar.set_message(truncate_middle(&stats.current.lock().unwrap(), width));
            stats.want_current.store(true, Ordering::Relaxed);
        }
        thread::sleep(Duration::from_millis(30));
    }
    bar.finish_and_clear();
}

fn rate(state: &ProgressState, w: &mut dyn std::fmt::Write) {
    let _ = write!(w, "{}/s", HumanCount(state.per_sec() as u64));
}

fn console_width() -> Option<usize> {
    let ws = rustix::termios::tcgetwinsize(std::io::stderr()).ok()?;
    (ws.ws_col > 0).then_some(ws.ws_col as usize)
}

/// Shorten `s` to `max` chars by replacing its middle with "…", keeping the file name visible.
pub fn truncate_middle(s: &str, max: usize) -> String {
    let len = s.chars().count();
    if len <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let tail = keep * 2 / 3;
    let head = keep - tail;
    let head_s: String = s.chars().take(head).collect();
    let tail_s: String = s.chars().skip(len - tail).collect();
    format!("{head_s}…{tail_s}")
}

#[cfg(test)]
mod tests {
    use super::truncate_middle;

    #[test]
    fn truncates_in_the_middle() {
        assert_eq!(truncate_middle("short", 10), "short");
        let t = truncate_middle("aaaaaaaaaa/bbbbbbbbbb/file.txt", 16);
        assert_eq!(t.chars().count(), 16);
        assert!(t.ends_with("file.txt"));
        assert!(t.contains('…'));
    }
}
