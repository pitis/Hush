//! The call engine's thread: owns RingRTC's `CallManager` and `PeerConnectionFactory`, driven
//! by `CallCommand`s, reporting through the `EventListener` and `CallOutput`s. RingRTC calls
//! back from its own worker threads; nothing here touches presage.

use std::sync::{Arc, Mutex};

use ringrtc::common::{CallConfig, CallId, CallMediaType};
use ringrtc::core::call_manager::CallManager;
use ringrtc::core::signaling;
use ringrtc::lite::http;
use ringrtc::native::{
    CallState, CallStateHandler, GroupUpdateHandler, NativeCallContext, NativePlatform, SignalingSender,
};
use ringrtc::webrtc::media::{AudioTrack, VideoSink, VideoTrack};
use ringrtc::webrtc::peer_connection_factory::{AudioConfig, IceServer, PeerConnectionFactory};
use ringrtc::webrtc::peer_connection_observer::NetworkRoute;

use crate::calls::state::{map_state, ActiveCall};
use crate::calls::wire;
use crate::events::{CoreEvent, EventListener};
use crate::projection::ChatEventKind;

/// Commands from the core actor (and through it, the app).
pub(crate) enum CallCommand {
    /// Start a voice call to this ACI (bare uuid string).
    Start {
        peer_aci: String,
    },
    /// Accept the ringing incoming call (held until RingRTC can take it).
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
        calls: Vec<wire::IncomingCall>,
    },
    /// The relays answer for a pending proceed (possibly empty: direct ICE still tries). Tagged
    /// with the call it was fetched for, so a late answer can't proceed a later call.
    Relays {
        call_id_u64: u64,
        servers: Vec<IceServer>,
    },
    /// The send outcome of an outgoing envelope.
    SendResult {
        call_id_u64: u64,
        ok: bool,
    },
    ListAudioDevices {
        reply: tokio::sync::oneshot::Sender<AudioDevices>,
    },
    /// Select a device by its unique id.
    SetAudioInput(String),
    SetAudioOutput(String),
}

/// What the call thread asks the core actor to do.
pub(crate) enum CallOutput {
    /// Send this envelope to the peer, then answer with `SendResult`.
    Send {
        peer_aci: String,
        call_id_u64: u64,
        proto: presage::proto::CallMessage,
    },
    /// Fetch relays and answer with `CallCommand::Relays` for this call.
    NeedRelays { call_id_u64: u64 },
    /// Write a chat event into the peer's conversation.
    ChatEvent {
        peer_aci: String,
        kind: ChatEventKind,
        value: Option<i64>,
    },
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct AudioDevices {
    pub inputs: Vec<AudioDevice>,
    pub outputs: Vec<AudioDevice>,
    pub current_input: Option<String>,
    pub current_output: Option<String>,
}

/// Call state shared between the thread and RingRTC's callbacks.
#[derive(Default)]
struct Shared {
    active: Option<ActiveCall>,
    /// A Start was handed to RingRTC and its Outgoing state hasn't come back yet: a second
    /// Start meanwhile would make RingRTC kill the first call as "already in progress".
    starting: bool,
    /// An incoming call can be accepted only once RingRTC says it rings locally.
    ringing: bool,
    /// The user accepted before that: taken as soon as it rings.
    accept_pending: bool,
}

/// A panic in one callback must not take the whole engine down with a poisoned lock.
fn lock(shared: &Mutex<Shared>) -> std::sync::MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

/// Spawns the call thread. Dropping the returned sender ends it (and closes the engine).
pub(crate) fn spawn(
    our_device_id: u32,
    listener: Arc<dyn EventListener>,
    out_tx: tokio::sync::mpsc::UnboundedSender<CallOutput>,
) -> std::sync::mpsc::Sender<CallCommand> {
    let (tx, rx) = std::sync::mpsc::channel::<CallCommand>();
    let commands = tx.clone();
    std::thread::Builder::new()
        .name("sig-calls".into())
        .spawn(move || run(our_device_id, listener, out_tx, commands, rx))
        .expect("spawn the call thread");
    tx
}

/// The engine, created on first use (it opens the audio stack).
struct Engine {
    call_manager: CallManager<NativePlatform>,
    pcf: PeerConnectionFactory,
    audio_track: AudioTrack,
    video_track: VideoTrack,
}

fn run(
    our_device_id: u32,
    listener: Arc<dyn EventListener>,
    out_tx: tokio::sync::mpsc::UnboundedSender<CallOutput>,
    commands: std::sync::mpsc::Sender<CallCommand>,
    rx: std::sync::mpsc::Receiver<CallCommand>,
) {
    let shared = Arc::new(Mutex::new(Shared::default()));
    let mut engine: Option<Engine> = None;
    while let Ok(cmd) = rx.recv() {
        // Lazy init: the audio stack opens on first use, not at app start.
        if engine.is_none()
            && matches!(
                cmd,
                CallCommand::Start { .. }
                    | CallCommand::Incoming { .. }
                    | CallCommand::ListAudioDevices { .. }
                    | CallCommand::SetAudioInput(_)
                    | CallCommand::SetAudioOutput(_)
            )
        {
            match init_engine(&shared, &listener, &out_tx, &commands) {
                Ok(e) => engine = Some(e),
                Err(e) => {
                    listener.on_event(CoreEvent::Error {
                        message: format!("the call engine couldn't start: {e}"),
                    });
                    continue;
                }
            }
        }
        let Some(engine) = engine.as_mut() else { continue };
        match cmd {
            CallCommand::Start { peer_aci } => {
                {
                    let mut shared = lock(&shared);
                    if shared.active.is_some() || shared.starting {
                        listener.on_event(CoreEvent::Error {
                            message: "already in a call".into(),
                        });
                        continue;
                    }
                    shared.starting = true;
                }
                // a call never starts muted, whatever the last one ended as
                engine.audio_track.set_enabled(true);
                engine.call_manager.call(peer_aci, CallMediaType::Audio, our_device_id);
            }
            CallCommand::Accept => {
                let mut shared = lock(&shared);
                match shared.active.clone() {
                    Some(active) if active.incoming && shared.ringing => {
                        engine.call_manager.accept_call(CallId::new(active.call_id_u64));
                    }
                    // RingRTC takes an accept only once the call rings locally: hold it
                    Some(active) if active.incoming => shared.accept_pending = true,
                    _ => {}
                }
            }
            CallCommand::Hangup => {
                if lock(&shared).active.is_some() {
                    engine.call_manager.hangup();
                }
            }
            CallCommand::SetMuted(muted) => {
                engine.audio_track.set_enabled(!muted);
            }
            CallCommand::Incoming {
                sender_aci,
                sender_device_id,
                age_secs,
                sender_identity_key,
                receiver_identity_key,
                calls,
            } => {
                for incoming in calls {
                    let call_id = incoming.call_id;
                    match incoming.message {
                        signaling::Message::Offer(offer) => engine.call_manager.received_offer(
                            sender_aci.clone(),
                            call_id,
                            signaling::ReceivedOffer {
                                offer,
                                age: std::time::Duration::from_secs(age_secs),
                                sender_device_id,
                                receiver_device_id: our_device_id,
                                sender_identity_key: sender_identity_key.clone(),
                                receiver_identity_key: receiver_identity_key.clone(),
                            },
                        ),
                        signaling::Message::Answer(answer) => engine.call_manager.received_answer(
                            sender_aci.clone(),
                            call_id,
                            signaling::ReceivedAnswer {
                                answer,
                                sender_device_id,
                                sender_identity_key: sender_identity_key.clone(),
                                receiver_identity_key: receiver_identity_key.clone(),
                            },
                        ),
                        signaling::Message::Ice(ice) => engine.call_manager.received_ice(
                            sender_aci.clone(),
                            call_id,
                            signaling::ReceivedIce { ice, sender_device_id },
                        ),
                        signaling::Message::Hangup(hangup) => engine.call_manager.received_hangup(
                            sender_aci.clone(),
                            call_id,
                            signaling::ReceivedHangup {
                                hangup,
                                sender_device_id,
                            },
                        ),
                        signaling::Message::Busy => engine.call_manager.received_busy(
                            sender_aci.clone(),
                            call_id,
                            signaling::ReceivedBusy { sender_device_id },
                        ),
                    }
                }
            }
            CallCommand::Relays { call_id_u64, servers } => {
                let active = lock(&shared).active.clone();
                let Some(active) = active else { continue };
                if active.call_id_u64 != call_id_u64 {
                    continue; // fetched for a call that is already over
                }
                engine.audio_track.set_enabled(true);
                let context = NativeCallContext::new(
                    // hide_ip=false is Signal's own default: a 1:1 call may connect peer-to-peer,
                    // which shows your IP to your contact. The "Always relay calls" privacy
                    // setting (which sets this true and forces TURN) is planned M4 settings work.
                    false,
                    servers,
                    engine.audio_track.clone(),
                    engine.video_track.clone(),
                    Box::new(NoVideo),
                );
                engine
                    .call_manager
                    .proceed(CallId::new(call_id_u64), context, CallConfig::default(), None);
            }
            CallCommand::SendResult { call_id_u64, ok } => {
                if ok {
                    engine.call_manager.message_sent(CallId::new(call_id_u64));
                } else {
                    engine.call_manager.message_send_failure(CallId::new(call_id_u64));
                }
            }
            CallCommand::ListAudioDevices { reply } => {
                let _ = reply.send(list_devices(&mut engine.pcf));
            }
            CallCommand::SetAudioInput(id) => {
                if let Err(e) = engine.pcf.set_audio_recording_device_by_id(&id) {
                    listener.on_event(CoreEvent::Error {
                        message: format!("switching the microphone failed: {e}"),
                    });
                }
            }
            CallCommand::SetAudioOutput(id) => {
                if let Err(e) = engine.pcf.set_audio_playout_device_by_id(&id) {
                    listener.on_event(CoreEvent::Error {
                        message: format!("switching the speaker failed: {e}"),
                    });
                }
            }
        }
    }
    // the core is going away (unlink, relink): end whatever call is up with it
    if let Some(mut engine) = engine {
        if let Err(e) = engine.call_manager.close() {
            tracing::warn!("closing the call engine failed: {e}");
        }
    }
    tracing::info!("the call thread ended");
}

fn init_engine(
    shared: &Arc<Mutex<Shared>>,
    listener: &Arc<dyn EventListener>,
    out_tx: &tokio::sync::mpsc::UnboundedSender<CallOutput>,
    commands: &std::sync::mpsc::Sender<CallCommand>,
) -> Result<Engine, String> {
    let pcf = PeerConnectionFactory::new(&AudioConfig::default(), false, "", None).map_err(|e| e.to_string())?;
    let audio_track = pcf.create_outgoing_audio_track().map_err(|e| e.to_string())?;
    let video_source = pcf.create_outgoing_video_source().map_err(|e| e.to_string())?;
    let video_track = pcf
        .create_outgoing_video_track(&video_source)
        .map_err(|e| e.to_string())?;
    // voice only: the camera never turns on
    video_track.set_enabled(false);
    let bridge = Bridge {
        shared: shared.clone(),
        listener: listener.clone(),
        out_tx: out_tx.clone(),
        commands: commands.clone(),
    };
    let platform = NativePlatform::new(
        pcf.clone(),
        Box::new(bridge.clone()),
        false, // sends are acked through SendResult
        Box::new(bridge.clone()),
        Box::new(bridge.clone()),
    );
    let call_manager = CallManager::new(platform, http::DelegatingClient::new(NoHttp)).map_err(|e| e.to_string())?;
    Ok(Engine {
        call_manager,
        pcf,
        audio_track,
        video_track,
    })
}

fn list_devices(pcf: &mut PeerConnectionFactory) -> AudioDevices {
    let map = |devices: Vec<ringrtc::webrtc::peer_connection_factory::AudioDevice>| {
        devices
            .into_iter()
            .map(|d| AudioDevice {
                id: d.unique_id,
                name: d.name,
            })
            .collect::<Vec<_>>()
    };
    let inputs = pcf
        .get_audio_recording_devices()
        .inspect_err(|e| tracing::warn!("listing microphones failed: {e}"))
        .map(map)
        .unwrap_or_default();
    let outputs = pcf
        .get_audio_playout_devices()
        .inspect_err(|e| tracing::warn!("listing speakers failed: {e}"))
        .map(map)
        .unwrap_or_default();
    AudioDevices {
        current_input: inputs.first().map(|d| d.id.clone()),
        current_output: outputs.first().map(|d| d.id.clone()),
        inputs,
        outputs,
    }
}

/// RingRTC's callbacks (its worker threads): map and forward, never block, never touch presage,
/// never call back into the engine (that goes through `commands`, handled on the call thread).
#[derive(Clone)]
struct Bridge {
    shared: Arc<Mutex<Shared>>,
    listener: Arc<dyn EventListener>,
    out_tx: tokio::sync::mpsc::UnboundedSender<CallOutput>,
    commands: std::sync::mpsc::Sender<CallCommand>,
}

impl Bridge {
    fn now_ms() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    /// One state change of the call with `remote_peer_id`, at `now`.
    fn state_changed(&self, remote_peer_id: &str, call_id: CallId, state: CallState, now: i64) {
        // ids, states and reasons only: this is what a failed call leaves to debug with
        match &state {
            CallState::Ended(reason, _) => tracing::warn!(call = %call_id, ?reason, "call ended"),
            CallState::Rejected(reason) => tracing::warn!(call = %call_id, ?reason, "offer rejected"),
            other => tracing::info!(call = %call_id, state = %other, "call state"),
        }
        let mut shared = lock(&self.shared);
        // an offer that couldn't start a call (expired while we were away, or we were busy)
        // still was a call we missed; it needs no active call to be recorded
        if let CallState::Rejected(_) = state {
            let _ = self.out_tx.send(CallOutput::ChatEvent {
                peer_aci: remote_peer_id.to_owned(),
                kind: ChatEventKind::MissedCall,
                value: None,
            });
            return;
        }
        // the first we hear of a call: remember it
        if matches!(state, CallState::Incoming(_) | CallState::Outgoing(_)) && shared.active.is_none() {
            *shared = Shared {
                active: Some(ActiveCall {
                    call_id_u64: call_id.as_u64(),
                    incoming: matches!(state, CallState::Incoming(_)),
                    connected_at: None,
                }),
                starting: false,
                ringing: false,
                accept_pending: false,
            };
        }
        let Some(active) = shared.active.clone() else {
            // a call that failed before it started (RingRTC's own refusal): nothing is shown,
            // so say why
            if let CallState::Ended(reason, _) = &state {
                shared.starting = false;
                drop(shared);
                self.listener.on_event(CoreEvent::Error {
                    message: format!("the call couldn't start: {reason:?}"),
                });
            }
            return;
        };
        if active.call_id_u64 != call_id.as_u64() {
            return; // a stale state of a call that is already over
        }
        if matches!(state, CallState::Connected) {
            if let Some(active) = shared.active.as_mut() {
                active.connected_at.get_or_insert(now);
            }
        }
        // an incoming call can be accepted from now on; take an accept the user already gave
        if matches!(state, CallState::Ringing) && active.incoming {
            shared.ringing = true;
            if std::mem::take(&mut shared.accept_pending) {
                let _ = self.commands.send(CallCommand::Accept);
            }
        }
        let mapped = map_state(&active, &state, now);
        if matches!(state, CallState::Incoming(_) | CallState::Outgoing(_)) {
            let _ = self.out_tx.send(CallOutput::NeedRelays {
                call_id_u64: call_id.as_u64(),
            });
        }
        if let Some((kind, value)) = mapped.chat_event {
            let _ = self.out_tx.send(CallOutput::ChatEvent {
                peer_aci: remote_peer_id.to_owned(),
                kind,
                value,
            });
        }
        if mapped.call_over {
            *shared = Shared::default();
        }
        drop(shared);
        if let Some((phase, reason)) = mapped.phase {
            self.listener.on_event(CoreEvent::CallChanged {
                conversation_id: format!("c:{remote_peer_id}"),
                phase,
                reason,
            });
        }
    }
}

impl SignalingSender for Bridge {
    fn send_signaling(
        &self,
        recipient_id: &str,
        call_id: CallId,
        receiver_device_id: Option<ringrtc::common::DeviceId>,
        message: signaling::Message,
    ) -> ringrtc::common::Result<()> {
        let proto = wire::to_proto(call_id, receiver_device_id, &message);
        // a closed router means the core is gone: fail the call now rather than at the timeout
        self.out_tx
            .send(CallOutput::Send {
                peer_aci: recipient_id.to_owned(),
                call_id_u64: call_id.as_u64(),
                proto,
            })
            .map_err(|_| std::io::Error::other("the call router is gone").into())
    }

    fn send_call_message(
        &self,
        _recipient_id: ringrtc::lite::sfu::UserId,
        _message: Vec<u8>,
        _urgency: ringrtc::core::group_call::SignalingMessageUrgency,
    ) -> ringrtc::common::Result<()> {
        tracing::warn!("group call messages aren't supported yet");
        Ok(())
    }

    fn send_call_message_to_group(
        &self,
        _group_id: ringrtc::core::group_call::GroupId,
        _message: Vec<u8>,
        _urgency: ringrtc::core::group_call::SignalingMessageUrgency,
        _recipients_override: std::collections::HashSet<ringrtc::lite::sfu::UserId>,
    ) -> ringrtc::common::Result<()> {
        tracing::warn!("group calls aren't supported yet");
        Ok(())
    }

    fn send_call_message_to_adhoc_group(
        &self,
        _message: Vec<u8>,
        _urgency: ringrtc::core::group_call::SignalingMessageUrgency,
        _expiration: u64,
        _recipients_to_endorsements: std::collections::HashMap<ringrtc::lite::sfu::UserId, Vec<u8>>,
    ) -> ringrtc::common::Result<()> {
        tracing::warn!("call links aren't supported yet");
        Ok(())
    }
}

impl CallStateHandler for Bridge {
    fn handle_call_state(
        &self,
        remote_peer_id: &str,
        call_id: CallId,
        state: CallState,
    ) -> ringrtc::common::Result<()> {
        self.state_changed(remote_peer_id, call_id, state, Self::now_ms());
        Ok(())
    }

    fn handle_remote_audio_state(&self, _remote_peer_id: &str, _enabled: bool) -> ringrtc::common::Result<()> {
        Ok(())
    }

    fn handle_remote_video_state(&self, _remote_peer_id: &str, _enabled: bool) -> ringrtc::common::Result<()> {
        Ok(())
    }

    fn handle_remote_sharing_screen(&self, _remote_peer_id: &str, _enabled: bool) -> ringrtc::common::Result<()> {
        Ok(())
    }

    fn handle_network_route(&self, _remote_peer_id: &str, _network_route: NetworkRoute) -> ringrtc::common::Result<()> {
        Ok(())
    }

    fn handle_audio_levels(
        &self,
        _remote_peer_id: &str,
        _captured_level: ringrtc::webrtc::peer_connection::AudioLevel,
        _received_level: ringrtc::webrtc::peer_connection::AudioLevel,
    ) -> ringrtc::common::Result<()> {
        Ok(())
    }

    fn handle_low_bandwidth_for_video(&self, _remote_peer_id: &str, _recovered: bool) -> ringrtc::common::Result<()> {
        Ok(())
    }
}

impl GroupUpdateHandler for Bridge {
    fn handle_group_update(&self, _update: ringrtc::native::GroupUpdate) -> ringrtc::common::Result<()> {
        tracing::warn!("group calls aren't supported yet");
        Ok(())
    }
}

/// Voice only: incoming frames are dropped.
#[derive(Clone)]
struct NoVideo;

impl VideoSink for NoVideo {
    fn on_video_frame(&self, _demux_id: u32, _frame: ringrtc::webrtc::media::VideoFrame) {}
    fn box_clone(&self) -> Box<dyn VideoSink> {
        Box::new(NoVideo)
    }
}

/// Group calls only; unused for 1:1.
struct NoHttp;

impl http::Delegate for NoHttp {
    fn send_request(&self, _request_id: u32, _request: http::Request) {
        tracing::warn!("a group-call http request was dropped (group calls aren't supported yet)");
    }
}

#[cfg(test)]
mod tests {
    use ringrtc::common::{CallEndReason, CallMediaType};
    use ringrtc::core::call_summary::CallSummary;
    use ringrtc::native::RejectReason;

    use super::*;
    use crate::calls::{CallEnd, CallPhase};

    struct Quiet;
    impl EventListener for Quiet {
        fn on_event(&self, _: CoreEvent) {}
    }

    /// Proves PCF + cubeb initialize inside sig-core (the spike proved it standalone). The
    /// device cache fills asynchronously on cubeb's worker thread, so poll briefly — the app's
    /// device menus open long after init.
    #[test]
    fn the_call_actor_spawns_and_lists_devices() {
        let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = spawn(1, Arc::new(Quiet), out_tx);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let (reply, rx) = tokio::sync::oneshot::channel();
            tx.send(CallCommand::ListAudioDevices { reply }).unwrap();
            let devices = rx.blocking_recv().unwrap();
            if !devices.outputs.is_empty() {
                return; // this development Mac always has at least one output
            }
            assert!(std::time::Instant::now() < deadline, "no audio devices in 5s");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    // The Bridge is RingRTC's view of us; it is plain state plus channels, so its sequencing is
    // tested without the engine.

    struct Recorder(Mutex<Vec<CoreEvent>>);
    impl EventListener for Recorder {
        fn on_event(&self, e: CoreEvent) {
            self.0.lock().unwrap().push(e);
        }
    }

    struct Harness {
        bridge: Bridge,
        events: Arc<Recorder>,
        out_rx: tokio::sync::mpsc::UnboundedReceiver<CallOutput>,
        commands_rx: std::sync::mpsc::Receiver<CallCommand>,
    }

    fn harness() -> Harness {
        let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (commands, commands_rx) = std::sync::mpsc::channel();
        let events = Arc::new(Recorder(Mutex::new(vec![])));
        let bridge = Bridge {
            shared: Arc::new(Mutex::new(Shared::default())),
            listener: events.clone(),
            out_tx,
            commands,
        };
        Harness {
            bridge,
            events,
            out_rx,
            commands_rx,
        }
    }

    const ALICE: &str = "22222222-2222-2222-2222-222222222222";
    const BOB: &str = "33333333-3333-3333-3333-333333333333";

    fn phases(h: &Harness) -> Vec<(CallPhase, Option<CallEnd>)> {
        h.events
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                CoreEvent::CallChanged { phase, reason, .. } => Some((*phase, *reason)),
                _ => None,
            })
            .collect()
    }

    fn ended(reason: CallEndReason) -> CallState {
        CallState::Ended(reason, CallSummary::default())
    }

    #[test]
    fn an_incoming_call_is_remembered_and_asks_for_relays() {
        let mut h = harness();
        h.bridge
            .state_changed(ALICE, CallId::new(7), CallState::Incoming(CallMediaType::Audio), 1_000);
        let active = h.bridge.shared.lock().unwrap().active.clone().unwrap();
        assert_eq!(
            (active.call_id_u64, active.incoming, active.connected_at),
            (7, true, None)
        );
        assert!(matches!(
            h.out_rx.try_recv(),
            Ok(CallOutput::NeedRelays { call_id_u64: 7 })
        ));
        assert_eq!(phases(&h), vec![(CallPhase::Incoming, None)]);
    }

    /// Offers that sat in the queue while we were away arrive expired: no call starts, but the
    /// chat still shows the missed call.
    #[test]
    fn an_expired_offer_is_a_missed_call() {
        let mut h = harness();
        h.bridge.state_changed(
            BOB,
            CallId::new(5),
            CallState::Rejected(RejectReason::ReceivedOfferExpired {
                age: std::time::Duration::from_secs(120),
            }),
            1_000,
        );
        match h.out_rx.try_recv() {
            Ok(CallOutput::ChatEvent { peer_aci, kind, value }) => {
                assert_eq!((peer_aci.as_str(), kind, value), (BOB, ChatEventKind::MissedCall, None));
            }
            other => panic!("expected a missed-call event, got {}", other.is_ok()),
        }
        assert!(h.bridge.shared.lock().unwrap().active.is_none());
        assert!(phases(&h).is_empty(), "nothing to show");
    }

    #[test]
    fn a_rejected_offer_while_busy_is_missed_and_leaves_the_active_call_alone() {
        let mut h = harness();
        h.bridge
            .state_changed(ALICE, CallId::new(7), CallState::Incoming(CallMediaType::Audio), 1_000);
        let _ = h.out_rx.try_recv();
        h.bridge.state_changed(
            BOB,
            CallId::new(8),
            CallState::Rejected(RejectReason::ReceivedOfferWhileActive),
            2_000,
        );
        match h.out_rx.try_recv() {
            Ok(CallOutput::ChatEvent { peer_aci, kind, .. }) => {
                assert_eq!((peer_aci.as_str(), kind), (BOB, ChatEventKind::MissedCall));
            }
            _ => panic!("expected bob's missed call"),
        }
        assert_eq!(h.bridge.shared.lock().unwrap().active.as_ref().unwrap().call_id_u64, 7);
        assert_eq!(phases(&h).len(), 1, "no phase change for the active call");
    }

    /// An accept given before RingRTC can take it is replayed the moment the call rings.
    #[test]
    fn an_early_accept_is_taken_when_the_call_rings() {
        let h = harness();
        h.bridge
            .state_changed(ALICE, CallId::new(7), CallState::Incoming(CallMediaType::Audio), 1_000);
        h.bridge.shared.lock().unwrap().accept_pending = true; // what the thread does for an early Accept
        h.bridge.state_changed(ALICE, CallId::new(7), CallState::Ringing, 1_500);
        assert!(matches!(h.commands_rx.try_recv(), Ok(CallCommand::Accept)));
        assert!(h.bridge.shared.lock().unwrap().ringing);
    }

    /// RingRTC refusing a call before it exists (busy engine, internal error) would otherwise
    /// be invisible: the window never opened, so the user needs to hear why.
    #[test]
    fn a_call_that_fails_to_start_is_reported() {
        let h = harness();
        h.bridge.shared.lock().unwrap().starting = true;
        h.bridge
            .state_changed(ALICE, CallId::new(7), ended(CallEndReason::InternalFailure), 1_000);
        let said = h
            .events
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, CoreEvent::Error { message } if message.contains("couldn't start")));
        assert!(said);
        assert!(!h.bridge.shared.lock().unwrap().starting, "a new Start may follow");
    }

    #[test]
    fn the_duration_counts_from_the_first_connect() {
        let h = harness();
        h.bridge
            .state_changed(ALICE, CallId::new(7), CallState::Outgoing(CallMediaType::Audio), 1_000);
        h.bridge
            .state_changed(ALICE, CallId::new(7), CallState::Connected, 5_000);
        h.bridge
            .state_changed(ALICE, CallId::new(7), CallState::Connected, 9_000);
        assert_eq!(
            h.bridge.shared.lock().unwrap().active.as_ref().unwrap().connected_at,
            Some(5_000)
        );
    }

    #[test]
    fn a_stale_end_of_another_call_changes_nothing() {
        let mut h = harness();
        h.bridge
            .state_changed(ALICE, CallId::new(7), CallState::Incoming(CallMediaType::Audio), 1_000);
        let _ = h.out_rx.try_recv();
        h.bridge
            .state_changed(BOB, CallId::new(9), ended(CallEndReason::RemoteHangup), 2_000);
        assert!(h.out_rx.try_recv().is_err(), "no chat event for a foreign call");
        assert_eq!(phases(&h).len(), 1);
        assert!(h.bridge.shared.lock().unwrap().active.is_some());
    }

    #[test]
    fn the_active_call_ending_clears_it_and_reports_why() {
        let mut h = harness();
        h.bridge
            .state_changed(ALICE, CallId::new(7), CallState::Incoming(CallMediaType::Audio), 1_000);
        let _ = h.out_rx.try_recv();
        h.bridge
            .state_changed(ALICE, CallId::new(7), ended(CallEndReason::RemoteHangupAccepted), 3_000);
        assert!(
            h.out_rx.try_recv().is_err(),
            "the phone took it: nothing to record here"
        );
        assert!(h.bridge.shared.lock().unwrap().active.is_none());
        assert_eq!(
            phases(&h).last(),
            Some(&(CallPhase::Ended, Some(CallEnd::AnsweredElsewhere)))
        );
    }
}
