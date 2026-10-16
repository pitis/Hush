//! What RingRTC's call states mean for us: the phase the app shows, and the chat event a
//! finished call leaves behind. Pure, so every rule is testable.

use ringrtc::common::CallEndReason;
use ringrtc::native::CallState;

use crate::calls::CallPhase;
use crate::projection::ChatEventKind;

/// Our bookkeeping for the one active call.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ActiveCall {
    pub call_id_u64: u64,
    /// The peer's ACI uuid string (the conversation is `c:<this>`).
    pub peer: String,
    pub incoming: bool,
    /// ms timestamps; `connected_at` None until Connected.
    pub started_at: i64,
    pub connected_at: Option<i64>,
}

/// What a RingRTC state change means for us.
#[derive(Debug, PartialEq)]
pub(crate) struct Mapped {
    /// The phase the app shows (None: nothing user-visible changed).
    pub phase: Option<(CallPhase, Option<String>)>,
    /// A chat event to write when a call concludes (kind, value = duration seconds).
    pub chat_event: Option<(ChatEventKind, Option<i64>)>,
    /// The active call is over and may be forgotten.
    pub call_over: bool,
}

pub(crate) fn map_state(active: &ActiveCall, state: &CallState, now_ms: i64) -> Mapped {
    match state {
        CallState::Incoming(_) => phase_only(CallPhase::Incoming),
        CallState::Outgoing(_) => phase_only(CallPhase::Outgoing),
        CallState::Ringing => phase_only(CallPhase::Ringing),
        CallState::Connecting => phase_only(CallPhase::Connecting),
        CallState::Connected => phase_only(CallPhase::Connected),
        CallState::Ended(reason, _) => {
            let duration = active
                .connected_at
                .map(|connected| ((now_ms - connected) / 1000).max(0));
            let chat_event = match (duration, active.incoming) {
                (Some(secs), true) => (ChatEventKind::IncomingCall, Some(secs)),
                (Some(secs), false) => (ChatEventKind::OutgoingCall, Some(secs)),
                // never connected: theirs was missed (unanswered or declined), ours unanswered
                (None, true) => (ChatEventKind::MissedCall, None),
                (None, false) => (ChatEventKind::OutgoingCall, None),
            };
            Mapped {
                phase: Some((CallPhase::Ended, Some(end_reason(reason).to_owned()))),
                chat_event: Some(chat_event),
                call_over: true,
            }
        }
        // a *different* offer was rejected (busy, expired, glare): the active call is untouched,
        // but the rejected caller's attempt was missed
        CallState::Rejected(_) => Mapped {
            phase: None,
            chat_event: Some((ChatEventKind::MissedCall, None)),
            call_over: false,
        },
        CallState::Concluded => Mapped {
            phase: None,
            chat_event: None,
            call_over: true,
        },
    }
}

fn phase_only(phase: CallPhase) -> Mapped {
    Mapped {
        phase: Some((phase, None)),
        chat_event: None,
        call_over: false,
    }
}

/// A short human reason for the call window and the Core Log.
pub(crate) fn end_reason(reason: &CallEndReason) -> &'static str {
    match reason {
        CallEndReason::LocalHangup
        | CallEndReason::RemoteHangup
        | CallEndReason::RemoteHangupNeedPermission
        | CallEndReason::AppDroppedCall => "hung up",
        CallEndReason::RemoteHangupAccepted => "accepted on another device",
        CallEndReason::RemoteHangupDeclined => "declined on another device",
        CallEndReason::RemoteHangupBusy | CallEndReason::RemoteBusy => "busy",
        CallEndReason::Timeout => "no answer",
        CallEndReason::RemoteGlare | CallEndReason::RemoteReCall => "you called each other",
        _ => "call failed",
    }
}

#[cfg(test)]
mod tests {
    use ringrtc::core::call_summary::CallSummary;

    use super::*;

    fn active(incoming: bool, connected_at: Option<i64>) -> ActiveCall {
        ActiveCall {
            call_id_u64: 1,
            peer: "22222222-2222-2222-2222-222222222222".into(),
            incoming,
            started_at: 1_000,
            connected_at,
        }
    }

    fn ended(reason: CallEndReason) -> CallState {
        CallState::Ended(reason, CallSummary::default())
    }

    #[test]
    fn the_live_phases_map_one_to_one() {
        use ringrtc::common::CallMediaType;
        let a = active(true, None);
        let phase = |s: &CallState| map_state(&a, s, 0).phase.unwrap().0;
        assert_eq!(phase(&CallState::Incoming(CallMediaType::Audio)), CallPhase::Incoming);
        assert_eq!(phase(&CallState::Outgoing(CallMediaType::Audio)), CallPhase::Outgoing);
        assert_eq!(phase(&CallState::Ringing), CallPhase::Ringing);
        assert_eq!(phase(&CallState::Connecting), CallPhase::Connecting);
        assert_eq!(phase(&CallState::Connected), CallPhase::Connected);
    }

    #[test]
    fn a_connected_incoming_call_ends_as_an_incoming_call_event() {
        let m = map_state(&active(true, Some(5_000)), &ended(CallEndReason::RemoteHangup), 65_000);
        assert_eq!(m.phase, Some((CallPhase::Ended, Some("hung up".into()))));
        assert_eq!(m.chat_event, Some((ChatEventKind::IncomingCall, Some(60))));
        assert!(m.call_over);
    }

    #[test]
    fn a_connected_outgoing_call_ends_as_an_outgoing_call_event() {
        let m = map_state(&active(false, Some(2_000)), &ended(CallEndReason::LocalHangup), 32_000);
        assert_eq!(m.chat_event, Some((ChatEventKind::OutgoingCall, Some(30))));
    }

    #[test]
    fn an_unanswered_incoming_call_is_missed() {
        // remote hung up before we answered — and a local decline reads the same way
        for reason in [CallEndReason::RemoteHangup, CallEndReason::LocalHangup] {
            let m = map_state(&active(true, None), &ended(reason), 9_000);
            assert_eq!(m.chat_event, Some((ChatEventKind::MissedCall, None)));
            assert!(m.call_over);
        }
    }

    #[test]
    fn an_unanswered_outgoing_call_reads_no_answer() {
        let m = map_state(&active(false, None), &ended(CallEndReason::Timeout), 9_000);
        assert_eq!(m.phase, Some((CallPhase::Ended, Some("no answer".into()))));
        assert_eq!(m.chat_event, Some((ChatEventKind::OutgoingCall, None)));
    }

    #[test]
    fn a_rejected_second_offer_is_missed_but_does_not_end_the_active_call() {
        let m = map_state(
            &active(true, Some(5_000)),
            &CallState::Rejected(ringrtc::native::RejectReason::ReceivedOfferWhileActive),
            9_000,
        );
        assert_eq!(m.phase, None);
        assert_eq!(m.chat_event, Some((ChatEventKind::MissedCall, None)));
        assert!(!m.call_over);
    }

    #[test]
    fn concluded_only_cleans_up() {
        let m = map_state(&active(true, None), &CallState::Concluded, 9_000);
        assert_eq!(m.phase, None);
        assert_eq!(m.chat_event, None);
        assert!(m.call_over);
    }
}
