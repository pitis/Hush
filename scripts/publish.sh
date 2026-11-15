#!/usr/bin/env bash
# scripts/publish.sh [version]      (make publish [version=0.2])
#
# Builds origin/main's Release app in worktrees/publish (never installs it; the running app stays
# open), zips Hush.app and publishes it as the GitHub release v<version> on that commit.
# Versions: v0.1 first, then each run takes the next patch of the highest v… tag on origin
# (0.1.1, 0.1.2, …); a version argument above every tag starts a new line (0.2, then 0.2.1, …).
# A commit with a v… tag on origin is skipped (deleting a GitHub release keeps its tag).
set -euo pipefail
root="$(cd "$(git rev-parse --path-format=absolute --git-common-dir)/.." && pwd)"
cd "$root"
mkdir -p worktrees
# one run at a time: a second one would switch the sources under the first one's build
exec 9>worktrees/.publish.lock
lockf -t 0 9 || { echo "✗ another make publish is running; nothing published" >&2; exit 1; }

git fetch -q origin main
sha=$(git rev-parse origin/main)
# one "<sha>\trefs/tags/v…" line per tag; an annotated tag adds a "…^{}" line with its commit
tags=$(git ls-remote --tags origin 'v*')
if grep -q "^$sha" <<<"$tags"; then
  echo "✓ $(git rev-parse --short "$sha") is already released${1:+ (version $1 not used)}"; exit 0
fi

by_version() { sort -t. -k1,1n -k2,2n -k3,3n; }
last=$(sed -n 's|.*refs/tags/v\([0-9][0-9.]*\)$|\1|p' <<<"$tags" | by_version | tail -1)
version=${1:-}
if [ -n "$version" ]; then
  [[ $version =~ ^[0-9]+\.[0-9]+$ ]] || { echo "✗ a new line is major.minor, like 0.2 (got: $version)" >&2; exit 2; }
  [ "$(printf '%s\n' "$last" "$version" | by_version | tail -1)" = "$version" ] && [ "$version" != "$last" ] ||
    { echo "✗ a new line must be above the latest release, v$last (got: $version)" >&2; exit 2; }
elif [ -z "$last" ]; then version=0.1
elif [[ $last =~ ^[0-9]+\.[0-9]+$ ]]; then version=$last.1
else version=${last%.*}.$((${last##*.} + 1)); fi

dir=worktrees/publish
app=$dir/build/Build/Products/Release/Hush.app
if [ -e "$dir/.git" ]; then
  changes=$(git -C "$dir" status --porcelain)
  [ -z "$changes" ] || { echo "✗ $dir has local changes (git -C $dir status); nothing published" >&2; exit 1; }
  git -C "$dir" checkout -q --detach "$sha"
else
  git worktree add -q --detach "$dir" "$sha"
fi

out=$(mktemp -d)
# a fresh bundle (an incremental build keeps files main deleted), and no built copy left for
# Spotlight or the Dock to open in place of the installed app
rm -rf "$app"
trap 'rm -rf "$out" "$app"' EXIT
make -C "$dir" bundle
ditto -c -k --keepParent "$app" "$out/Hush-$version.zip"
gh release create "v$version" "$out/Hush-$version.zip" --target "$sha" \
  --title "Hush $version" --notes "$(git log -1 --format=%s "$sha")"
echo "✓ Hush $version"
