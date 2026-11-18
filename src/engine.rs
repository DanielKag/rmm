//! Parallel delete engine.
//!
//! A single walk both discovers and deletes. Reading a directory yields its
//! entries: files are queued as unlink batches, subdirectories as scan tasks.
//! "Scanner" workers prefer scan tasks (so the total becomes known quickly),
//! "deleter" workers prefer unlink tasks (so deletion starts immediately).
//! A directory is removed once its last child task finishes.

use std::ffi::{CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use crossbeam_deque::{Injector, Steal};
use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags};
use rustix::io::Errno;

use crate::progress::Stats;

type TryFn = fn(&Engine) -> bool;

/// Max file names per unlink task, so huge directories are split across workers.
const CHUNK: usize = 512;

struct DirNode {
    path: PathBuf,
    parent: Option<Arc<DirNode>>,
    /// Outstanding child tasks + 1 guard held by the scan itself.
    pending: AtomicUsize,
    /// A descendant could not be removed, so this directory can't be either.
    failed: AtomicBool,
}

/// An open directory shared by its unlink batches; closing it frees an fd slot.
struct OpenDir {
    dir: Dir,
    open: Arc<AtomicUsize>,
}

impl Drop for OpenDir {
    fn drop(&mut self) {
        self.open.fetch_sub(1, Ordering::AcqRel);
    }
}

struct Batch {
    node: Arc<DirNode>,
    dir: Arc<OpenDir>,
    names: Vec<CString>,
}

pub struct Engine {
    stats: Arc<Stats>,
    scan_q: Injector<Arc<DirNode>>,
    unlink_q: Injector<Batch>,
    /// Queued + running tasks; workers exit when this hits zero.
    outstanding: AtomicUsize,
    /// Directories kept open by queued unlink batches.
    open_dirs: Arc<AtomicUsize>,
    fd_budget: usize,
}

impl Engine {
    pub fn new(stats: Arc<Stats>) -> Self {
        Self {
            stats,
            scan_q: Injector::new(),
            unlink_q: Injector::new(),
            outstanding: AtomicUsize::new(0),
            open_dirs: Arc::new(AtomicUsize::new(0)),
            fd_budget: raise_fd_limit() / 2,
        }
    }

    /// Queue a top-level path given on the command line.
    pub fn add_root(&self, path: &Path) {
        let meta = match std::fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => return self.stats.error(path, e),
        };
        self.stats.add_found(1);
        if meta.is_dir() {
            self.push_scan(Arc::new(DirNode {
                path: path.to_path_buf(),
                parent: None,
                pending: AtomicUsize::new(1),
                failed: AtomicBool::new(false),
            }));
        } else {
            match std::fs::remove_file(path) {
                Ok(()) => self.stats.add_deleted(1),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.stats.add_deleted(1),
                Err(e) => self.stats.error(path, e),
            }
        }
    }

    /// Run `threads` workers until everything queued is done.
    pub fn run(self: &Arc<Self>, threads: usize) {
        let scanners = (threads / 4).max(1);
        let handles: Vec<_> = (0..threads)
            .map(|i| {
                let engine = Arc::clone(self);
                thread::spawn(move || engine.worker(i < scanners))
            })
            .collect();
        for h in handles {
            let _ = h.join();
        }
    }

    fn worker(&self, prefer_scan: bool) {
        let (first, second): (TryFn, TryFn) = if prefer_scan {
            (Self::try_scan, Self::try_unlink)
        } else {
            (Self::try_unlink, Self::try_scan)
        };
        let mut idle = 0u32;
        loop {
            let did_work = first(self) || second(self);
            if did_work {
                idle = 0;
                continue;
            }
            if self.outstanding.load(Ordering::Acquire) == 0 {
                return;
            }
            idle += 1;
            if idle < 64 {
                std::hint::spin_loop();
            } else if idle < 128 {
                thread::yield_now();
            } else {
                thread::sleep(Duration::from_micros(50));
            }
        }
    }

    fn try_scan(&self) -> bool {
        match steal(&self.scan_q) {
            Some(node) => {
                self.scan(node);
                self.stats.scan_pending.fetch_sub(1, Ordering::Release);
                self.outstanding.fetch_sub(1, Ordering::AcqRel);
                true
            }
            None => false,
        }
    }

    fn try_unlink(&self) -> bool {
        match steal(&self.unlink_q) {
            Some(batch) => {
                self.unlink(batch);
                self.outstanding.fetch_sub(1, Ordering::AcqRel);
                true
            }
            None => false,
        }
    }

    fn push_scan(&self, node: Arc<DirNode>) {
        self.outstanding.fetch_add(1, Ordering::AcqRel);
        self.stats.scan_pending.fetch_add(1, Ordering::AcqRel);
        self.scan_q.push(node);
    }

    fn scan(&self, node: Arc<DirNode>) {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let dir = match rustix::fs::openat(CWD, &node.path, flags, Mode::empty()).and_then(Dir::new) {
            Ok(d) => d,
            Err(Errno::NOENT) => return self.release(node),
            Err(e) => {
                self.stats.error(&node.path, e.into());
                node.failed.store(true, Ordering::Relaxed);
                return self.release(node);
            }
        };

        let mut files = Vec::new();
        let mut subdirs = Vec::new();
        let mut dir = dir;
        while let Some(entry) = dir.read() {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    self.stats.error(&node.path, e.into());
                    node.failed.store(true, Ordering::Relaxed);
                    break;
                }
            };
            let name = entry.file_name();
            let bytes = name.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            let is_dir = match entry.file_type() {
                FileType::Directory => true,
                FileType::Unknown => match rustix::fs::statat(dir.fd().unwrap(), name, AtFlags::SYMLINK_NOFOLLOW) {
                    Ok(st) => FileType::from_raw_mode(st.st_mode) == FileType::Directory,
                    Err(_) => false,
                },
                _ => false,
            };
            if is_dir {
                subdirs.push(node.path.join(OsStr::from_bytes(bytes)));
            } else {
                files.push(name.to_owned());
            }
        }
        self.stats.add_found((files.len() + subdirs.len()) as u64);

        let chunks = files.len().div_ceil(CHUNK);
        node.pending.fetch_add(subdirs.len() + chunks, Ordering::AcqRel);

        for path in subdirs {
            self.push_scan(Arc::new(DirNode {
                path,
                parent: Some(Arc::clone(&node)),
                pending: AtomicUsize::new(1),
                failed: AtomicBool::new(false),
            }));
        }

        if chunks > 0 {
            // Keep the fd open for queued batches only while under budget;
            // otherwise delete inline so open fds stay bounded.
            let inline = self.open_dirs.fetch_add(1, Ordering::AcqRel) >= self.fd_budget;
            let dir = Arc::new(OpenDir { dir, open: Arc::clone(&self.open_dirs) });
            let mut files = files;
            while !files.is_empty() {
                let rest = files.split_off(files.len().saturating_sub(CHUNK));
                let batch = Batch { node: Arc::clone(&node), dir: Arc::clone(&dir), names: rest };
                if inline {
                    self.unlink(batch);
                } else {
                    self.outstanding.fetch_add(1, Ordering::AcqRel);
                    self.unlink_q.push(batch);
                }
            }
        }

        self.release(node);
    }

    fn unlink(&self, batch: Batch) {
        let fd = batch.dir.dir.fd().expect("dirfd");
        for name in &batch.names {
            match rustix::fs::unlinkat(fd, name.as_c_str(), AtFlags::empty()) {
                Ok(()) | Err(Errno::NOENT) => {}
                Err(e) => {
                    self.stats.error(&batch.node.path.join(OsStr::from_bytes(name.to_bytes())), e.into());
                    batch.node.failed.store(true, Ordering::Relaxed);
                    continue;
                }
            }
            self.stats.add_deleted(1);
            if self.stats.want_current() {
                self.stats.set_current(&batch.node.path.join(OsStr::from_bytes(name.to_bytes())));
            }
        }
        // Close the directory (if this was its last batch) before removing it.
        drop(batch.dir);
        self.release(batch.node);
    }

    /// Drop one pending reference; remove directories whose work is complete,
    /// walking up the parent chain.
    fn release(&self, node: Arc<DirNode>) {
        let mut cur = Some(node);
        while let Some(node) = cur {
            if node.pending.fetch_sub(1, Ordering::AcqRel) != 1 {
                return;
            }
            let failed = node.failed.load(Ordering::Relaxed);
            if !failed {
                match rustix::fs::unlinkat(CWD, &node.path, AtFlags::REMOVEDIR) {
                    Ok(()) | Err(Errno::NOENT) => {
                        self.stats.add_deleted(1);
                        if self.stats.want_current() {
                            self.stats.set_current(&node.path);
                        }
                    }
                    Err(e) => {
                        self.stats.error(&node.path, e.into());
                        if let Some(p) = &node.parent {
                            p.failed.store(true, Ordering::Relaxed);
                        }
                    }
                }
            } else if let Some(p) = &node.parent {
                p.failed.store(true, Ordering::Relaxed);
            }
            cur = node.parent.clone();
        }
    }
}

fn steal<T>(q: &Injector<T>) -> Option<T> {
    loop {
        match q.steal() {
            Steal::Success(t) => return Some(t),
            Steal::Empty => return None,
            Steal::Retry => continue,
        }
    }
}

/// Raise the soft open-file limit as far as allowed; returns the resulting limit.
fn raise_fd_limit() -> usize {
    use rustix::process::{Resource, getrlimit, setrlimit};
    let mut lim = getrlimit(Resource::Nofile);
    let hard = lim.maximum.unwrap_or(u64::MAX);
    // macOS rejects values above OPEN_MAX for the soft limit.
    let target = if cfg!(target_os = "macos") { hard.min(10240) } else { hard };
    if lim.current.is_some_and(|c| c < target) {
        lim.current = Some(target);
        let _ = setrlimit(Resource::Nofile, lim);
    }
    getrlimit(Resource::Nofile).current.unwrap_or(1024).min(1 << 20) as usize
}
