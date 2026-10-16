# Voice and video calls (Milestone 3 design)

## Context
M3 (#19): 1:1 and group calls. Signal calls run on RingRTC — Signal's library wrapping their
WebRTC fork with the call state machines every official client uses. The signaling envelopes
(`CallMessage`: Offer/Answer/ICE/Hangup/Busy, modern clients fill only the `opaque` RingRTC
payloads) already flow through our stack: libsignal-service carries the proto both ways, and
presage yields incoming call messages to our actor like any other content.

The feasibility spike (2026-10-16) retired the ticket's "largest risk":
- The `ringrtc` Rust crate (git tag, `crate-type = ["staticlib", "lib"]`) builds on stable Rust
  with features `native,prebuilt_webrtc`: its build script downloads Signal's own prebuilt
  `libwebrtc.a` for mac-arm64 (28 MB, sha256-pinned in the ringrtc repo) from
  build-artifacts.signal.org. Verified on this machine: the native 1:1 test binary (`direct`)
  built in under a minute and initialized CoreAudio (cubeb) with this Mac's devices. No WebRTC
  source build, no fork. Build prerequisites: `cmake`, `coreutils` (Homebrew), `protoc`.
- The `native` API surface is a clean embedder contract: `CallManager` (call/proceed/accept/
  hangup/received_*), `NativePlatform` with three traits we implement (`SignalingSender`,
  `CallStateHandler`, `GroupUpdateHandler`), and `PeerConnectionFactory` for tracks and device
  lists. `src/rust/src/bin/direct.rs` is a complete working native embedder to crib from.
- Audio is free: RingRTC's own cubeb/CoreAudio audio device module, with device enumeration and
  selection APIs. Video is the embedder's job in both directions (push outgoing frames, sink
  incoming ones) — the same contract Signal Desktop implements.
- Precedent: Whisperfish (Rust + presage) got partway and stalled on an OpenSSL/BoringSSL
  conflict specific to their Linux stack; our tree has no OpenSSL (SQLCipher comes through the
  whisperfish libsqlite3-sys fork), and the spike's successful link proves no collision.
- Version coupling: ringrtc v2.72.1 pins libsignal v0.99.1 internally; our tree pins v0.99.0.
  Cargo simply carries both source trees (Signal acknowledges this pattern, ringrtc#55); no
  zkgroup types cross ringrtc's public API, so the duplication costs build time, not
  correctness. RingRTC is AGPL-3.0, same as Semaphore.

## Decisions (owner, 2026-10-16)
1. **Voice only for now.** M3 ships 1:1 voice calls; video is deferred to its own later stage
   (M3b), group calls later still (M3c, its own spec). The owner's words: "lets do JUST voice
   for now. we can put video for later." This spec's build scope is therefore M3a; the M3b
   notes stay as the agreed direction for later.
2. **Use the `ringrtc` crate with Signal's prebuilt WebRTC**, pinned by tag (v2.72.1), like
   presage is pinned by rev. The prebuilt is fetched at build time from Signal's artifact
   bucket and checksum-verified by ringrtc's own build script. Bumps happen in their own PR.
   (Alternative: build WebRTC from source — tens of GB and hours, only needed to patch WebRTC
   itself.)
3. **Accept the duplicated libsignal tree** (v0.99.0 via presage, v0.99.1 inside ringrtc) for
   now; revisit with a `[patch]` or a presage libsignal bump in a later chore PR if build times
   hurt.
4. **Voice-only first means no video plumbing in M3a at all**: a dummy incoming-video sink and
   an unused outgoing track. M3b adds AVFoundation capture and a Metal-backed renderer with a
   dedicated frame path outside uniffi (frames are ~40 MB/s at 720p30 — too heavy for uniffi's
   copying callbacks; raw-pointer FFI or IOSurface sharing, decided in the M3b plan).
5. **UI: a dedicated call window + a notification.** An incoming call shows a notification with
   Accept/Decline (and rings); accepting opens a compact call window (title, state, duration,
   mute, device pickers, hang up). No CallKit (macOS's is Catalyst-oriented); no menu-bar call
   controls in M3.

## Scope
**M3a (in):**
- Outgoing 1:1 voice call from a direct chat's header; incoming 1:1 voice call with
  ring + notification + Accept/Decline.
- Mute, speaker/microphone pickers, call duration, hangup; busy signal when already in a call.
- Missed/declined/accepted call events in the chat timeline (new `ChatEventKind` entries), and
  a missed-call notification.
- TURN/STUN relay fetch from the chat server.
- Mic permission flow (`NSMicrophoneUsageDescription`, TCC prompt on first call).

**Out for now — M3b (later, agreed direction):** 1:1 video — camera capture, incoming
rendering, camera toggle, camera permission.

**Out (M3c and later):** group calls, call links, screen share, call history sync
(`SyncMessage::CallEvent` beyond timeline events), stories. Also out: ringing multiple linked
devices' coordination beyond what RingRTC does by itself.

## Components
New core code lives in `core/sig-core/src/calls/`.

- **Dependency (`core/sig-core/Cargo.toml`).**
  `ringrtc = { git = "https://github.com/signalapp/ringrtc", tag = "v2.72.1", features = ["prebuilt_webrtc"] }`.
- **Wire codec (`calls/wire.rs`).** Pure translation between RingRTC's `signaling::Message`
  and the `CallMessage` proto, both directions; junk fields are skipped, never fatal.
- **Call thread (`calls/actor.rs` + `calls/state.rs`).** Its own OS thread owns the
  `CallManager` and `PeerConnectionFactory` (created lazily — the audio stack opens on first
  use). RingRTC's callbacks (its worker threads) only map state (pure `state.rs` rules) and
  push channel messages; `proceed()` routes back through the thread after the relays arrive.
  One call at a time; a second incoming offer gets RingRTC's busy path and a missed-call event.
  Sends are ack-driven (`should_assume_messages_sent = false`): the sender reports each
  envelope's outcome back as `message_sent`/`message_send_failure`.
- **Relays (`calls/relays.rs`).** Authenticated `GET /v2/calling/relays` through the shared
  `net.rs` PushService (also used by the history fetcher), fetched per `proceed()`; failure or
  an empty answer still lets the call try direct ICE, with a Core Log note.
- **Core actor (`actor.rs`).** A call router task: incoming `CallMessage`s get the sender's and
  our identity keys from presage's store (RingRTC's call fingerprint needs the real ones; a
  sender with no stored identity can't ring) and go to the call thread; outgoing envelopes ride
  the existing sender as `Job::Call` with an ack; chat events (OutgoingCall/IncomingCall with
  duration, MissedCall for unanswered or declined) land in the projection.
- **Client (`client.rs`, uniffi).** `start_call(conversation_id)` (direct non-PNI chats only),
  `accept_call()`, `hangup_call()`, `set_call_muted(bool)`, `call_audio_devices()`,
  `set_call_audio_input/output(id)`, and `CoreEvent::CallChanged { conversation_id, phase,
  reason }` with `CallPhase` (Incoming/Outgoing/Ringing/Connecting/Connected/Ended).
- **App.**
  - `CallWindow.swift`: a small fixed-size window per active call (peer name/avatar initials,
    state line, duration, mute toggle, device pickers, hang up).
  - Incoming: ringing notification with Accept/Decline actions (and in-app banner when active);
    ring sound respects the notification settings.
  - Chat header gains a phone button in direct chats; the timeline shows call events.
  - `Info.plist`: `NSMicrophoneUsageDescription` (M3b: `NSCameraUsageDescription`).

## Data flow (1:1 voice)
1. **Outgoing:** app `start_call` → manager `call()` → RingRTC asks `SignalingSender` for an
   Offer → actor sends `CallMessage{offer}` via presage → peer answers → `received_answer` →
   ICE trickles both ways → `CallStateHandler` fires ringing→connected; the app opens the call
   window at `start_call` and updates from events.
2. **Incoming:** actor receives `CallMessage{offer}` → manager `received_offer` → RingRTC
   validates and fires ringing → `CoreEvent::CallEvent{IncomingRinging}` → notification +
   window; `accept_call()` → `accept()` → connected. Decline → `hangup()` → RingRTC sends the
   Hangup envelope; the chat gets a declined-call event; the phone shows it consistently.
3. **Teardown:** either side's Hangup/Busy, or RingRTC's failure path → `CallEvent{Ended{why}}`
   → window closes, chat event written (missed if never accepted and remote hung up).

## Error handling
| Case | Behaviour |
|---|---|
| Relay fetch fails | The call still tries (STUN-less direct ICE may work); event carries a "connectivity limited" note; errors in the Core Log |
| Mic permission denied | The call UI says so and offers System Settings; no retry loop |
| Offer while already in a call | RingRTC busy path: callee's other call untouched, caller sees Busy |
| Peer never answers | RingRTC's ringing timeout ends the call; missed-call event on their side, "no answer" on ours |
| Network drops mid-call | RingRTC reconnects ICE; prolonged loss → Ended(ConnectionLost), event in chat |
| App quits mid-call | RingRTC threads die with the process; peer times out; no persistence needed |
| CallMessage for an unknown/PNI-only sender | Logged (ids only), ignored |

Privacy: call events in logs are ids, states and reasons only. RingRTC's `hide_ip` option is
wired to "always relay calls" later (M4 settings); default follows Signal's default (off).

## Testing
- **Bridge unit tests:** fake `SignalingSender`/`CallStateHandler` targets — our translation
  of envelopes→CallManager inputs and events→CoreEvents, including the busy and
  unknown-sender paths. CallManager itself is Signal-tested; we test our glue by wrapping it
  behind a thin trait where needed.
- **Relay fetch:** scripted-server tests like the history fetcher's.
- **Chat events:** projection tests for the new kinds (insert, dedupe, summaries).
- **App:** AppStore tests for CallEvent handling (window state machine, notification trigger);
  FakeClient grows the call methods.
- **Manual, on the owner's phone:** outgoing/incoming voice both ways, decline, busy, hangup
  from each side, mic permission prompt, device switching mid-call, poor-network behaviour.
  (Media correctness is only provable live; this is the real gate, like M2's phone tests.)

## Open questions (answered during implementation)
- Whether ringrtc links cleanly *inside* sig-core's staticlib next to presage (the spike linked
  a standalone binary; expect yes — no shared native deps — but the first in-tree build
  proves it). Fallback: a sibling crate linked into the same xcframework.
- Whether ringrtc's build script writing into its cargo checkout's `out/` dir survives clean
  checkouts/CI-less rebuilds gracefully, or we vendor it as a submodule for hygiene.
- The exact relay endpoint shape for linked devices (Desktop uses the calling relays endpoint
  with device credentials; confirm path and JSON against Signal-Server).
- Ring-on-all-devices semantics: what the phone expects from us when it answers first
  (RingRTC handles glare/accepted-elsewhere; verify the events we must surface).

## Build packaging
Cargo never folds foreign static archives into a staticlib, so Signal's prebuilt `libwebrtc.a`
is merged into the xcframework by `scripts/build-core.sh` (libtool), and the app links
CoreAudio, AudioToolbox, CoreGraphics and libc++ (WebRTC and cubeb are C++).

## Estimate
- M3a voice: dependency + bridge + relays ~2–3 days; actor/client wiring ~2 days; app UI +
  notifications ~2–3 days; phone testing throughout. Roughly 1.5 weeks.
- M3b video: frame paths + capture + rendering ~1 week.
- M3c group calls: separate spec; order of 1–2 weeks.
