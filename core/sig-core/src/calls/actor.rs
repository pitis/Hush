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
    /// The relays answer for a pending proceed (possibly empty: direct ICE still tries).
    Relays(Vec<IceServer>),
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
        urgent: bool,
    },
    /// Fetch relays and answer with `CallCommand::Relays`.
    NeedRelays,
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
struct Shared {
    active: Option<ActiveCall>,
}

/// Spawns the call thread. Dropping the returned sender ends it.
pub(crate) fn spawn(
    our_device_id: u32,
    our_aci: String,
    listener: Arc<dyn EventListener>,
    out_tx: tokio::sync::mpsc::UnboundedSender<CallOutput>,
) -> std::sync::mpsc::Sender<CallCommand> {
    let (tx, rx) = std::sync::mpsc::channel::<CallCommand>();
    std::thread::Builder::new()
        .name("sig-calls".into())
        .spawn(move || run(our_device_id, our_aci, listener, out_tx, rx))
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
    our_aci: String,
    listener: Arc<dyn EventListener>,
    out_tx: tokio::sync::mpsc::UnboundedSender<CallOutput>,
    rx: std::sync::mpsc::Receiver<CallCommand>,
) {
    let shared = Arc::new(Mutex::new(Shared { active: None }));
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
            match init_engine(&shared, &listener, &out_tx) {
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
                if shared.lock().unwrap().active.is_some() {
                    listener.on_event(CoreEvent::Error {
                        message: "already in a call".into(),
                    });
                    continue;
                }
                engine.call_manager.call(peer_aci, CallMediaType::Audio, our_device_id);
            }
            CallCommand::Accept => {
                let active = shared.lock().unwrap().active.clone();
                if let Some(active) = active {
                    engine.call_manager.accept_call(CallId::new(active.call_id_u64));
                }
            }
            CallCommand::Hangup => {
                if shared.lock().unwrap().active.is_some() {
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
            CallCommand::Relays(ice_servers) => {
                let active = shared.lock().unwrap().active.clone();
                let Some(active) = active else { continue };
                let context = NativeCallContext::new(
                    false, // hide_ip: "always relay calls" is a later setting
                    ice_servers,
                    engine.audio_track.clone(),
                    engine.video_track.clone(),
                    Box::new(NoVideo),
                );
                engine
                    .call_manager
                    .proceed(CallId::new(active.call_id_u64), context, CallConfig::default(), None);
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
    tracing::info!("the call thread ended");
    let _ = our_aci;
}

fn init_engine(
    shared: &Arc<Mutex<Shared>>,
    listener: &Arc<dyn EventListener>,
    out_tx: &tokio::sync::mpsc::UnboundedSender<CallOutput>,
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

/// RingRTC's callbacks (its worker threads): map and forward, never block, never touch presage.
#[derive(Clone)]
struct Bridge {
    shared: Arc<Mutex<Shared>>,
    listener: Arc<dyn EventListener>,
    out_tx: tokio::sync::mpsc::UnboundedSender<CallOutput>,
}

impl Bridge {
    fn now_ms() -> i64 {
        chrono::Utc::now().timestamp_millis()
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
        let urgent = !matches!(message, signaling::Message::Ice(_));
        let proto = wire::to_proto(call_id, receiver_device_id, &message);
        let _ = self.out_tx.send(CallOutput::Send {
            peer_aci: recipient_id.to_owned(),
            call_id_u64: call_id.as_u64(),
            proto,
            urgent,
        });
        Ok(())
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
        let now = Self::now_ms();
        let mut shared = self.shared.lock().unwrap();
        // the first we hear of a call names the peer: remember it
        if matches!(state, CallState::Incoming(_) | CallState::Outgoing(_)) && shared.active.is_none() {
            shared.active = Some(ActiveCall {
                call_id_u64: call_id.as_u64(),
                peer: remote_peer_id.to_owned(),
                incoming: matches!(state, CallState::Incoming(_)),
                started_at: now,
                connected_at: None,
            });
        }
        if matches!(state, CallState::Connected) {
            if let Some(active) = shared.active.as_mut() {
                active.connected_at.get_or_insert(now);
            }
        }
        let Some(active) = shared.active.clone() else {
            return Ok(());
        };
        let mapped = map_state(&active, &state, now);
        // the engine needs relays before it can proceed with a fresh call
        if matches!(state, CallState::Incoming(_) | CallState::Outgoing(_)) {
            let _ = self.out_tx.send(CallOutput::NeedRelays);
        }
        if let Some((kind, value)) = mapped.chat_event {
            let _ = self.out_tx.send(CallOutput::ChatEvent {
                // a Rejected state belongs to the rejected caller, not the active call
                peer_aci: remote_peer_id.to_owned(),
                kind,
                value,
            });
        }
        if mapped.call_over && active.call_id_u64 == call_id.as_u64() {
            shared.active = None;
        }
        drop(shared);
        if let Some((phase, reason)) = mapped.phase {
            self.listener.on_event(CoreEvent::CallChanged {
                conversation_id: format!("c:{remote_peer_id}"),
                phase,
                reason,
            });
        }
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
    use super::*;

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
        let tx = spawn(1, "me".into(), Arc::new(Quiet), out_tx);
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
}
