#!/usr/bin/env bash
# scripts/worktree.sh <name> <branch> [base]
#
# Creates worktrees/<number>--<name> (gitignored) on <branch>: the existing local branch, a local
# branch tracking origin/<branch>, or a new branch from [base] (default origin/main). A new branch
# records its base in `git config branch.<branch>.updateBase` so `make update` merges the right
# thing for stacked PRs. <number> is the branch's PR if it has one, otherwise the next free
# issue/PR number (a guess until the PR exists).
set -euo pipefail
name=$1 branch=$2 base=${3:-origin/main}
root="$(cd "$(git rev-parse --path-format=absolute --git-common-dir)/.." && pwd)"
cd "$root"
git fetch -q origin

number=$(gh pr list --head "$branch" --state all --json number --jq '.[0].number // empty')
if [ -z "$number" ]; then
  last=$(gh api "repos/{owner}/{repo}/issues?state=all&per_page=1&sort=created&direction=desc" --jq '.[0].number // 0')
  local_max=$(ls worktrees 2>/dev/null | sed -n 's/^\([0-9]*\)--.*/\1/p' | sort -n | tail -1)
  number=$(( (last > ${local_max:-0} ? last : ${local_max:-0}) + 1 ))
fi
dir="worktrees/$number--$name"

if git show-ref -q "refs/heads/$branch"; then
  git worktree add -q "$dir" "$branch"
elif git show-ref -q "refs/remotes/origin/$branch"; then
  git worktree add -q --track -b "$branch" "$dir" "origin/$branch"
else
  git worktree add -q --no-track -b "$branch" "$dir" "$base"
  case "$base" in origin/*) git config "branch.$branch.updateBase" "${base#origin/}" ;; esac
fi
echo "✓ $dir on $branch"
