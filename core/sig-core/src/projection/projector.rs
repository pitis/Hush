//! Pure translation of presage `Content` into projection operations. No I/O: the actor applies
//! the returned ops to the database.

use presage::libsignal_service::content::{sync_message, Content, ContentBody, DataMessage};
use presage::store::Thread;

use super::ids::{aci, conversation_id};
use super::model::*;

#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    InsertMessage(Message),
}

/// The ops one received `Content` produces. `me` is our own ACI (canonical UUID string).
/// Content we don't show yet (calls, stories, receipts, …) yields no ops.
pub fn project(content: &Content, me: &str) -> Vec<Op> {
    let sender = content.metadata.sender.raw_uuid().to_string();
    let received_at = content.metadata.server_timestamp.timestamp_millis();
    match &content.body {
        ContentBody::DataMessage(dm) => {
            let Some(conv) = thread_of(dm, content) else {
                return vec![];
            };
            data_message(dm, &conv, &sender, received_at, sender == me)
        }
        ContentBody::SynchronizeMessage(sm) => match &sm.content {
            Some(sync_message::Content::Sent(sent)) => match &sent.message {
                Some(dm) => {
                    let Some(conv) = thread_of(dm, content) else {
                        return vec![];
                    };
                    data_message(dm, &conv, me, received_at, true)
                }
                None => vec![],
            },
            _ => vec![],
        },
        _ => vec![],
    }
}

/// The conversation a data message belongs to. The group context wins over a sync transcript's
/// destination, so a group message sent from the phone never lands in a 1:1 chat.
fn thread_of(dm: &DataMessage, content: &Content) -> Option<String> {
    if let Some(key) = dm.group_v2.as_ref().and_then(|g| g.master_key.as_deref()) {
        let key: [u8; 32] = key.try_into().ok()?;
        return Some(conversation_id(&Thread::Group(key)));
    }
    Thread::try_from(content).ok().map(|t| conversation_id(&t))
}

fn data_message(dm: &DataMessage, conv: &str, author: &str, received_at: i64, outgoing: bool) -> Vec<Op> {
    let Some(sent_at) = dm.timestamp.and_then(|t| i64::try_from(t).ok()) else {
        return vec![];
    };
    if dm.body.is_none() && dm.attachments.is_empty() {
        return vec![];
    }
    let quote = dm.quote.as_ref().and_then(|q| {
        Some(Quote {
            author: aci(q.author_aci.as_deref(), q.author_aci_binary.as_deref())?,
            sent_at: i64::try_from(q.id?).ok()?,
            text: q.text.clone(),
        })
    });
    vec![Op::InsertMessage(Message {
        conversation_id: conv.to_string(),
        author: author.to_string(),
        sent_at,
        received_at,
        body: dm.body.clone(),
        is_outgoing: outgoing,
        status: if outgoing {
            MessageStatus::Sent
        } else {
            MessageStatus::Delivered
        },
        edited: false,
        deleted: false,
        quote,
    })]
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::libsignal_service::content::{
        sync_message, Content, ContentBody, DataMessage, GroupContextV2, Metadata, NullMessage, SyncMessage,
    };
    use presage::libsignal_service::prelude::Uuid;
    use presage::libsignal_service::protocol::{Aci, DeviceId, ServiceId};
    use presage::proto::data_message;

    const ME: &str = "11111111-1111-1111-1111-111111111111";
    const ALICE: &str = "22222222-2222-2222-2222-222222222222";

    fn sid(u: &str) -> ServiceId {
        ServiceId::from(Aci::from(Uuid::parse_str(u).unwrap()))
    }

    fn content(sender: &str, server_ts: i64, body: ContentBody) -> Content {
        let t = chrono::DateTime::from_timestamp_millis(server_ts).unwrap();
        Content {
            metadata: Metadata {
                sender: sid(sender),
                destination: sid(ME),
                sender_device: DeviceId::new(1).unwrap(),
                pni_verified: None,
                client_timestamp: t,
                server_timestamp: t,
                needs_receipt: false,
                unidentified_sender: false,
                was_plaintext: false,
                server_guid: None,
            },
            body,
        }
    }

    fn text(ts: u64, body: &str) -> DataMessage {
        DataMessage {
            body: Some(body.into()),
            timestamp: Some(ts),
            ..Default::default()
        }
    }

    fn sent_sync(dest: &str, dm: DataMessage) -> ContentBody {
        ContentBody::SynchronizeMessage(SyncMessage {
            content: Some(sync_message::Content::Sent(sync_message::Sent {
                destination_service_id: Some(dest.into()),
                timestamp: dm.timestamp,
                message: Some(dm),
                ..Default::default()
            })),
            ..Default::default()
        })
    }

    fn one(ops: Vec<Op>) -> Message {
        match ops.as_slice() {
            [Op::InsertMessage(m)] => m.clone(),
            _ => panic!("expected one InsertMessage, got {ops:?}"),
        }
    }

    #[test]
    fn incoming_direct_text() {
        let m = one(project(
            &content(ALICE, 150, ContentBody::DataMessage(text(100, "hi"))),
            ME,
        ));
        assert_eq!(m.conversation_id, format!("c:{ALICE}"));
        assert_eq!((m.author.as_str(), m.sent_at, m.received_at), (ALICE, 100, 150));
        assert_eq!(m.body.as_deref(), Some("hi"));
        assert!(!m.is_outgoing);
        assert_eq!(m.status, MessageStatus::Delivered);
    }

    #[test]
    fn sent_from_my_phone_lands_in_recipient_conversation_as_outgoing() {
        let m = one(project(
            &content(ME, 250, sent_sync(ALICE, text(200, "from phone"))),
            ME,
        ));
        assert_eq!(m.conversation_id, format!("c:{ALICE}"));
        assert_eq!((m.author.as_str(), m.sent_at), (ME, 200));
        assert!(m.is_outgoing);
        assert_eq!(m.status, MessageStatus::Sent);
    }

    #[test]
    fn note_to_self_from_phone_is_outgoing_in_my_own_conversation() {
        let m = one(project(&content(ME, 260, sent_sync(ME, text(210, "note"))), ME));
        assert_eq!(m.conversation_id, format!("c:{ME}"));
        assert!(m.is_outgoing);
    }

    #[test]
    fn group_text_uses_group_conversation() {
        let mut dm = text(300, "group hi");
        dm.group_v2 = Some(GroupContextV2 {
            master_key: Some(vec![7u8; 32]),
            ..Default::default()
        });
        let m = one(project(&content(ALICE, 300, ContentBody::DataMessage(dm.clone())), ME));
        assert_eq!(m.conversation_id, format!("g:{}", hex::encode([7u8; 32])));
        assert_eq!(m.author, ALICE);
        // the same group message sent from my phone
        let m = one(project(&content(ME, 301, sent_sync(ALICE, dm)), ME));
        assert_eq!(m.conversation_id, format!("g:{}", hex::encode([7u8; 32])));
        assert!(m.is_outgoing);
    }

    #[test]
    fn quote_is_carried_from_string_or_binary_author() {
        let mut dm = text(400, "reply");
        dm.quote = Some(data_message::Quote {
            id: Some(100),
            author_aci: Some(ME.into()),
            text: Some("orig".into()),
            ..Default::default()
        });
        let m = one(project(&content(ALICE, 400, ContentBody::DataMessage(dm.clone())), ME));
        assert_eq!(
            m.quote,
            Some(Quote {
                author: ME.into(),
                sent_at: 100,
                text: Some("orig".into())
            })
        );

        let q = dm.quote.as_mut().unwrap();
        q.author_aci = None;
        q.author_aci_binary = Some(Uuid::parse_str(ME).unwrap().as_bytes().to_vec());
        let m = one(project(&content(ALICE, 400, ContentBody::DataMessage(dm)), ME));
        assert_eq!(m.quote.unwrap().author, ME);
    }

    #[test]
    fn quote_without_author_or_id_is_dropped_not_fatal() {
        let mut dm = text(410, "reply");
        dm.quote = Some(data_message::Quote {
            id: Some(1),
            ..Default::default()
        });
        let m = one(project(&content(ALICE, 410, ContentBody::DataMessage(dm)), ME));
        assert_eq!((m.body.as_deref(), m.quote), (Some("reply"), None));
    }

    #[test]
    fn attachment_only_message_is_kept() {
        let dm = DataMessage {
            timestamp: Some(450),
            attachments: vec![Default::default()],
            ..Default::default()
        };
        let m = one(project(&content(ALICE, 450, ContentBody::DataMessage(dm)), ME));
        assert_eq!(m.body, None);
    }

    #[test]
    fn unsupported_content_is_skipped() {
        let skipped = [
            ContentBody::NullMessage(NullMessage::default()),
            // empty data message (e.g. a profile key update)
            ContentBody::DataMessage(DataMessage {
                timestamp: Some(501),
                ..Default::default()
            }),
            // text without a timestamp can't be identified
            ContentBody::DataMessage(DataMessage {
                body: Some("x".into()),
                ..Default::default()
            }),
            // sync messages other than Sent
            ContentBody::SynchronizeMessage(SyncMessage::default()),
            // a Sent transcript with no message
            ContentBody::SynchronizeMessage(SyncMessage {
                content: Some(sync_message::Content::Sent(sync_message::Sent {
                    destination_service_id: Some(ALICE.into()),
                    ..Default::default()
                })),
                ..Default::default()
            }),
        ];
        for body in skipped {
            let ops = project(&content(ALICE, 500, body.clone()), ME);
            assert!(ops.is_empty(), "{body:?} gave {ops:?}");
        }
    }
}
