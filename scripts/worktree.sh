#!/usr/bin/env bash
# scripts/worktree.sh <name> <branch> [base]      (make worktree name=… branch=… [base=origin/…])
#
# Creates worktrees/<number>--<name> (gitignored) on <branch>:
#   - an existing local branch, or a local branch tracking origin/<branch>: checked out as is;
#   - otherwise a new branch from [base] (default origin/main).
# The branch's stacked base is recorded in `git config branch.<branch>.updateBase` (used by
# `make update`): [base] when given, else the base of the branch's open PR, else nothing (main).
# <number> is the branch's PR if it has one, otherwise the next free issue/PR number (a guess
# until the PR exists).
set -euo pipefail
[ $# -ge 2 ] || { echo "usage: scripts/worktree.sh <name> <branch> [origin/<base>]" >&2; exit 2; }
name=$1 branch=$2 base=${3:-}
if [ -n "$base" ] && [[ $base != origin/* ]]; then
  echo "✗ base must be a remote branch like origin/feat/x (got: $base)" >&2; exit 2
fi
root="$(cd "$(git rev-parse --path-format=absolute --git-common-dir)/.." && pwd)"
cd "$root"
mkdir -p worktrees
git fetch -q --prune origin

number=$(gh pr list --head "$branch" --state all --json number --jq '.[0].number // empty')
if [ -z "$number" ]; then
  last=$(gh api "repos/{owner}/{repo}/issues?state=all&per_page=1&sort=created&direction=desc" --jq '.[0].number // 0')
  local_max=0
  for d in worktrees/*--*; do
    [ -d "$d" ] || continue
    n=${d#worktrees/}; n=${n%%--*}
    [[ $n =~ ^[0-9]+$ ]] && [ "$n" -gt "$local_max" ] && local_max=$n
  done
  number=$(( (last > local_max ? last : local_max) + 1 ))
fi
dir="worktrees/$number--$name"

if git show-ref --verify -q "refs/heads/$branch"; then
  git worktree add -q "$dir" "$branch"
elif git show-ref --verify -q "refs/remotes/origin/$branch"; then
  git worktree add -q --track -b "$branch" "$dir" "origin/$branch"
else
  git worktree add -q --no-track -b "$branch" "$dir" "${base:-origin/main}"
fi

[ -n "$base" ] || base="origin/$(gh pr list --head "$branch" --state open --json baseRefName --jq '.[0].baseRefName // "main"')"
git config "branch.$branch.updateBase" "${base#origin/}"
echo "✓ $dir on $branch (updates from ${base})"
