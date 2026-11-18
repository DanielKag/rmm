#!/usr/bin/env bash
# Create a node_modules-like test tree: mktree.sh DIR [DIRS] [FILES_PER_DIR]
# Default: 2000 dirs x 100 files = 200k files.
set -euo pipefail
root=${1:?usage: mktree.sh DIR [DIRS] [FILES_PER_DIR]}
dirs=${2:-2000}
files=${3:-100}
rm -rf "$root"
mkdir -p "$root"
cd "$root"
for ((d = 0; d < dirs; d++)); do
  p="pkg$((d % 50))/sub$((d / 50 % 20))/d$d"
  mkdir -p "$p"
  (cd "$p" && seq -f "f%g.js" 1 "$files" | xargs touch)
done
