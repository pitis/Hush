#!/usr/bin/env bash
# Run inside a worktree: merge origin/<base> into the current branch. <base> is the branch this one
# is stacked on (branch.<branch>.updateBase) until that branch's PR is merged or its branch is
# gone, then main. On a conflict no merge is made; the command to run by hand is printed.
set -euo pipefail
branch=$(git branch --show-current)
[ -n "$branch" ] || { echo "✗ detached HEAD: check out a branch first" >&2; exit 1; }
git fetch -q --prune origin
base=$(git config --default main --get "branch.$branch.updateBase")
if [ "$base" != main ]; then
  merged=$(gh pr list --head "$base" --state merged --json number --jq 'length')
  if [ "$merged" != 0 ] || ! git show-ref --verify -q "refs/remotes/origin/$base"; then
    echo "stacked base $base is merged or gone → following main"
    git config "branch.$branch.updateBase" main
    base=main
  fi
fi
set +e
git merge-tree --write-tree HEAD "origin/$base" >/dev/null
rc=$?
set -e
case $rc in
  0) ;;
  1) echo "✗ conflicts with origin/$base; no merge made. Resolve by hand: git merge origin/$base" >&2; exit 1 ;;
  *) echo "✗ git merge-tree failed (exit $rc); see the error above" >&2; exit "$rc" ;;
esac
git merge -q --no-edit "origin/$base"
echo "✓ $branch is up to date with origin/$base"
