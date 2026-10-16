#!/usr/bin/env bash
# Builds the Rust core for arm64 and packages it for the app:
#   app/Generated/sig_core.swift            UniFFI Swift bindings
#   app/Frameworks/SigCore.xcframework      static lib + C headers + modulemap
# Usage: scripts/build-core.sh [debug|release]   (default release)
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PROFILE="${1:-release}"
# Cargo reserves the profile name "debug" (it is "dev"), but its output directory is still debug/.
DIR="$PROFILE"; [ "$PROFILE" = dev ] && DIR=debug
[ "$PROFILE" = debug ] && PROFILE=dev
cd "$ROOT/core"
# Match the app's deployment target so C code built by cargo (SQLCipher, libsignal) links without warnings.
export MACOSX_DEPLOYMENT_TARGET=15.0
cargo build -p sig-core --target aarch64-apple-darwin --profile "$PROFILE"
LIB="$ROOT/core/target/aarch64-apple-darwin/$DIR/libsig_core.a"
GEN="$ROOT/app/Generated"; HDR="$ROOT/core/target/uniffi-headers"
rm -rf "$GEN" "$HDR" "$ROOT/app/Frameworks/SigCore.xcframework"
mkdir -p "$GEN" "$HDR" "$ROOT/app/Frameworks"
cargo run -q -p uniffi-bindgen -- generate --library "$LIB" --language swift --out-dir "$GEN"
mv "$GEN"/*.h "$HDR/"
maps=("$GEN"/*.modulemap)
[ ${#maps[@]} -eq 1 ] || { echo "✗ expected one modulemap, got ${#maps[@]}" >&2; exit 1; }
mv "${maps[0]}" "$HDR/module.modulemap"
# RingRTC's prebuilt WebRTC stays a separate static library (cargo doesn't fold foreign
# archives into a staticlib): merge it, or the app fails to link with missing Rust_* symbols.
# ringrtc's build script extracts it into its cargo checkout (release-only prebuilts).
WEBRTC="$(ls "$HOME"/.cargo/git/checkouts/ringrtc-*/*/out/release/obj/libwebrtc.a 2>/dev/null | head -1)"
[ -n "$WEBRTC" ] || { echo "✗ libwebrtc.a not found in the ringrtc checkout (cargo build fetches it)" >&2; exit 1; }
MERGED="$ROOT/core/target/aarch64-apple-darwin/$DIR/libsig_core_merged.a"
libtool -static -no_warning_for_no_symbols -o "$MERGED" "$LIB" "$WEBRTC"
xcodebuild -create-xcframework -library "$MERGED" -headers "$HDR" \
  -output "$ROOT/app/Frameworks/SigCore.xcframework" >/dev/null
echo "built SigCore.xcframework ($DIR, with WebRTC)"
