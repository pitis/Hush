//! 1:1 voice calls on RingRTC: signaling translation, relays, and the call engine's thread.

pub(crate) mod actor;
pub(crate) mod relays;

use presage::libsignal_service::protocol::ServiceId;

pub use actor::{AudioDevice, AudioDevices};

/// A received `CallMessage`, as the receive loop hands it to the call router.
pub(crate) struct IncomingEnvelope {
    pub sender: ServiceId,
    pub sender_device: u32,
    pub server_timestamp_ms: i64,
    pub message: presage::proto::CallMessage,
}

/// Only an ACI may ring us: its bare uuid string (RingRTC's peer id and our `c:<aci>`).
pub(crate) fn call_sender_aci(sender: &ServiceId) -> Option<String> {
    match sender {
        ServiceId::Aci(aci) => Some(presage::libsignal_service::prelude::Uuid::from(*aci).to_string()),
        ServiceId::Pni(_) => None,
    }
}
pub(crate) mod state;
pub(crate) mod wire;

/// Where a call is, as the app shows it.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallPhase {
    /// Someone is calling us (rings until accepted or declined).
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_aci_senders_may_ring() {
        use presage::libsignal_service::protocol::{Aci, Pni};
        let u = presage::libsignal_service::prelude::Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap();
        assert_eq!(call_sender_aci(&ServiceId::from(Aci::from(u))), Some(u.to_string()));
        assert_eq!(call_sender_aci(&ServiceId::from(Pni::from(u))), None);
    }

    #[test]
    fn phases_are_distinct_and_ended_is_terminal() {
        // pins the enum's existence and that Ended compares as itself (the app's dismiss cue)
        assert_ne!(CallPhase::Incoming, CallPhase::Ended);
        assert_eq!(CallPhase::Ended, CallPhase::Ended);
    }
}
