//! Pure translation of presage `Content` into projection operations. No I/O: the actor applies
//! the returned ops to the database.

use presage::libsignal_service::content::{sync_message, Content, ContentBody, DataMessage};

use super::ids::{aci, direct_id, group_id};
use super::model::*;

#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    InsertMessage(Message),
}

/// The ops one received (or locally sent) `Content` produces. `me` is our own ACI as a canonical
/// UUID string. Content we don't show yet (calls, stories, receipts, …) yields no ops.
pub fn project(content: &Content, me: &str) -> Vec<Op> {
    // The sender is always an ACI (sealed sender certificates carry the ACI).
    let sender = content.metadata.sender.raw_uuid().to_string();
    let received_at = content.metadata.server_timestamp.timestamp_millis();
    match &content.body {
        ContentBody::DataMessage(dm) => {
            let outgoing = sender == me;
            // Our own messages (as presage saves them after sending) belong to the recipient's chat.
            let peer = if outgoing {
                content.metadata.destination
            } else {
                content.metadata.sender
            };
            let Some(conv) = group_of(dm).unwrap_or_else(|| Some(direct_id(&peer))) else {
                return vec![];
            };
            data_message(dm, &conv, &sender, received_at, outgoing)
        }
        // libsignal-service already drops sync messages that aren't from our account; checked
        // again here because accepting one would put words in our mouth.
        ContentBody::SynchronizeMessage(sm) if sender == me => match &sm.content {
            Some(sync_message::Content::Sent(sent)) => {
                let Some(dm) = &sent.message else { return vec![] };
                let conv = match group_of(dm) {
                    Some(group) => group,
                    None => sent.parse_destination_service_id().map(|d| direct_id(&d)),
                };
                let Some(conv) = conv else { return vec![] };
                data_message(dm, &conv, me, received_at, true)
            }
            _ => vec![],
        },
        _ => vec![],
    }
}

/// `Some(conversation)` if the message is a group message: `Some(None)` when its group context is
/// malformed (the message is dropped rather than shown in a 1:1 chat), `None` if it has none.
fn group_of(dm: &DataMessage) -> Option<Option<String>> {
    let key = dm.group_v2.as_ref()?.master_key.as_deref()?;
    Some(<[u8; 32]>::try_from(key).ok().map(|k| group_id(&k)))
}

fn data_message(dm: &DataMessage, conv: &str, author: &str, received_at: i64, outgoing: bool) -> Vec<Op> {
    let Some(sent_at) = dm.timestamp.and_then(|t| i64::try_from(t).ok()) else {
        return vec![];
    };
    // Not shown yet: story replies (they belong to the story, M4), view-once media (M4), and
    // messages with nothing visible to us yet (stickers, shared contacts, payments, polls; see the
    // "Needs decision" card).
    if dm.story_context.is_some() || dm.is_view_once == Some(true) {
        return vec![];
    }
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
    use presage::proto::data_message::{self, StoryContext};

    const ME: &str = "11111111-1111-1111-1111-111111111111";
    const ALICE: &str = "22222222-2222-2222-2222-222222222222";
    const BOB: &str = "33333333-3333-3333-3333-333333333333";
    const KEY: [u8; 32] = [7u8; 32];

    fn uuid(u: &str) -> Uuid {
        Uuid::parse_str(u).unwrap()
    }

    fn sid(u: &str) -> ServiceId {
        ServiceId::from(Aci::from(uuid(u)))
    }

    /// Content from `sender` to `dest`. The client clock is 10 s behind the server's, so tests
    /// can tell which timestamp the projector used.
    fn content_to(sender: &str, dest: &str, server_ts: i64, body: ContentBody) -> Content {
        let ts = |ms| chrono::DateTime::from_timestamp_millis(ms).unwrap();
        Content {
            metadata: Metadata {
                sender: sid(sender),
                destination: sid(dest),
                sender_device: DeviceId::new(1).unwrap(),
                pni_verified: None,
                client_timestamp: ts(server_ts - 10_000),
                server_timestamp: ts(server_ts),
                needs_receipt: false,
                unidentified_sender: false,
                was_plaintext: false,
                server_guid: None,
            },
            body,
        }
    }

    fn content(sender: &str, server_ts: i64, body: ContentBody) -> Content {
        content_to(sender, ME, server_ts, body)
    }

    fn text(ts: u64, body: &str) -> DataMessage {
        DataMessage {
            body: Some(body.into()),
            timestamp: Some(ts),
            ..Default::default()
        }
    }

    fn in_group(mut dm: DataMessage, key: Vec<u8>) -> DataMessage {
        dm.group_v2 = Some(GroupContextV2 {
            master_key: Some(key),
            ..Default::default()
        });
        dm
    }

    fn sync(sent: sync_message::Sent) -> ContentBody {
        ContentBody::SynchronizeMessage(SyncMessage {
            content: Some(sync_message::Content::Sent(sent)),
            ..Default::default()
        })
    }

    /// A Sent transcript to `dest` whose own timestamp differs from the message's.
    fn sent_sync(dest: &str, dm: DataMessage) -> ContentBody {
        sync(sync_message::Sent {
            destination_service_id: Some(dest.into()),
            timestamp: Some(999_999),
            message: Some(dm),
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
        assert_eq!((m.author.as_str(), m.sent_at, m.received_at), (ME, 200, 250));
        assert!(m.is_outgoing);
        assert_eq!(m.status, MessageStatus::Sent);
    }

    #[test]
    fn sent_transcript_with_binary_destination_only() {
        let sent = sync_message::Sent {
            destination_service_id_binary: Some(uuid(ALICE).as_bytes().to_vec()),
            message: Some(text(220, "bin")),
            ..Default::default()
        };
        let m = one(project(&content(ME, 270, sync(sent)), ME));
        assert_eq!(m.conversation_id, format!("c:{ALICE}"));
        assert!(m.is_outgoing);
    }

    #[test]
    fn sent_transcript_to_pni_gets_a_distinct_pni_conversation() {
        let m = one(project(
            &content(ME, 280, sent_sync(&format!("PNI:{BOB}"), text(230, "hi"))),
            ME,
        ));
        assert_eq!(m.conversation_id, format!("c:PNI:{BOB}"));
    }

    #[test]
    fn sent_transcript_without_usable_destination_is_dropped_not_put_in_note_to_self() {
        for dest in [None, Some("garbage".to_string())] {
            let sent = sync_message::Sent {
                destination_service_id: dest.clone(),
                message: Some(text(240, "x")),
                ..Default::default()
            };
            assert!(project(&content(ME, 290, sync(sent)), ME).is_empty(), "{dest:?}");
        }
    }

    #[test]
    fn note_to_self_from_phone_is_outgoing_in_my_own_conversation() {
        let m = one(project(&content(ME, 260, sent_sync(ME, text(210, "note"))), ME));
        assert_eq!(m.conversation_id, format!("c:{ME}"));
        assert!(m.is_outgoing);
    }

    #[test]
    fn own_data_message_goes_to_the_recipient_not_note_to_self() {
        // How presage stores a message this device sent.
        let c = content_to(ME, ALICE, 160, ContentBody::DataMessage(text(110, "mine")));
        let m = one(project(&c, ME));
        assert_eq!(m.conversation_id, format!("c:{ALICE}"));
        assert_eq!(m.author, ME);
        assert!(m.is_outgoing);
        assert_eq!(m.status, MessageStatus::Sent);
    }

    #[test]
    fn group_text_uses_group_conversation() {
        let dm = in_group(text(300, "group hi"), KEY.to_vec());
        let m = one(project(&content(ALICE, 310, ContentBody::DataMessage(dm.clone())), ME));
        assert_eq!(m.conversation_id, format!("g:{}", hex::encode(KEY)));
        assert_eq!((m.author.as_str(), m.sent_at, m.received_at), (ALICE, 300, 310));
        // the same group message sent from my phone, even if the transcript names a destination
        let m = one(project(&content(ME, 320, sent_sync(ALICE, dm)), ME));
        assert_eq!(m.conversation_id, format!("g:{}", hex::encode(KEY)));
        assert!(m.is_outgoing);
    }

    #[test]
    fn group_message_with_malformed_key_is_dropped_not_shown_in_a_direct_chat() {
        let dm = in_group(text(330, "x"), vec![7u8; 31]);
        assert!(project(&content(ALICE, 340, ContentBody::DataMessage(dm.clone())), ME).is_empty());
        assert!(project(&content(ME, 340, sent_sync(ALICE, dm)), ME).is_empty());
    }

    #[test]
    fn sync_message_from_someone_else_is_ignored() {
        assert!(project(&content(ALICE, 350, sent_sync(BOB, text(345, "forged"))), ME).is_empty());
    }

    #[test]
    fn quote_is_carried_from_string_or_binary_author() {
        let mut dm = text(400, "reply");
        dm.quote = Some(data_message::Quote {
            id: Some(100),
            author_aci: Some(BOB.into()),
            text: Some("orig".into()),
            ..Default::default()
        });
        let m = one(project(&content(ALICE, 400, ContentBody::DataMessage(dm.clone())), ME));
        assert_eq!(
            m.quote,
            Some(Quote {
                author: BOB.into(),
                sent_at: 100,
                text: Some("orig".into())
            })
        );

        let q = dm.quote.as_mut().unwrap();
        q.author_aci = None;
        q.author_aci_binary = Some(uuid(BOB).as_bytes().to_vec());
        let m = one(project(&content(ALICE, 400, ContentBody::DataMessage(dm)), ME));
        assert_eq!(m.quote.unwrap().author, BOB);
    }

    #[test]
    fn incomplete_quote_is_dropped_but_message_kept() {
        let quotes = [
            data_message::Quote {
                id: Some(1),
                ..Default::default()
            },
            data_message::Quote {
                author_aci: Some(ALICE.into()),
                ..Default::default()
            },
        ];
        for q in quotes {
            let mut dm = text(410, "reply");
            dm.quote = Some(q);
            let m = one(project(&content(ALICE, 410, ContentBody::DataMessage(dm)), ME));
            assert_eq!((m.body.as_deref(), m.quote), (Some("reply"), None));
        }
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
    fn story_replies_and_view_once_are_not_shown_yet() {
        let mut reply = text(460, "nice story");
        reply.story_context = Some(StoryContext::default());
        let mut view_once = DataMessage {
            timestamp: Some(461),
            attachments: vec![Default::default()],
            ..Default::default()
        };
        view_once.is_view_once = Some(true);
        for dm in [reply.clone(), in_group(reply, KEY.to_vec()), view_once] {
            assert!(
                project(&content(ALICE, 470, ContentBody::DataMessage(dm.clone())), ME).is_empty(),
                "{dm:?}"
            );
        }
    }

    #[test]
    fn content_without_visible_text_or_attachments_is_skipped() {
        let reaction = DataMessage {
            timestamp: Some(502),
            reaction: Some(data_message::Reaction {
                emoji: Some("👍".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let delete = DataMessage {
            timestamp: Some(503),
            delete: Some(data_message::Delete {
                target_sent_timestamp: Some(1),
            }),
            ..Default::default()
        };
        let mut group_update = in_group(
            DataMessage {
                timestamp: Some(504),
                ..Default::default()
            },
            KEY.to_vec(),
        );
        group_update.group_v2.as_mut().unwrap().group_change = Some(vec![1]);
        let timer = DataMessage {
            timestamp: Some(505),
            flags: Some(2),
            expire_timer: Some(60),
            ..Default::default()
        };
        // pinned M1 behaviour until decided: stickers have no body or attachments and are skipped
        let sticker = DataMessage {
            timestamp: Some(506),
            sticker: Some(Default::default()),
            ..Default::default()
        };
        let skipped = [
            ContentBody::NullMessage(NullMessage::default()),
            ContentBody::DataMessage(DataMessage {
                timestamp: Some(501),
                ..Default::default()
            }),
            ContentBody::DataMessage(DataMessage {
                body: Some("no timestamp".into()),
                ..Default::default()
            }),
            ContentBody::DataMessage(reaction),
            ContentBody::DataMessage(delete),
            ContentBody::DataMessage(group_update),
            ContentBody::DataMessage(timer),
            ContentBody::DataMessage(sticker),
            ContentBody::SynchronizeMessage(SyncMessage::default()),
            sync(sync_message::Sent {
                destination_service_id: Some(ALICE.into()),
                ..Default::default()
            }),
        ];
        for body in skipped {
            let ops = project(&content(ALICE, 500, body.clone()), ME);
            assert!(ops.is_empty(), "{body:?} gave {ops:?}");
        }
    }
}
