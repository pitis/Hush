# 1:1 Voice Calls (M3a) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Outgoing and incoming 1:1 voice calls (ring, accept/decline, mute, device pickers,
hangup, busy, call events in the chat), on RingRTC with Signal's prebuilt WebRTC.

**Architecture:** sig-core gains a `calls/` module: a pure signaling codec between RingRTC's
`signaling::Message` and libsignal-service's `CallMessage` proto; a relay (TURN) fetcher over
the existing `PushService` route; and a `CallActor` owning RingRTC's `CallManager` +
`PeerConnectionFactory` on its own thread (RingRTC is internally threaded and `Send`-friendly;
presage stays untouched on the core actor thread). The core actor routes incoming
`ContentBody::CallMessage`s to the CallActor and sends its outgoing envelopes through the
existing sender with delivery acks. Call state surfaces as `CoreEvent::CallChanged`; finished
calls become chat events. The app adds a call window, ringing notification with Accept/Decline,
and a call button in direct chats.

**Tech Stack:** ringrtc v2.72.1 (`native`, `prebuilt_webrtc` — downloads Signal's checksummed
`libwebrtc.a` for mac-arm64 at build time), presage/libsignal-service (CallMessage transport),
uniffi, SwiftUI.

**Spec:** `docs/superpowers/specs/2026-10-16-m3-calls-design.md` (voice-only scope; the
spike's API findings are in the spec's Context).

## Global Constraints

- Voice only: no camera/frame plumbing; the outgoing video track exists but stays disabled and
  the incoming sink is a no-op.
- ringrtc pinned by tag `v2.72.1`; bumped only in its own PR (like presage's rev pin).
- presage's `Manager`/store stay `!Send` on the core actor thread; RingRTC's callbacks arrive
  on its own worker threads and must never touch presage directly.
- Never log message content; for calls: ids, call ids, states and reasons only. Never log
  opaque payloads or ICE candidates (they contain addresses; log counts).
- Never edit `app/Generated/`; regenerate with `make core && make project`.
- No `--no-verify`; `make format && make check` before push (Swift tests run unsigned on this
  machine: `xcodebuild … test CODE_SIGNING_ALLOWED=NO CODE_SIGN_IDENTITY=""`).
- Work in `/Users/pitis/development/semaphore/worktrees/48--calls`, branch `feat/calls`.
- Commits: plain sentences, no `#NNN`, no AI attribution.
- App strings may say "Signal" only in instructions about the phone app, never as our branding.

## Review Focus

1. **A CallMessage from a PNI-only or unknown sender** — must be ignored with an id-only log,
   never crash or ring. Test in Task 6 (actor routing).
2. **A second incoming offer while already in a call** — RingRTC answers Busy; our side must
   write a missed-call chat event for the rejected call and keep the active call untouched.
   Test in Task 5 (state mapping for `CallState::Rejected`).
3. **A signaling send that fails (offline caller)** — the ack path must call
   `message_send_failure` so RingRTC ends the call; the UI must reach `Ended`, not hang in
   "Calling…". Test in Task 6 (ack plumbing).
4. **A CallMessage carrying several fields at once** (proto allows offer+ice in one envelope) —
   decode deterministically, offer first, and never drop the ice. Test in Task 2 (codec).
5. **Relay fetch failing or returning an empty list** — the call still proceeds (direct ICE),
   with a Core Log error, never a stuck pre-ring state. Test in Task 4 (fetch) and Task 5
   (proceed with empty relays).

---

### Task 1: The ringrtc dependency and the call event surface

Adds the dependency (proving it links inside sig-core next to presage — the spec's first open
question), and the uniffi-visible types every later task emits.

**Files:**
- Modify: `core/sig-core/Cargo.toml`
- Create: `core/sig-core/src/calls/mod.rs`
- Modify: `core/sig-core/src/lib.rs` (add `mod calls;` + re-exports)
- Modify: `core/sig-core/src/events.rs` (new CoreEvent variant)

**Interfaces:**
- Produces:

```rust
// calls/mod.rs
/// Where a call is, as the app shows it.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallPhase {
    /// Someone is calling us (ring until accepted or declined).
    Incoming,
    /// We are calling; the offer is on its way.
    Outgoing,
    /// The remote side is ringing.
    Ringing,
    Connecting,
    Connected,
    /// Over — `CallChanged.reason` says why (hung up, declined, busy, no answer, failed).
    Ended,
}
```

and in `events.rs`:

```rust
/// A 1:1 call changed state. `conversation_id` is the peer's chat; `reason` is set for
/// `Ended` ("hung up", "declined", "busy", "no answer", or an error).
CallChanged {
    conversation_id: String,
    phase: crate::calls::CallPhase,
    reason: Option<String>,
},
```

- [ ] **Step 1: Add the dependency**

In `core/sig-core/Cargo.toml`:

```toml
# Signal's call engine. `prebuilt_webrtc` downloads their checksummed libwebrtc.a for
# mac-arm64 at build time, so no WebRTC source build. Pinned by tag; bump in its own PR.
ringrtc = { git = "https://github.com/signalapp/ringrtc", tag = "v2.72.1", features = ["prebuilt_webrtc"] }
```

- [ ] **Step 2: Verify it builds inside sig-core** —
  `cd core && cargo check -p sig-core 2>&1 | tail -5`. Expected: success (first run fetches the
  28 MB artifact; needs `cmake` and `coreutils` from Homebrew, both already installed). If the
  staticlib link ever fails later at `make core`, the fallback is a sibling crate — stop and
  reassess then, not now.

- [ ] **Step 3: Write the failing test** (in `calls/mod.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phases_are_distinct_and_ended_is_terminal() {
        // pins the enum's existence and that Ended compares as itself (the app's dismiss cue)
        assert_ne!(CallPhase::Incoming, CallPhase::Ended);
        assert_eq!(CallPhase::Ended, CallPhase::Ended);
    }
}
```

- [ ] **Step 4: Run it** — `cargo test -p sig-core calls` → FAIL (module missing), then add
  `mod calls;` + `pub use calls::CallPhase;` to lib.rs, the enum, and the CoreEvent variant →
  PASS. Full suite green.

- [ ] **Step 5: Commit** — `git commit -am "The core can say where a call stands"`

---

### Task 2: The signaling codec

Pure translation between RingRTC's `signaling::Message` and libsignal-service's `CallMessage`
proto, both directions. This is the wire contract with real phones, so it gets the densest
tests.

**Files:**
- Create: `core/sig-core/src/calls/wire.rs`
- Modify: `core/sig-core/src/calls/mod.rs` (`pub(crate) mod wire;`)

**Interfaces:**
- Consumes: `ringrtc::core::signaling` (`Message`, `Offer`, `Answer`, `Ice`, `IceCandidate`,
  `Hangup`, `HangupType`), `ringrtc::common::{CallId, CallMediaType, DeviceId}`,
  `presage::proto::CallMessage` (prost; submodules `call_message::{Offer, Answer, IceUpdate,
  Hangup, Busy, offer::Type as OfferType, hangup::Type as HangupType}`).
- Produces:

```rust
/// An envelope for the wire. `destination_device_id` filters receiver-side; the message still
/// goes to every device.
pub(crate) fn to_proto(
    call_id: ringrtc::common::CallId,
    receiver_device_id: Option<ringrtc::common::DeviceId>,
    message: &ringrtc::core::signaling::Message,
) -> presage::proto::CallMessage;

/// What one incoming CallMessage asks of the call engine, in handling order (an envelope may
/// carry an offer and ice updates together).
pub(crate) struct IncomingCall {
    pub call_id: ringrtc::common::CallId,
    pub message: ringrtc::core::signaling::Message,
}
pub(crate) fn from_proto(proto: &presage::proto::CallMessage) -> Vec<IncomingCall>;
```

Mapping rules (each line is asserted by a test):
- `Message::Offer` → `offer { id: call_id, type: OFFER_AUDIO_CALL (voice only here), opaque }`.
- `Message::Answer` → `answer { id, opaque }`.
- `Message::Ice` → one `iceUpdate { id, opaque }` per candidate.
- `Message::Hangup` → `hangup { id, type, deviceId }` via `Hangup::to_type_and_device_id()`
  (proto HangupType values match ringrtc's `#[repr(i32)]` one for one).
- `Message::Busy` → `busy { id }`.
- `destination_device_id` set iff `receiver_device_id` is `Some`.
- Decoding: offer first, then answer, ice (all candidates in one `Ice`), hangup, busy — one
  `IncomingCall` per present field, in that order. Fields with no `id` or no `opaque` (where
  required) are skipped; `Offer::new`/`Answer::new` failures (undecodable opaque) are skipped —
  count, don't crash. A video offer (`OFFER_VIDEO_CALL`) still decodes (RingRTC answers it;
  we just never enable our camera): `CallMediaType::Video` passes through.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use presage::proto::call_message as pcm;
    use ringrtc::common::CallId;
    use ringrtc::core::signaling;

    use super::*;

    /// A valid little opaque offer body (RingRTC requires it to decode).
    fn opaque_offer() -> Vec<u8> {
        use prost::Message as _;
        // protobuf::signaling::Offer { v4: Some(default) } — encode via ringrtc's test surface:
        // the simplest valid bytes are an empty proto (all fields optional).
        Vec::new()
    }

    #[test]
    fn an_offer_round_trips() {
        let offer = signaling::Offer::new(ringrtc::common::CallMediaType::Audio, opaque_offer()).unwrap();
        let proto = to_proto(CallId::new(7), Some(3), &signaling::Message::Offer(offer));
        let offer_proto = proto.offer.as_ref().unwrap();
        assert_eq!(offer_proto.id, Some(7));
        assert_eq!(offer_proto.r#type, Some(pcm::offer::Type::OfferAudioCall as i32));
        assert_eq!(proto.destination_device_id, Some(3));
        let incoming = from_proto(&proto);
        assert_eq!(incoming.len(), 1);
        assert_eq!(incoming[0].call_id, CallId::new(7));
        assert!(matches!(incoming[0].message, signaling::Message::Offer(_)));
    }

    #[test]
    fn ice_candidates_fan_out_and_back() {
        let ice = signaling::Ice {
            candidates: vec![
                signaling::IceCandidate { opaque: vec![1] },
                signaling::IceCandidate { opaque: vec![2] },
            ],
        };
        let proto = to_proto(CallId::new(9), None, &signaling::Message::Ice(ice));
        assert_eq!(proto.ice_update.len(), 2);
        assert_eq!(proto.destination_device_id, None);
        let incoming = from_proto(&proto);
        assert_eq!(incoming.len(), 1, "all candidates in one Ice");
        match &incoming[0].message {
            signaling::Message::Ice(ice) => assert_eq!(ice.candidates.len(), 2),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn hangup_types_match_the_proto_numbers() {
        for (hangup, proto_type, device) in [
            (signaling::Hangup::Normal, pcm::hangup::Type::HangupNormal, None),
            (
                signaling::Hangup::AcceptedOnAnotherDevice(2),
                pcm::hangup::Type::HangupAccepted,
                Some(2),
            ),
            (
                signaling::Hangup::DeclinedOnAnotherDevice(2),
                pcm::hangup::Type::HangupDeclined,
                Some(2),
            ),
        ] {
            let proto = to_proto(CallId::new(1), None, &signaling::Message::Hangup(hangup));
            let h = proto.hangup.as_ref().unwrap();
            assert_eq!(h.r#type, Some(proto_type as i32));
            assert_eq!(h.device_id, device);
            let back = from_proto(&proto);
            assert!(matches!(back[0].message, signaling::Message::Hangup(_)));
        }
    }

    #[test]
    fn busy_round_trips() {
        let proto = to_proto(CallId::new(4), None, &signaling::Message::Busy);
        assert_eq!(proto.busy.as_ref().unwrap().id, Some(4));
        assert!(matches!(from_proto(&proto)[0].message, signaling::Message::Busy));
    }

    /// The proto allows several fields in one envelope: offer first, nothing dropped.
    #[test]
    fn a_combined_envelope_keeps_order() {
        let offer = signaling::Offer::new(ringrtc::common::CallMediaType::Audio, opaque_offer()).unwrap();
        let mut proto = to_proto(CallId::new(7), None, &signaling::Message::Offer(offer));
        proto.ice_update.push(pcm::IceUpdate {
            id: Some(7),
            opaque: Some(vec![1]),
        });
        let incoming = from_proto(&proto);
        assert_eq!(incoming.len(), 2);
        assert!(matches!(incoming[0].message, signaling::Message::Offer(_)));
        assert!(matches!(incoming[1].message, signaling::Message::Ice(_)));
    }

    /// Junk never panics: missing ids, missing opaques, undecodable opaques are skipped.
    #[test]
    fn junk_fields_are_skipped() {
        let proto = presage::proto::CallMessage {
            offer: Some(pcm::Offer {
                id: None, // no id: skip
                r#type: Some(pcm::offer::Type::OfferAudioCall as i32),
                opaque: Some(opaque_offer()),
            }),
            busy: Some(pcm::Busy { id: None }),
            ..Default::default()
        };
        assert!(from_proto(&proto).is_empty());
    }
}
```

(**Implementer note:** check `CallId::new(u64)` exists — common/mod.rs has `CallId { id: u64 }`
with `as_u64`; if the constructor is `CallId::from(u64)` or `new`, use what compiles. Same for
prost field spellings: `r#type`, `ice_update`, `destination_device_id` per prost's snake_case.
If `signaling::Offer::new(vec![])` rejects empty bytes, build a minimal valid opaque with
ringrtc's `protobuf::signaling` types if they're public, else any bytes that decode — check
`signaling.rs::deserialize_opaque`; an empty buffer decodes to an all-None proto in prost, so
`Vec::new()` should pass.)

- [ ] **Step 2: Run them** — FAIL (module missing).
- [ ] **Step 3: Implement `wire.rs`** per the mapping rules; decoding uses
  `signaling::Offer::new(media_type, opaque)`, `Answer::new`, `IceCandidate { opaque }`,
  `Hangup::from_type_and_device_id(typ, device_id.unwrap_or(0))`.
- [ ] **Step 4: Run them** — PASS; full suite green.
- [ ] **Step 5: Commit** — `git commit -am "Call signaling translates between RingRTC and the wire"`

---

### Task 3: Call events in the chat timeline

Finished calls become chat events, like timer changes do.

**Files:**
- Modify: `core/sig-core/src/projection/model.rs` (`ChatEventKind`)
- Modify: `core/sig-core/src/projection/db.rs` (only if `add_chat_event` needs a `value` param —
  check: `add_chat_event(&self, conversation_id, at, kind, author)` exists; add a
  `add_chat_event_value(..., value: Option<i64>)` variant or extend the existing fn and update
  its callers)
- Modify: `app/Semaphore/Timeline.swift` + `app/Semaphore/Placeholder.swift` (wording) — the
  Swift side lands in Task 8; this task is core-only.

**Interfaces:**
- Produces: `ChatEventKind::{OutgoingCall, IncomingCall, MissedCall}` with
  `to_i64` = 3, 4, 5; `value` carries the call's duration in seconds (NULL: never connected).
  MissedCall covers unanswered and declined incoming calls (Desktop's wording model).

- [ ] **Step 1: Write the failing test** (model.rs tests)

```rust
#[test]
fn call_event_kinds_round_trip() {
    for (kind, n) in [
        (ChatEventKind::OutgoingCall, 3),
        (ChatEventKind::IncomingCall, 4),
        (ChatEventKind::MissedCall, 5),
    ] {
        assert_eq!(kind.to_i64(), n);
        assert_eq!(ChatEventKind::from_i64(n), Some(kind));
    }
}
```

- [ ] **Step 2: Run** — FAIL; **Step 3:** add the variants (update `from_i64`'s array and any
  exhaustive matches the compiler flags); **Step 4:** suite green (db's `chat_events` already
  reads `value`).
- [ ] **Step 5: db test** — a call event lands in the timeline and bumps the conversation:

```rust
#[tokio::test]
async fn a_missed_call_shows_in_the_chat() {
    let db = db().await;
    db.add_chat_event("c:alice", 50, ChatEventKind::MissedCall, "alice").await.unwrap();
    let events = db.chat_events("c:alice", 0).await.unwrap();
    assert_eq!(events[0].kind, ChatEventKind::MissedCall);
}
```

(If durations need storing, extend `add_chat_event` with `value: Option<i64>` — mirror how
`Op::SetTimer` writes `value` — and update existing callers to pass `None`.)

- [ ] **Step 6: Commit** — `git commit -am "Calls leave their trace in the chat timeline"`

---

### Task 4: The relay fetch

TURN/STUN servers from the chat server, behind a trait like the history fetcher's.

**Files:**
- Create: `core/sig-core/src/calls/relays.rs`
- Modify: `core/sig-core/src/calls/mod.rs`

**Interfaces:**
- Produces:

```rust
/// One relay group from `GET /v2/calling/relays` (Desktop's iceServerConfigToList shape).
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub(crate) struct RelayGroup {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub urls: Option<Vec<String>>,
    #[serde(rename = "urlsWithIps", default)]
    pub urls_with_ips: Option<Vec<String>>,
}

/// Parses the server's JSON (`{"relays": [...]}`); tolerant of unknown fields.
pub(crate) fn parse_relays(body: &serde_json::Value) -> Vec<RelayGroup>;

/// Each group becomes up to two RingRTC IceServers, exactly as Desktop builds them:
/// (hostname + urlsWithIps) and ("" + urls).
pub(crate) fn to_ice_servers(groups: &[RelayGroup]) -> Vec<ringrtc::webrtc::peer_connection_factory::IceServer>;

/// Fetches and converts; an error or empty answer gives `vec![]` with a warn — a call still
/// tries direct ICE.
pub(crate) async fn fetch(push: &presage::libsignal_service::prelude::PushService) -> Vec<...IceServer>;
```

The `PushService` comes from the same credential construction the history fetcher uses: factor
`SignalServer::from_registration`'s credential block in `history/fetch.rs` into a shared
`pub(crate) fn push_service(data: &presage::manager::RegistrationData) -> Result<PushService, String>`
(put it in a new `core/sig-core/src/net.rs`; history's `SignalServer` and this both call it).

- [ ] **Step 1: Write the failing tests** (pure parts)

```rust
#[test]
fn desktop_shaped_json_parses() {
    let v: serde_json::Value = serde_json::json!({
        "relays": [{
            "username": "u", "password": "p", "hostname": "turn.example.org",
            "urls": ["turn:example.org"], "urlsWithIps": ["turn:1.2.3.4"]
        }]
    });
    let groups = parse_relays(&v);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].urls_with_ips.as_deref(), Some(&["turn:1.2.3.4".to_string()][..]));
    // one group → two IceServers (ip-pinned with hostname, plain with empty hostname)
    assert_eq!(to_ice_servers(&groups).len(), 2);
}

#[test]
fn empty_or_junk_json_means_no_relays() {
    assert!(parse_relays(&serde_json::json!({})).is_empty());
    assert!(parse_relays(&serde_json::json!({"relays": "nope"})).is_empty());
    // a group with neither url list yields no servers
    let only_creds = vec![RelayGroup { username: "u".into(), password: "p".into(), hostname: None, urls: None, urls_with_ips: None }];
    assert!(to_ice_servers(&only_creds).is_empty());
}
```

- [ ] **Step 2: Run** — FAIL. **Step 3:** implement (the `fetch` fn mirrors history's `wait`
  request: `Endpoint::service("/v2/calling/relays")`, `HttpAuthOverride::NoOverride`, json,
  errors → `tracing::warn!` + `vec![]`). **Step 4:** PASS, suite green.
- [ ] **Step 5: Commit** — `git commit -am "The core fetches calling relays"`

---

### Task 5: The CallActor

RingRTC lives on its own thread: a `CallActor` owning `CallManager` + `PeerConnectionFactory` +
the active call's bookkeeping, driven by a command channel, reporting through the
`EventListener` and a channel back to the core actor (for sends and chat events). The state
mapping (RingRTC `CallState` → our `CallPhase` + chat-event decisions) is pure and fully
tested; the thread wrapper is thin.

**Files:**
- Create: `core/sig-core/src/calls/state.rs` (pure mapping)
- Create: `core/sig-core/src/calls/actor.rs` (thread + CallManager glue)
- Modify: `core/sig-core/src/calls/mod.rs`

**Interfaces:**
- Consumes: Tasks 1, 2, 4.
- Produces (`state.rs`, pure):

```rust
/// Our bookkeeping for the one active call.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ActiveCall {
    pub call_id_u64: u64,
    /// The peer's ACI uuid string (the conversation is `c:<this>`).
    pub peer: String,
    pub incoming: bool,
    /// ms timestamps; connected_at None until Connected.
    pub started_at: i64,
    pub connected_at: Option<i64>,
}

/// What a RingRTC state change means for us.
#[derive(Debug, PartialEq)]
pub(crate) struct Mapped {
    pub phase: Option<(crate::calls::CallPhase, Option<String>)>, // None: not user-visible
    /// A chat event to write when the call concludes (kind, value = duration seconds).
    pub chat_event: Option<(crate::projection::ChatEventKind, Option<i64>)>,
    pub call_over: bool,
}

pub(crate) fn map_state(
    active: &ActiveCall,
    state: &ringrtc::native::CallState,
    now_ms: i64,
) -> Mapped;
```

Mapping rules (each a test):
- `Incoming(_)` → phase Incoming; `Outgoing(_)` → phase Outgoing (both also trigger `proceed`,
  done by the actor, not the mapper).
- `Ringing` → phase Ringing.
- `Connecting` → Connecting; `Connected` → Connected (sets `connected_at`, handled by caller).
- `Ended(reason, _)` → phase Ended with a human reason; `call_over`; chat event:
  - connected before → (Incoming→IncomingCall / Outgoing→OutgoingCall, duration =
    (now-connected_at)/1000);
  - never connected and incoming → MissedCall (covers remote hangup and our decline);
  - never connected and outgoing → OutgoingCall with value None ("no answer").
- `Rejected(reason)` → **no phase change for the active call** (this fires for a *second* offer
  rejected while busy — `ReceivedOfferWhileActive` — and for expired offers); chat event
  MissedCall for `remote_peer` of that rejection; `call_over` false.
- `Concluded` → nothing visible; `call_over` true (final cleanup).

Produces (`actor.rs`):

```rust
/// Commands from the Client/core actor.
pub(crate) enum CallCommand {
    /// aci: bare uuid string; our device id; relays fetched by the caller (core actor).
    Start { peer_aci: String },
    Accept,
    Hangup,
    SetMuted(bool),
    /// An incoming envelope, pre-translated. The identity keys come from presage's protocol
    /// store (the core actor has it); RingRTC feeds them into the call fingerprint, and real
    /// phones send their real keys, so ours must match.
    Incoming {
        sender_aci: String,
        sender_device_id: u32,
        age_secs: u64,
        sender_identity_key: Vec<u8>,
        receiver_identity_key: Vec<u8>,
        calls: Vec<crate::calls::wire::IncomingCall>,
    },
    /// The relays answer for a pending proceed.
    Relays(Vec<IceServer>),
    /// The send outcome of an outgoing envelope (drives message_sent / message_send_failure).
    SendResult { call_id_u64: u64, ok: bool },
    ListAudioDevices { reply: tokio::sync::oneshot::Sender<AudioDevices> },
    SetAudioInput(String),  // device unique id
    SetAudioOutput(String),
}

/// What the CallActor asks the core actor to do (sends run through presage).
pub(crate) enum CallOutput {
    /// Send this envelope to the peer (and note the call id for the ack).
    Send {
        peer_aci: String,
        call_id_u64: u64,
        proto: presage::proto::CallMessage,
        urgent: bool,
    },
    /// Fetch relays and answer with CallCommand::Relays.
    NeedRelays,
    /// Write a chat event (conversation c:<aci>).
    ChatEvent {
        peer_aci: String,
        kind: crate::projection::ChatEventKind,
        value: Option<i64>,
    },
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct AudioDevice { pub id: String, pub name: String }
#[derive(uniffi::Record, Clone, Debug)]
pub struct AudioDevices { pub inputs: Vec<AudioDevice>, pub outputs: Vec<AudioDevice>,
    pub current_input: Option<String>, pub current_output: Option<String> }

/// Spawns the call thread. `our_device_id` from registration; outputs flow to `out_tx`;
/// state changes go to the listener as `CoreEvent::CallChanged`.
pub(crate) fn spawn(
    our_device_id: u32,
    our_aci: String,
    listener: std::sync::Arc<dyn crate::events::EventListener>,
    out_tx: tokio::sync::mpsc::UnboundedSender<CallOutput>,
) -> tokio::sync::mpsc::UnboundedSender<CallCommand>;
```

Actor internals (implementer's map, from the spike's `direct.rs` reading):
- A `std::thread` with a small command loop (std mpsc or a current-thread tokio runtime —
  plain `std::sync::mpsc` suffices; RingRTC is callback-driven).
- Lazy init on first Start/Incoming: `PeerConnectionFactory::new(&AudioConfig::default(),
  false, "", None)`, `pcf.create_outgoing_audio_track()`, `create_outgoing_video_source()` +
  `create_outgoing_video_track(&src)` (video track stays disabled),
  `NativePlatform::new(pcf.clone(), Box::new(sender), /*should_assume_messages_sent*/ false,
  Box::new(handler), Box::new(NoGroups))`, `CallManager::new(platform,
  http::DelegatingClient::new(NoHttp))`.
- `SignalingSender::send_signaling` impl: `wire::to_proto` + `CallOutput::Send` (urgent =
  matches!(message, Offer|Answer|Hangup|Busy); ice is not urgent) — then the core actor sends
  and reports `SendResult`, which maps to `call_manager.message_sent(call_id)` or
  `message_send_failure(call_id)`. The `send_call_message*` group-call methods return
  `Ok(())` with a warn (unreachable in 1:1).
- `CallStateHandler::handle_call_state`: on `Incoming/Outgoing` remember the pending call and
  emit `CallOutput::NeedRelays`; the `Relays` answer builds `NativeCallContext::new(false,
  ice_servers, audio_track.clone(), video_track.clone(), Box::new(NoVideo))` and calls
  `call_manager.proceed(call_id, ctx, CallConfig::default(), None)`. Everything else goes
  through `state::map_state` → listener event + optional `CallOutput::ChatEvent`.
  `handle_remote_audio_state`/`video`/`sharing`/`network_route`/`audio_levels`/
  `low_bandwidth`: log-only (`trace!`) in M3a.
- `Start`: refuse (Error event) if a call is active; else `call_manager.call(peer_aci.clone(),
  CallMediaType::Audio, our_device_id)`.
- `Incoming` envelopes map to `received_offer/answer/ice/hangup/busy` with
  `signaling::Received*` structs, passing the command's identity keys straight through
  (`ReceivedOffer/ReceivedAnswer.sender_identity_key`/`receiver_identity_key`).
- `Accept` → `accept_call(active_id)`; `Hangup` → `call_manager.hangup()` (RingRTC sends the
  right Hangup/Decline envelope itself); `SetMuted(m)` → `audio_track.set_enabled(!m)`.
- Device commands → `pcf.get_audio_{playout,recording}_devices()` /
  `set_audio_{playout,recording}_device_by_id(&id)` (AudioDevice has `name` and `unique_id`
  fields — check the struct in peer_connection_factory.rs and map to our record).
- `NoVideo`: `impl VideoSink { fn on_video_frame(&self, _, _) {} fn box_clone(...) }`.
- `NoHttp`: `impl http::Delegate { fn send_request(&self, id, _req) { /* answer 503 via
  DelegatingClient? no handle here — simply log; group calls only */ } }`.

- [ ] **Step 1: Write the failing state.rs tests** — one per mapping rule above, e.g.:

```rust
#[test]
fn a_connected_incoming_call_ends_as_an_incoming_call_event() {
    let active = ActiveCall { call_id_u64: 1, peer: "a".into(), incoming: true, started_at: 1_000, connected_at: Some(5_000) };
    let m = map_state(&active, &CallState::Ended(CallEndReason::RemoteHangup, Default::default()), 65_000);
    assert_eq!(m.phase.as_ref().unwrap().0, CallPhase::Ended);
    assert_eq!(m.chat_event, Some((ChatEventKind::IncomingCall, Some(60))));
    assert!(m.call_over);
}

#[test]
fn an_unanswered_incoming_call_is_missed() { /* connected_at None, Ended → MissedCall, value None */ }

#[test]
fn an_unanswered_outgoing_call_reads_no_answer() { /* OutgoingCall, value None, reason "no answer" for ReceivedOfferExpired/Timeout reasons */ }

#[test]
fn a_rejected_second_offer_is_missed_but_does_not_end_the_active_call() {
    let active = ActiveCall { /* connected */ };
    let m = map_state(&active, &CallState::Rejected(RejectReason::ReceivedOfferWhileActive), 9_000);
    assert_eq!(m.phase, None);
    assert_eq!(m.chat_event, Some((ChatEventKind::MissedCall, None)));
    assert!(!m.call_over);
}
```

(**Implementer note:** `CallState::Ended(CallEndReason, CallSummary)` — check `CallEndReason`
variants in ringrtc `common/mod.rs` (e.g. `LocalHangup`, `RemoteHangup`, `Declined?`,
`ReceivedOfferExpired`, `Timeout`, `Busy`...) and map reasons to the strings: "hung up",
"declined", "busy", "no answer", else "call failed (<reason:?>)". `CallSummary: Default` —
verify; otherwise construct minimal.)

- [ ] **Step 2: RED**, **Step 3:** implement `state.rs`, **Step 4:** GREEN.
- [ ] **Step 5: actor.rs** — implement per the map above. Testing the thread end-to-end needs
  RingRTC network machinery; keep `actor.rs` glue-thin and add one smoke test:

```rust
#[test]
fn the_call_actor_spawns_and_lists_devices() {
    // proves PCF + cubeb init inside sig-core (the spike proved it standalone)
    let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
    let tx = spawn(1, "me".into(), std::sync::Arc::new(Quiet), out_tx);
    let (reply, rx) = tokio::sync::oneshot::channel();
    tx.send(CallCommand::ListAudioDevices { reply }).unwrap();
    let devices = rx.blocking_recv().unwrap();
    // CI-less dev machine always has at least one output
    assert!(!devices.outputs.is_empty());
}
```

- [ ] **Step 6:** suite green; **Step 7: Commit** —
  `git commit -am "A call engine runs on its own thread"`

---

### Task 6: Core actor and sender wiring

Incoming call messages route to the CallActor (with identity keys and age); its outputs are
sent through presage with acks; relays are fetched on demand; chat events land in the db.

**Files:**
- Modify: `core/sig-core/src/actor.rs`
- Modify: `core/sig-core/src/send.rs` (only if `Job::Mutation` can't carry an ack — add
  `Job::Call { to: ServiceId, body: Box<ContentBody>, at: u64, done: tokio::sync::oneshot::Sender<bool> }`
  handled like Mutation but reporting the outcome)
- Modify: `core/sig-core/src/client.rs` (uniffi API)

**Interfaces:**
- Consumes: Tasks 1–5.
- Produces (client.rs, uniffi):

```rust
pub fn start_call(&self, conversation_id: String) -> Result<(), CoreError>  // c:<aci> only
pub fn accept_call(&self) -> Result<(), CoreError>
pub fn hangup_call(&self) -> Result<(), CoreError>
pub fn set_call_muted(&self, muted: bool) -> Result<(), CoreError>
pub async fn call_audio_devices(&self) -> Result<AudioDevices, CoreError>
pub fn set_call_audio_input(&self, id: String) -> Result<(), CoreError>
pub fn set_call_audio_output(&self, id: String) -> Result<(), CoreError>
```

Wiring map:
- The core actor spawns the CallActor lazily on first use (link status needed: device id from
  `m.registration_data()`), keeps `call_tx`, and a `spawn_local` task draining `out_rx`:
  - `CallOutput::Send` → `Job::Call` via send_tx with the peer's `ServiceId` (parse ACI uuid;
    `send::thread`-style), `at = next unique ms timestamp`; on the oneshot's outcome send
    `CallCommand::SendResult` back. The sender implements `Job::Call` exactly like `Mutation`
    (`send_message(recipient, body, at)`) but resolves the oneshot with ok/err; **urgency**:
    pass `urgent` into presage's send if its `send_message` takes it (check signature; if not,
    drop the flag).
  - `CallOutput::NeedRelays` → async fetch via `calls::relays::fetch(push_service(data))`,
    then `CallCommand::Relays(servers)` (empty on failure + one Core Log error).
  - `CallOutput::ChatEvent` → `db.add_chat_event(&format!("c:{aci}"), now, kind, &peer_aci)`
    (+ value variant) + `ChatEventAdded` + `ConversationsChanged` events.
- Incoming: in `handle()`'s `Received::Content` arm, before `project()`: if
  `ContentBody::CallMessage(cm)` → build `CallCommand::Incoming` with
  `sender_aci = metadata.sender` (ACI only — a PNI sender is logged and dropped),
  `sender_device_id = u32::from(metadata.sender_device)`,
  `age_secs = max(0, now - server_timestamp)/1000`, identity keys from presage's protocol
  store: ours `store.aci_protocol_store().get_identity_key_pair().await?.public_key().serialize()`,
  theirs `store.aci_protocol_store().get_identity(&ProtocolAddress::new(sender.service_id_string(), device_id)).await?`
  (None → drop with a warn: no session, no call), `calls = wire::from_proto(&cm)`. Then send
  to `call_tx`. (Check exact trait paths: presage re-exports
  `libsignal_service::protocol::{ProtocolAddress, IdentityKeyStore}`; the aci protocol store
  is what `link`/`receive` already use.)
- Unlink/Relink: drop the call channel (hangs up by thread death — acceptable; note it).

- [ ] **Step 1: Failing tests.** The actor glue needs a linked manager, so test what's pure:
  the PNI/unknown-sender guard and the ack plumbing as free functions where possible. Write
  `fn call_sender_aci(metadata: &Metadata) -> Option<String>` in calls/mod.rs returning the
  bare uuid for ACI senders, None for PNI:

```rust
#[test]
fn only_aci_senders_may_ring() {
    use presage::libsignal_service::protocol::{Aci, Pni, ServiceId};
    let u = uuid();
    assert_eq!(call_sender_aci(&ServiceId::from(Aci::from(u))), Some(u.to_string()));
    assert_eq!(call_sender_aci(&ServiceId::from(Pni::from(u))), None);
}
```

  And a sender test in send.rs's existing test style: a `Job::Call` resolves its oneshot false
  when sending fails (unlinked manager path) — mirror how existing sender tests fake failure;
  if none exist for Mutation, test at the `client.rs` level: `start_call` on an unlinked
  client → `Err(NotLinked)`; on a linked-but-fake path, skip (phone test covers it).

- [ ] **Step 2: RED → implement → GREEN**; client API + input validation (`start_call` demands
  `c:<aci>` — reuse `send::recipient` + the PNI check from `reset_session`).
- [ ] **Step 3:** client tests: `start_call` unlinked → NotLinked; bad conversation id →
  InvalidInput. Suite green.
- [ ] **Step 4: Commit** — `git commit -am "Calls flow through the core: signaling, relays and chat events"`

---

### Task 7: Bindings and FakeClient

- [ ] **Step 1:** `make core && make project` (regenerates uniffi bindings with CallPhase,
  AudioDevices, the new Client methods, CoreEvent.callChanged).
- [ ] **Step 2:** FakeClient grows: `startCallCalls: [String]`, `acceptCallCalls`,
  `hangupCallCalls`, `setMutedCalls: [Bool]`, `audioDevicesAnswer: AudioDevices`,
  `setInputCalls/setOutputCalls: [String]`, each recording like the existing methods; throwing
  variants via `var callError: Error?`.
- [ ] **Step 3:** Swift build of the test target compiles (`xcodebuild … build-for-testing
  CODE_SIGNING_ALLOWED=NO`). Commit — `git commit -am "The app sees the call API"`

---

### Task 8: The app: call window, ringing, chat button, timeline rows

**Files:**
- Modify: `app/Semaphore/AppStore.swift` (call state machine)
- Create: `app/Semaphore/CallWindow.swift`
- Modify: `app/Semaphore/SemaphoreApp.swift` (Window scene + notification categories)
- Modify: `app/Semaphore/Notifications.swift` (call category with Accept/Decline actions)
- Modify: `app/Semaphore/ConversationView.swift` (phone button in direct chats)
- Modify: `app/Semaphore/Timeline.swift` + `app/Semaphore/Placeholder.swift` (call event rows:
  "Outgoing voice call (2 min)", "Incoming voice call (2 min)", "Missed voice call")
- Modify: `app/project.yml` (`NSMicrophoneUsageDescription`: "Semaphore uses the microphone
  for voice calls." — check how Info.plist keys are declared; `app/Semaphore/Info.plist`
  exists, add there)
- Create: `app/SemaphoreTests/AppStoreCallTests.swift`

**Interfaces (AppStore):**

```swift
struct ActiveCall: Equatable {
    var conversationId: String
    var title: String          // peer display name
    var phase: CallPhase
    var connectedAt: Date?     // drives the duration ticker
    var muted: Bool
    var endedReason: String?
}
private(set) var call: ActiveCall?    // non-nil: the call window is shown
func startCall(_ conversationId: String)
func acceptCall()
func hangupCall()          // also declines an incoming ring
func toggleCallMuted()
```

Behavior (each a test):
- `.callChanged(conversationId, phase, reason)`:
  - phase != .ended → upsert `call` (title from conversations/titles like notifications do),
    `connectedAt` set on first .connected;
  - .ended → keep `call` with `endedReason` for 2 s then nil (tests: set then event ended →
    `call?.phase == .ended`; a dismiss timer is UI polish — in tests, `dismissEndedCall()`
    is callable directly).
- Incoming phase while app inactive → notification (category "call") with Accept/Decline;
  `NotificationDelegate` routes `accept` → `acceptCall()`, `decline` → `hangupCall()`.
  Opening a chat removes none of this (calls aren't messages).
- An incoming call that ends without ever connecting posts a "Missed call" notification (the
  spec's missed-call notification), whatever the app's focus; test: `.ended` after `.incoming`
  with no `.connected` → notifier records one missed-call notification.
- `startCall` guards: only direct non-PNI chats, no active call.
- Mute toggles call `setCallMuted` and flips local state optimistically.

CallWindow (SwiftUI):
- `Window("Call", id: "call")` scene shown/raised when `core.call != nil` (ContentView `.task`
  on change calls `openWindow(id: "call")`; closing the window while connected hangs up —
  `.onDisappear` guard).
- Layout: peer initials avatar + title; state line (Ringing…/Connecting…/duration ticker/
  ended reason); buttons: mute toggle (mic.slash), audio device menu (two `Menu`s fed by
  `call_audio_devices`), red hangup. Incoming ring state shows green Accept + red Decline.

- [ ] **Step 1:** failing AppStoreCallTests (event upserts, ended keeps reason, startCall
  guards, accept/decline plumbing to FakeClient, mute optimistic flip, notification trigger
  when inactive — follow AppStoreHistoryTests' style with `isAppActive` injection).
- [ ] **Step 2:** RED (unsigned xcodebuild, `-only-testing:SemaphoreTests/AppStoreCallTests`).
- [ ] **Step 3:** implement AppStore + views + plist + notification category.
- [ ] **Step 4:** GREEN; full Swift suite green.
- [ ] **Step 5:** `make project` diff check (project.yml changed? regenerate; never commit the
  xcodeproj), build the app ad-hoc and open it: the chat header shows the phone button in a
  direct chat; the call window opens on `startCall` (it will fail to connect without a phone —
  states still render).
- [ ] **Step 6: Commit** — `git commit -am "The app rings, answers and shows a call window"`

---

### Task 9: Gate, PR, review

- [ ] **Step 1:** Update the spec where reality diverged (actor-thread design, ack-driven
  `should_assume_messages_sent=false`, device-picker shape).
- [ ] **Step 2:** `make format`; Rust suite + unsigned Swift suite green; `make lint`.
- [ ] **Step 3:** Push, `gh pr create` — title "Semaphore makes and takes 1:1 voice calls";
  body: what/why/how tested, ending `Part of #19` (not `Closes`: #19 is the M3 epic and the
  owner staged it — video and group calls follow in their own PRs; say so in the body).
- [ ] **Step 4:** The card stays In progress (the epic is mid-way); say so in chat.
- [ ] **Step 5:** Run `/pr-review-toolkit:review-pr` (code-reviewer, silent-failure-hunter,
  pr-test-analyzer) on `git diff origin/main...HEAD`; fix Critical/Important with tests; post
  the PR comment with findings and rulings.
- [ ] **Step 6:** Report in chat; the owner's phone test list: outgoing call both ways,
  incoming accept/decline, hangup each side, busy (call while in a call), mute audible check,
  device switch mid-call, mic permission prompt on first call.
