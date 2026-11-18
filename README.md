# rmm

Fast parallel `rm -rf` with a live progress bar.

Deleting a big `node_modules` or build directory with `rm -rf` walks the tree on a
single thread. `rmm` discovers and deletes in parallel, shows how far along it is,
and prints a summary when done.

```
$ rmm node_modules
Removed 200,000 items in 5.55s (36,036/s)
```

On a MacBook (APFS), removing a 200k-file tree took **5.6s with `rmm`** vs **12.1s with `rm -rf`**.

## Install

### Homebrew

```sh
brew install DanielKag/rmm/rmm
```

### Cargo

```sh
cargo install --git https://github.com/DanielKag/rmm
```

## Usage

```
rmm [OPTIONS] <PATHS>...

  -r, -R, --recursive     Accepted for `rm` compatibility (always recursive)
  -f, --force             Accepted for `rm` compatibility (missing paths are ignored, never prompts)
  -q, --quiet             No progress bar or summary; errors are still printed
  -j, --jobs <JOBS>       Worker threads (default: 4 on macOS, else number of CPUs)
      --no-preserve-root  Allow removing `/` or your home directory
```

`rmm -rf dir` works, so you can alias or swap it in for `rm -rf`.

## Behavior

- Always recursive; missing paths are silently ignored.
- Symlinks are removed, never followed.
- Refuses `.`, `..`, `/` and your home directory (the latter two unless `--no-preserve-root`).
- If something can't be removed (e.g. permission denied), everything else is still
  deleted, errors are printed, and the exit code is non-zero.
- The progress bar only shows when stderr is a terminal.

## How it works

A single walk both discovers and deletes. Reading a directory queues its files as
unlink batches and its subdirectories as scan tasks. Some workers prefer scanning
(so the total is known quickly) and the rest prefer unlinking (so deletion starts
immediately). A directory is removed as soon as its last child is gone.

## License

MIT
