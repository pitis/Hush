# Hush

An unofficial, personal native macOS client for Signal: a Rust core built on
[presage](https://github.com/whisperfish/presage) and a SwiftUI/AppKit app. It links to your
phone as a secondary device, like Signal Desktop. Not affiliated with Signal. AGPL-3.0.

```sh
make setup   # once: brew tools, Rust target, git hooks
make run     # build the core + app and launch it
make check   # lint + all tests
```

Building the app needs the owner's Apple Development certificate (see `app/project.yml`).

## Desktop app (Windows, Linux, macOS)

`core/desktop` is a second, smaller client written in Rust on [GPUI](https://www.gpui.rs) over the
same core: it links as its own device and keeps its own data. It is meant to link, list the chats,
show a chat's newest messages and send text; it hasn't been tried with a linked phone yet.

```sh
make desktop-run   # or: cd core && cargo run -p hush-desktop
```

Linux (x86-64) needs `protobuf-compiler clang cmake pkg-config libxkbcommon-x11-dev libwayland-dev
libx11-xcb-dev libfontconfig-dev libfreetype-dev libvulkan-dev libasound2-dev` and a Secret Service
(GNOME Keyring or KWallet) for the database key. arm64 Linux doesn't build yet: the SQLCipher fork's
Rust crypto assumes a signed C `char`. Windows is untested: expect it to need `protoc`, Git Bash
(RingRTC's build runs its WebRTC download script with bash), and `OPENSSL_DIR` set to any folder
with `lib` and `include` in it (the SQLite build script asks for it although SQLCipher doesn't use
OpenSSL there).
