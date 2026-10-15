#!/usr/bin/env bash
# Regenerates app/Semaphore/Assets.xcassets/AppIcon.appiconset from scripts/make-icon.swift.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SET="$ROOT/app/Semaphore/Assets.xcassets/AppIcon.appiconset"
mkdir -p "$SET"
swift "$ROOT/scripts/make-icon.swift" "$SET/icon_1024.png"
images=""
for pt in 16 32 128 256 512; do
  for scale in 1 2; do
    px=$((pt * scale)); name="icon_${pt}@${scale}x.png"
    sips -z "$px" "$px" "$SET/icon_1024.png" --out "$SET/$name" >/dev/null
    images+="{\"idiom\":\"mac\",\"size\":\"${pt}x${pt}\",\"scale\":\"${scale}x\",\"filename\":\"$name\"},"
  done
done
rm "$SET/icon_1024.png"
printf '{"images":[%s],"info":{"author":"xcode","version":1}}\n' "${images%,}" > "$SET/Contents.json"
printf '{"info":{"author":"xcode","version":1}}\n' > "$SET/../Contents.json"
echo "✓ $SET"
