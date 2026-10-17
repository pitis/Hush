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
