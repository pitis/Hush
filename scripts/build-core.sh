#!/usr/bin/env bash
# Builds the Rust core for arm64 and packages it for the app:
#   app/Generated/sig_core.swift            UniFFI Swift bindings
#   app/Frameworks/SigCore.xcframework      static lib + C headers + modulemap
# Usage: scripts/build-core.sh [debug|release]   (default release)
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PROFILE="${1:-release}"
DIR="$PROFILE"; [ "$PROFILE" = dev ] && DIR=debug
[ "$PROFILE" = debug ] && PROFILE=dev
cd "$ROOT/core"
cargo build -p sig-core --target aarch64-apple-darwin --profile "$PROFILE"
LIB="$ROOT/core/target/aarch64-apple-darwin/$DIR/libsig_core.a"
GEN="$ROOT/app/Generated"; HDR="$ROOT/core/target/uniffi-headers"
rm -rf "$GEN" "$HDR" "$ROOT/app/Frameworks/SigCore.xcframework"
mkdir -p "$GEN" "$HDR" "$ROOT/app/Frameworks"
cargo run -q -p uniffi-bindgen -- generate --library "$LIB" --language swift --out-dir "$GEN"
mv "$GEN"/*.h "$HDR/"
mv "$GEN"/*.modulemap "$HDR/module.modulemap"
xcodebuild -create-xcframework -library "$LIB" -headers "$HDR" \
  -output "$ROOT/app/Frameworks/SigCore.xcframework" >/dev/null
echo "built SigCore.xcframework ($DIR)"
