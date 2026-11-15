#!/usr/bin/env bash
# scripts/publish.sh [version]      (make publish [version=0.2])
#
# Builds origin/main's Release app in worktrees/publish (never installs it; the running app stays
# open), zips Hush.app and publishes it as the GitHub release v<version> on that commit.
# Versions: v0.1 first, then each run bumps the last number (0.1.1, 0.1.2, …); a version argument
# starts a new line (0.2, then 0.2.1, …). A commit that already has a release is skipped.
set -euo pipefail
root="$(cd "$(git rev-parse --path-format=absolute --git-common-dir)/.." && pwd)"
cd "$root"
git fetch -q origin main
sha=$(git rev-parse origin/main)
# one "<commit>\trefs/tags/v…" line per release (annotated tags add a "…^{}" line with the commit)
tags=$(git ls-remote --tags origin 'v*')
if grep -q "^$sha" <<<"$tags"; then
  echo "✓ $(git rev-parse --short "$sha") is already released"; exit 0
fi

version=${1:-}
if [ -n "$version" ]; then
  [[ $version =~ ^[0-9]+\.[0-9]+$ ]] || { echo "✗ a new line is major.minor, like 0.2 (got: $version)" >&2; exit 2; }
  ! grep -q "refs/tags/v$version$" <<<"$tags" || { echo "✗ v$version is already released" >&2; exit 2; }
else
  last=$(sed -n 's|.*refs/tags/v\([0-9][0-9.]*\)$|\1|p' <<<"$tags" | sort -t. -k1,1n -k2,2n -k3,3n | tail -1)
  if [ -z "$last" ]; then version=0.1
  elif [[ $last =~ ^[0-9]+\.[0-9]+$ ]]; then version=$last.1
  else version=${last%.*}.$((${last##*.} + 1)); fi
fi

dir=worktrees/publish
if [ -d "$dir" ]; then git -C "$dir" checkout -q --detach "$sha"; else git worktree add -q --detach "$dir" "$sha"; fi
make -C "$dir" bundle

out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
ditto -c -k --keepParent "$dir/build/Build/Products/Release/Hush.app" "$out/Hush-$version.zip"
gh release create "v$version" "$out/Hush-$version.zip" --target "$sha" \
  --title "Hush $version" --notes "$(git log -1 --format=%s "$sha")"
echo "✓ Hush $version"
