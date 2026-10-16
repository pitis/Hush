//! Translates between RingRTC's signaling messages and the wire's `CallMessage` proto, both
//! directions. This is the contract with real phones.

use presage::proto::call_message as pcm;
use presage::proto::CallMessage;
use ringrtc::common::{CallId, CallMediaType, DeviceId};
use ringrtc::core::signaling;

/// An envelope for the wire. `destination_device_id` filters receiver-side; the message still
/// goes to every device.
pub(crate) fn to_proto(
    call_id: CallId,
    receiver_device_id: Option<DeviceId>,
    message: &signaling::Message,
) -> CallMessage {
    let id = Some(call_id.as_u64());
    let mut proto = CallMessage {
        destination_device_id: receiver_device_id,
        ..Default::default()
    };
    match message {
        signaling::Message::Offer(offer) => {
            proto.offer = Some(pcm::Offer {
                id,
                r#type: Some(match offer.call_media_type {
                    CallMediaType::Audio => pcm::offer::Type::OfferAudioCall as i32,
                    CallMediaType::Video => pcm::offer::Type::OfferVideoCall as i32,
                }),
                opaque: Some(offer.opaque.clone()),
            });
        }
        signaling::Message::Answer(answer) => {
            proto.answer = Some(pcm::Answer {
                id,
                opaque: Some(answer.opaque.clone()),
            });
        }
        signaling::Message::Ice(ice) => {
            proto.ice_update = ice
                .candidates
                .iter()
                .map(|c| pcm::IceUpdate {
                    id,
                    opaque: Some(c.opaque.clone()),
                })
                .collect();
        }
        signaling::Message::Hangup(hangup) => {
            let (typ, device_id) = hangup.to_type_and_device_id();
            proto.hangup = Some(pcm::Hangup {
                id,
                // ringrtc's HangupType is #[repr(i32)] with the proto's numbering
                r#type: Some(typ as i32),
                device_id,
            });
        }
        signaling::Message::Busy => {
            proto.busy = Some(pcm::Busy { id });
        }
    }
    proto
}

/// What one incoming CallMessage asks of the call engine, in handling order (an envelope may
/// carry an offer and ice updates together).
#[derive(Debug)]
pub(crate) struct IncomingCall {
    pub call_id: CallId,
    pub message: signaling::Message,
}

/// Decodes an envelope, skipping junk (missing ids, undecodable opaques) without failing:
/// offer first, then answer, ice, hangup, busy. An envelope addressed to another of our
/// devices (`destination_device_id`) is not ours to act on.
pub(crate) fn from_proto(proto: &CallMessage, our_device_id: u32) -> Vec<IncomingCall> {
    let mut out = Vec::new();
    if proto.destination_device_id.is_some_and(|d| d != our_device_id) {
        return out;
    }
    if let Some(offer) = &proto.offer {
        let media = match offer.r#type.and_then(|t| pcm::offer::Type::try_from(t).ok()) {
            Some(pcm::offer::Type::OfferVideoCall) => CallMediaType::Video,
            _ => CallMediaType::Audio,
        };
        if let (Some(id), Some(opaque)) = (offer.id, offer.opaque.clone()) {
            match signaling::Offer::new(media, opaque) {
                Ok(offer) => out.push(IncomingCall {
                    call_id: CallId::new(id),
                    message: signaling::Message::Offer(offer),
                }),
                Err(e) => tracing::warn!(call_id = id, "an offer couldn't be decoded: {e}"),
            }
        }
    }
    if let Some(answer) = &proto.answer {
        if let (Some(id), Some(opaque)) = (answer.id, answer.opaque.clone()) {
            match signaling::Answer::new(opaque) {
                Ok(answer) => out.push(IncomingCall {
                    call_id: CallId::new(id),
                    message: signaling::Message::Answer(answer),
                }),
                Err(e) => tracing::warn!(call_id = id, "an answer couldn't be decoded: {e}"),
            }
        }
    }
    // all of an envelope's candidates belong to one call: group by id (in practice one id)
    let mut ice_id: Option<u64> = None;
    let mut candidates = Vec::new();
    for update in &proto.ice_update {
        if let (Some(id), Some(opaque)) = (update.id, update.opaque.clone()) {
            if ice_id.is_none() {
                ice_id = Some(id);
            }
            if ice_id == Some(id) {
                candidates.push(signaling::IceCandidate { opaque });
            }
        }
    }
    if let (Some(id), false) = (ice_id, candidates.is_empty()) {
        out.push(IncomingCall {
            call_id: CallId::new(id),
            message: signaling::Message::Ice(signaling::Ice { candidates }),
        });
    }
    if let Some(hangup) = &proto.hangup {
        let typ = hangup
            .r#type
            .and_then(signaling::HangupType::from_i32)
            .unwrap_or(signaling::HangupType::Normal);
        if let Some(id) = hangup.id {
            out.push(IncomingCall {
                call_id: CallId::new(id),
                message: signaling::Message::Hangup(signaling::Hangup::from_type_and_device_id(
                    typ,
                    hangup.device_id.unwrap_or(0),
                )),
            });
        }
    }
    if let Some(busy) = &proto.busy {
        if let Some(id) = busy.id {
            out.push(IncomingCall {
                call_id: CallId::new(id),
                message: signaling::Message::Busy,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RingRTC requires opaque bodies to decode; an empty buffer decodes to an all-None proto.
    fn opaque_offer() -> Vec<u8> {
        Vec::new()
    }

    #[test]
    fn an_offer_round_trips() {
        let offer = signaling::Offer::new(CallMediaType::Audio, opaque_offer()).unwrap();
        let proto = to_proto(CallId::new(7), Some(3), &signaling::Message::Offer(offer));
        let offer_proto = proto.offer.as_ref().unwrap();
        assert_eq!(offer_proto.id, Some(7));
        assert_eq!(offer_proto.r#type, Some(pcm::offer::Type::OfferAudioCall as i32));
        assert_eq!(proto.destination_device_id, Some(3));
        let incoming = from_proto(&proto, 3);
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
        let incoming = from_proto(&proto, 3);
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
            let back = from_proto(&proto, 3);
            assert!(matches!(back[0].message, signaling::Message::Hangup(_)));
        }
    }

    #[test]
    fn busy_round_trips() {
        let proto = to_proto(CallId::new(4), None, &signaling::Message::Busy);
        assert_eq!(proto.busy.as_ref().unwrap().id, Some(4));
        assert!(matches!(from_proto(&proto, 3)[0].message, signaling::Message::Busy));
    }

    /// The proto allows several fields in one envelope: offer first, nothing dropped.
    #[test]
    fn a_combined_envelope_keeps_order() {
        let offer = signaling::Offer::new(CallMediaType::Audio, opaque_offer()).unwrap();
        let mut proto = to_proto(CallId::new(7), None, &signaling::Message::Offer(offer));
        proto.ice_update.push(pcm::IceUpdate {
            id: Some(7),
            opaque: Some(vec![1]),
        });
        let incoming = from_proto(&proto, 3);
        assert_eq!(incoming.len(), 2);
        assert!(matches!(incoming[0].message, signaling::Message::Offer(_)));
        assert!(matches!(incoming[1].message, signaling::Message::Ice(_)));
    }

    /// Junk never panics: missing ids and missing opaques are skipped.
    #[test]
    fn junk_fields_are_skipped() {
        let proto = CallMessage {
            offer: Some(pcm::Offer {
                id: None, // no id: skip
                r#type: Some(pcm::offer::Type::OfferAudioCall as i32),
                opaque: Some(opaque_offer()),
            }),
            busy: Some(pcm::Busy { id: None }),
            ..Default::default()
        };
        assert!(from_proto(&proto, 3).is_empty());
    }

    /// An envelope for another of our devices (a phone's answer to our phone) is dropped.
    #[test]
    fn envelopes_for_another_device_are_not_ours() {
        let proto = to_proto(CallId::new(4), Some(2), &signaling::Message::Busy);
        assert!(from_proto(&proto, 3).is_empty());
        assert_eq!(from_proto(&proto, 2).len(), 1);
    }

    /// A video offer still decodes (we answer voice-only; RingRTC negotiates).
    #[test]
    fn a_video_offer_decodes() {
        let offer = signaling::Offer::new(CallMediaType::Video, opaque_offer()).unwrap();
        let proto = to_proto(CallId::new(2), None, &signaling::Message::Offer(offer));
        assert_eq!(
            proto.offer.as_ref().unwrap().r#type,
            Some(pcm::offer::Type::OfferVideoCall as i32)
        );
        assert_eq!(from_proto(&proto, 3).len(), 1);
    }
}
