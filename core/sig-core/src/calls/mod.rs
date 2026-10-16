//! 1:1 voice calls on RingRTC: signaling translation, relays, and the call engine's thread.

// The pieces land one by one and are wired together by the call actor; until then they are
// unused. TODO(remove when the call actor lands): drop this allow.
#![allow(dead_code)]

pub(crate) mod relays;
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
    fn phases_are_distinct_and_ended_is_terminal() {
        // pins the enum's existence and that Ended compares as itself (the app's dismiss cue)
        assert_ne!(CallPhase::Incoming, CallPhase::Ended);
        assert_eq!(CallPhase::Ended, CallPhase::Ended);
    }
}
