//! Pure translation of presage `Content` into projection operations. No I/O: the actor applies
//! the returned ops to the database.

use presage::libsignal_service::content::{
    sync_message, Content, ContentBody, DataMessage, EditMessage, ReceiptMessage, TypingMessage,
};
use presage::libsignal_service::protocol::ServiceId;
use presage::proto::{receipt_message, typing_message};

use super::ids::{aci, direct_id, group_id};
use super::model::*;

#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    InsertMessage(Message),
    /// `emoji: None` removes the reactor's reaction.
    React {
        target: MessageKey,
        reactor: String,
        emoji: Option<String>,
    },
    /// Replaces the body of `target` (always the editor's own message); `edited_at` orders edits.
    Edit {
        target: MessageKey,
        new_body: Option<String>,
        edited_at: i64,
    },
    /// Delete for everyone (always the deleter's own message).
    Delete {
        target: MessageKey,
    },
    /// Raises the status of the targets: delivery/read receipts for our messages, or the phone
    /// telling us it read incoming ones. Never lowers it.
    SetStatus {
        targets: Vec<MessageKey>,
        status: MessageStatus,
    },
    Typing {
        conversation_id: String,
        who: String,
        started: bool,
    },
    /// Privacy settings from our phone; `None` leaves a setting as it is.
    Settings {
        read_receipts: Option<bool>,
        typing_indicators: Option<bool>,
    },
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
        ContentBody::EditMessage(em) => edit(em, &sender).into_iter().collect(),
        ContentBody::ReceiptMessage(rm) if sender != me => receipt(rm, me).into_iter().collect(),
        ContentBody::TypingMessage(tm) if sender != me => typing(tm, &content.metadata.sender).into_iter().collect(),
        // libsignal-service already drops sync messages that aren't from our account; checked
        // again here because accepting one would put words in our mouth.
        ContentBody::SynchronizeMessage(sm) if sender == me => {
            let mut ops = match &sm.content {
                Some(sync_message::Content::Sent(sent)) => sent_transcript(sent, me, received_at),
                Some(sync_message::Content::Configuration(c))
                    if c.read_receipts.is_some() || c.typing_indicators.is_some() =>
                {
                    vec![Op::Settings {
                        read_receipts: c.read_receipts,
                        typing_indicators: c.typing_indicators,
                    }]
                }
                _ => vec![],
            };
            let read: Vec<_> = sm
                .read
                .iter()
                .filter_map(|r| {
                    Some(MessageKey {
                        author: aci(r.sender_aci.as_deref(), r.sender_aci_binary.as_deref())?,
                        sent_at: i64::try_from(r.timestamp?).ok()?,
                    })
                })
                .collect();
            if !read.is_empty() {
                ops.push(Op::SetStatus {
                    targets: read,
                    status: MessageStatus::Read,
                });
            }
            ops
        }
        _ => vec![],
    }
}

fn sent_transcript(sent: &sync_message::Sent, me: &str, received_at: i64) -> Vec<Op> {
    if let Some(em) = &sent.edit_message {
        return edit(em, me).into_iter().collect();
    }
    let Some(dm) = &sent.message else { return vec![] };
    let conv = match group_of(dm) {
        Some(group) => group,
        None => sent.parse_destination_service_id().map(|d| direct_id(&d)),
    };
    let Some(conv) = conv else { return vec![] };
    data_message(dm, &conv, me, received_at, true)
}

fn edit(em: &EditMessage, author: &str) -> Option<Op> {
    let dm = em.data_message.as_ref()?;
    Some(Op::Edit {
        target: MessageKey {
            author: author.to_string(),
            sent_at: i64::try_from(em.target_sent_timestamp?).ok()?,
        },
        new_body: dm.body.clone(),
        edited_at: i64::try_from(dm.timestamp?).ok()?,
    })
}

/// A receipt from a recipient about messages we sent.
fn receipt(rm: &ReceiptMessage, me: &str) -> Option<Op> {
    use receipt_message::Type;
    let status = match Type::try_from(rm.r#type?).ok()? {
        Type::Delivery => MessageStatus::Delivered,
        Type::Read | Type::Viewed => MessageStatus::Read,
    };
    let targets: Vec<_> = rm
        .timestamp
        .iter()
        .filter_map(|t| {
            Some(MessageKey {
                author: me.to_string(),
                sent_at: i64::try_from(*t).ok()?,
            })
        })
        .collect();
    (!targets.is_empty()).then_some(Op::SetStatus { targets, status })
}

/// Typing in a 1:1 chat. Group typing carries a group id we can't map to a conversation yet.
fn typing(tm: &TypingMessage, sender: &ServiceId) -> Option<Op> {
    if tm.group_id.is_some() {
        return None;
    }
    let started = typing_message::Action::try_from(tm.action?).ok()? == typing_message::Action::Started;
    Some(Op::Typing {
        conversation_id: direct_id(sender),
        who: sender.raw_uuid().to_string(),
        started,
    })
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
    if let Some(r) = &dm.reaction {
        let target = (|| {
            Some(MessageKey {
                author: aci(r.target_author_aci.as_deref(), r.target_author_aci_binary.as_deref())?,
                sent_at: i64::try_from(r.target_sent_timestamp?).ok()?,
            })
        })();
        let emoji = if r.remove == Some(true) { None } else { r.emoji.clone() };
        return match target {
            Some(target) if r.remove == Some(true) || emoji.is_some() => {
                vec![Op::React {
                    target,
                    reactor: author.to_string(),
                    emoji,
                }]
            }
            _ => vec![],
        };
    }
    if let Some(d) = &dm.delete {
        return d
            .target_sent_timestamp
            .and_then(|t| i64::try_from(t).ok())
            .map(|sent_at| {
                vec![Op::Delete {
                    target: MessageKey {
                        author: author.to_string(),
                        sent_at,
                    },
                }]
            })
            .unwrap_or_default();
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
        sync_message, Content, ContentBody, DataMessage, EditMessage, GroupContextV2, Metadata, NullMessage,
        ReceiptMessage, SyncMessage, TypingMessage,
    };
    use presage::libsignal_service::prelude::Uuid;
    use presage::libsignal_service::protocol::{Aci, DeviceId, ServiceId};
    use presage::proto::data_message::{self, StoryContext};
    use presage::proto::{receipt_message, typing_message};

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

    fn key(author: &str, sent_at: i64) -> MessageKey {
        MessageKey {
            author: author.into(),
            sent_at,
        }
    }

    fn reaction(emoji: &str, remove: bool, target_author: &str, target: u64) -> DataMessage {
        DataMessage {
            timestamp: Some(600),
            reaction: Some(data_message::Reaction {
                emoji: Some(emoji.into()),
                remove: Some(remove),
                target_author_aci: Some(target_author.into()),
                target_sent_timestamp: Some(target),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn reaction_add_and_remove() {
        let ops = project(
            &content(ALICE, 600, ContentBody::DataMessage(reaction("👍", false, ME, 100))),
            ME,
        );
        assert_eq!(
            ops,
            vec![Op::React {
                target: key(ME, 100),
                reactor: ALICE.into(),
                emoji: Some("👍".into())
            }]
        );
        let ops = project(
            &content(ALICE, 601, ContentBody::DataMessage(reaction("👍", true, ME, 100))),
            ME,
        );
        assert_eq!(
            ops,
            vec![Op::React {
                target: key(ME, 100),
                reactor: ALICE.into(),
                emoji: None
            }]
        );
    }

    #[test]
    fn reaction_target_from_binary_author_and_in_groups() {
        let mut dm = in_group(reaction("❤️", false, BOB, 100), KEY.to_vec());
        let r = dm.reaction.as_mut().unwrap();
        r.target_author_aci = None;
        r.target_author_aci_binary = Some(uuid(BOB).as_bytes().to_vec());
        let ops = project(&content(ALICE, 602, ContentBody::DataMessage(dm)), ME);
        assert_eq!(
            ops,
            vec![Op::React {
                target: key(BOB, 100),
                reactor: ALICE.into(),
                emoji: Some("❤️".into())
            }]
        );
    }

    #[test]
    fn my_reaction_from_phone() {
        let ops = project(
            &content(ME, 603, sent_sync(ALICE, reaction("😂", false, ALICE, 50))),
            ME,
        );
        assert_eq!(
            ops,
            vec![Op::React {
                target: key(ALICE, 50),
                reactor: ME.into(),
                emoji: Some("😂".into())
            }]
        );
    }

    #[test]
    fn story_reaction_is_ignored() {
        let mut dm = reaction("🔥", false, ME, 100);
        dm.story_context = Some(StoryContext::default());
        assert!(project(&content(ALICE, 604, ContentBody::DataMessage(dm)), ME).is_empty());
    }

    #[test]
    fn delete_for_everyone_targets_the_senders_own_message() {
        let dm = DataMessage {
            timestamp: Some(700),
            delete: Some(data_message::Delete {
                target_sent_timestamp: Some(100),
            }),
            ..Default::default()
        };
        let ops = project(&content(ALICE, 700, ContentBody::DataMessage(dm.clone())), ME);
        assert_eq!(
            ops,
            vec![Op::Delete {
                target: key(ALICE, 100)
            }]
        );
        let ops = project(&content(ME, 701, sent_sync(ALICE, dm)), ME);
        assert_eq!(ops, vec![Op::Delete { target: key(ME, 100) }]);
    }

    #[test]
    fn edit_message_incoming_and_from_phone() {
        let em = EditMessage {
            target_sent_timestamp: Some(100),
            data_message: Some(text(800, "fixed")),
        };
        let ops = project(&content(ALICE, 810, ContentBody::EditMessage(em.clone())), ME);
        assert_eq!(
            ops,
            vec![Op::Edit {
                target: key(ALICE, 100),
                new_body: Some("fixed".into()),
                edited_at: 800
            }]
        );
        let sent = sync_message::Sent {
            destination_service_id: Some(ALICE.into()),
            edit_message: Some(em),
            ..Default::default()
        };
        let ops = project(&content(ME, 820, sync(sent)), ME);
        assert_eq!(
            ops,
            vec![Op::Edit {
                target: key(ME, 100),
                new_body: Some("fixed".into()),
                edited_at: 800
            }]
        );
    }

    #[test]
    fn receipts_target_my_messages() {
        let rm = |t: receipt_message::Type| ReceiptMessage {
            r#type: Some(t as i32),
            timestamp: vec![100, 101],
        };
        let targets = vec![key(ME, 100), key(ME, 101)];
        let ops = project(
            &content(
                ALICE,
                900,
                ContentBody::ReceiptMessage(rm(receipt_message::Type::Delivery)),
            ),
            ME,
        );
        assert_eq!(
            ops,
            vec![Op::SetStatus {
                targets: targets.clone(),
                status: MessageStatus::Delivered
            }]
        );
        for t in [receipt_message::Type::Read, receipt_message::Type::Viewed] {
            let ops = project(&content(ALICE, 901, ContentBody::ReceiptMessage(rm(t))), ME);
            assert_eq!(
                ops,
                vec![Op::SetStatus {
                    targets: targets.clone(),
                    status: MessageStatus::Read
                }]
            );
        }
        let empty = ReceiptMessage {
            r#type: Some(1),
            timestamp: vec![],
        };
        assert!(project(&content(ALICE, 902, ContentBody::ReceiptMessage(empty)), ME).is_empty());
    }

    #[test]
    fn read_on_my_phone_marks_incoming_messages_read() {
        let sm = SyncMessage {
            read: vec![
                sync_message::Read {
                    sender_aci: Some(ALICE.into()),
                    timestamp: Some(100),
                    ..Default::default()
                },
                sync_message::Read {
                    sender_aci_binary: Some(uuid(BOB).as_bytes().to_vec()),
                    timestamp: Some(101),
                    ..Default::default()
                },
                sync_message::Read {
                    timestamp: Some(102),
                    ..Default::default()
                }, // no sender: skipped
            ],
            ..Default::default()
        };
        let ops = project(&content(ME, 950, ContentBody::SynchronizeMessage(sm.clone())), ME);
        assert_eq!(
            ops,
            vec![Op::SetStatus {
                targets: vec![key(ALICE, 100), key(BOB, 101)],
                status: MessageStatus::Read
            }]
        );
        // only our own devices may tell us what we read
        assert!(project(&content(ALICE, 951, ContentBody::SynchronizeMessage(sm)), ME).is_empty());
    }

    #[test]
    fn typing_in_direct_chats_only() {
        let tm = |action: typing_message::Action, group: Option<Vec<u8>>| TypingMessage {
            timestamp: Some(1000),
            action: Some(action as i32),
            group_id: group,
        };
        let ops = project(
            &content(
                ALICE,
                1000,
                ContentBody::TypingMessage(tm(typing_message::Action::Started, None)),
            ),
            ME,
        );
        assert_eq!(
            ops,
            vec![Op::Typing {
                conversation_id: format!("c:{ALICE}"),
                who: ALICE.into(),
                started: true
            }]
        );
        let ops = project(
            &content(
                ALICE,
                1001,
                ContentBody::TypingMessage(tm(typing_message::Action::Stopped, None)),
            ),
            ME,
        );
        assert_eq!(
            ops,
            vec![Op::Typing {
                conversation_id: format!("c:{ALICE}"),
                who: ALICE.into(),
                started: false
            }]
        );
        let group = tm(typing_message::Action::Started, Some(vec![1; 32]));
        assert!(project(&content(ALICE, 1002, ContentBody::TypingMessage(group)), ME).is_empty());
        // my own typing on another device is not shown
        let mine = tm(typing_message::Action::Started, None);
        assert!(project(&content(ME, 1003, ContentBody::TypingMessage(mine)), ME).is_empty());
    }

    #[test]
    fn malformed_mutations_are_dropped() {
        let no_emoji = DataMessage {
            timestamp: Some(1),
            reaction: Some(data_message::Reaction {
                remove: Some(false),
                target_author_aci: Some(ME.into()),
                target_sent_timestamp: Some(1),
                ..Default::default()
            }),
            ..Default::default()
        };
        let bad_target = reaction("👍", false, "not-a-uuid", 1);
        let delete_no_target = DataMessage {
            timestamp: Some(1),
            delete: Some(data_message::Delete {
                target_sent_timestamp: None,
            }),
            ..Default::default()
        };
        let bodies = [
            ContentBody::DataMessage(no_emoji),
            ContentBody::DataMessage(bad_target),
            ContentBody::DataMessage(delete_no_target),
            ContentBody::EditMessage(EditMessage {
                target_sent_timestamp: Some(1),
                data_message: None,
            }),
            ContentBody::EditMessage(EditMessage {
                target_sent_timestamp: None,
                data_message: Some(text(2, "x")),
            }),
            ContentBody::ReceiptMessage(ReceiptMessage {
                r#type: Some(99),
                timestamp: vec![1],
            }),
            ContentBody::TypingMessage(TypingMessage {
                timestamp: Some(1),
                action: None,
                group_id: None,
            }),
        ];
        for body in bodies {
            assert!(project(&content(ALICE, 1, body.clone()), ME).is_empty(), "{body:?}");
        }
    }

    #[test]
    fn my_own_receipts_from_another_device_are_ignored() {
        let rm = ReceiptMessage {
            r#type: Some(receipt_message::Type::Read as i32),
            timestamp: vec![1],
        };
        assert!(project(&content(ME, 1, ContentBody::ReceiptMessage(rm)), ME).is_empty());
    }

    #[test]
    fn privacy_settings_come_from_my_phone_only() {
        let config = |read: Option<bool>, typing: Option<bool>| {
            ContentBody::SynchronizeMessage(SyncMessage {
                content: Some(sync_message::Content::Configuration(sync_message::Configuration {
                    read_receipts: read,
                    typing_indicators: typing,
                    ..Default::default()
                })),
                ..Default::default()
            })
        };
        assert_eq!(
            project(&content(ME, 1, config(Some(false), Some(true))), ME),
            vec![Op::Settings {
                read_receipts: Some(false),
                typing_indicators: Some(true)
            }]
        );
        assert_eq!(
            project(&content(ME, 2, config(Some(true), None)), ME),
            vec![Op::Settings {
                read_receipts: Some(true),
                typing_indicators: None
            }]
        );
        assert!(project(&content(ME, 3, config(None, None)), ME).is_empty());
        assert!(project(&content(ALICE, 4, config(Some(true), Some(true))), ME).is_empty());
    }
}
