#!/usr/bin/env bash
# Run inside a worktree: merge origin/<base> into the current branch, where <base> is the branch it
# is stacked on (branch.<branch>.updateBase) while that branch is still open, else main.
# On a conflict nothing is changed; the merge command to run by hand is printed.
set -euo pipefail
branch=$(git branch --show-current)
git fetch -q origin
base=$(git config "branch.$branch.updateBase" || echo main)
if [ "$base" != main ] && ! git show-ref -q "refs/remotes/origin/$base"; then
  echo "stacked base $base is gone (merged?) → following main"
  git config "branch.$branch.updateBase" main
  base=main
fi
if ! git merge-tree --write-tree HEAD "origin/$base" >/dev/null 2>&1; then
  echo "✗ conflicts with origin/$base; nothing changed. Resolve by hand: git merge origin/$base" >&2
  exit 1
fi
git merge -q --no-edit "origin/$base"
echo "✓ $branch is up to date with origin/$base"
