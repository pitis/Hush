//! Pure translation of presage `Content` into projection operations. No I/O: the actor applies
//! the returned ops to the database.

use presage::libsignal_service::content::{
    sync_message, Content, ContentBody, DataMessage, EditMessage, ReceiptMessage, TypingMessage,
};
use presage::libsignal_service::prelude::ProtobufMessage;
use presage::libsignal_service::protocol::ServiceId;
use presage::proto::{attachment_pointer, receipt_message, typing_message, AttachmentPointer};

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
        new_ranges: Vec<BodyRange>,
        edited_at: i64,
    },
    /// Delete for everyone (always the deleter's own message).
    Delete {
        target: MessageKey,
    },
    /// The options of the poll `message` (inserted with it).
    PollCreated {
        message: MessageKey,
        options: Vec<String>,
        allow_multiple: bool,
    },
    /// `voter`'s vote on a poll (indexes into its options; none: taken back); the highest
    /// `vote_count` per voter wins.
    PollVote {
        /// The chat it came in: it only counts on a poll there.
        conversation_id: String,
        target: MessageKey,
        voter: String,
        vote_count: u32,
        options: Vec<u32>,
    },
    /// A poll was ended (always by its author, in the chat it came in).
    PollEnded {
        conversation_id: String,
        target: MessageKey,
    },
    /// View-once media was opened (here or on another of our devices): it's gone.
    ViewOnceOpened {
        target: MessageKey,
    },
    /// Raises the status of the targets: delivery/read receipts for our messages, or the phone
    /// telling us it read incoming ones. Never lowers it.
    SetStatus {
        targets: Vec<MessageKey>,
        status: MessageStatus,
    },
    /// We read the targets, on another device (or here) at `at` (ms): their disappearing timers
    /// start then.
    Read {
        targets: Vec<MessageKey>,
        at: i64,
    },
    Typing {
        conversation_id: String,
        who: String,
        started: bool,
    },
    /// The files and link preview of a message inserted just before (ignored if it isn't stored).
    AddAttachments {
        message: MessageKey,
        attachments: Vec<NewAttachment>,
        preview: Option<NewPreview>,
    },
    /// `author` set the chat's disappearing-message timer (seconds, 0: off) at `at`.
    SetTimer {
        conversation_id: String,
        author: String,
        seconds: u32,
        at: i64,
    },
    /// Privacy settings from our phone; `None` leaves a setting as it is.
    Settings {
        read_receipts: Option<bool>,
        typing_indicators: Option<bool>,
    },
    /// Our phone's whole block list (`SyncMessage.Blocked`): it replaces ours. `groups` are
    /// group identifiers. Phone numbers aren't kept here.
    BlockedList {
        acis: Vec<String>,
        groups: Vec<Vec<u8>>,
    },
    /// Our phone answered a message request.
    RequestResponse {
        thread: RequestThread,
        response: RequestResponse,
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
                Some(sync_message::Content::Blocked(b)) => vec![blocked_list(b)],
                Some(sync_message::Content::MessageRequestResponse(r)) => request_response(r).into_iter().collect(),
                Some(sync_message::Content::ViewOnceOpen(v)) => (|| {
                    Some(Op::ViewOnceOpened {
                        target: MessageKey {
                            author: aci(v.sender_aci.as_deref(), v.sender_aci_binary.as_deref())?,
                            sent_at: i64::try_from(v.timestamp?).ok()?,
                        },
                    })
                })()
                .into_iter()
                .collect(),
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
                ops.push(Op::Read {
                    targets: read,
                    at: received_at,
                });
            }
            ops
        }
        _ => vec![],
    }
}

/// The phone's block list, ACIs from both encodings (phone numbers aren't kept).
fn blocked_list(b: &sync_message::Blocked) -> Op {
    let acis = b
        .acis
        .iter()
        .filter_map(|a| aci(Some(a), None))
        .chain(b.acis_binary.iter().filter_map(|a| aci(None, Some(a))))
        .collect();
    Op::BlockedList {
        acis,
        groups: b.group_ids.clone(),
    }
}

/// The phone's answer to a message request; reporting spam alone changes nothing here.
fn request_response(r: &sync_message::MessageRequestResponse) -> Option<Op> {
    use sync_message::message_request_response::Type;
    let response = match Type::try_from(r.r#type?).ok()? {
        Type::Accept => RequestResponse::Accept,
        Type::Delete => RequestResponse::Delete,
        Type::Block | Type::BlockAndSpam => RequestResponse::Block,
        Type::BlockAndDelete => RequestResponse::BlockAndDelete,
        Type::Spam | Type::Unknown => return None,
    };
    let thread = match &r.group_id {
        Some(g) => RequestThread::Group(g.clone()),
        None => RequestThread::Aci(aci(r.thread_aci.as_deref(), r.thread_aci_binary.as_deref())?),
    };
    Some(Op::RequestResponse { thread, response })
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
        new_ranges: text_ranges(&dm.body_ranges),
        edited_at: i64::try_from(dm.timestamp?).ok()?,
    })
}

/// Signal's body ranges as ours; unknown styles and mentions without a readable ACI are left out.
fn text_ranges(ranges: &[presage::proto::BodyRange]) -> Vec<BodyRange> {
    use presage::proto::body_range::AssociatedValue;
    ranges
        .iter()
        .filter_map(|r| {
            let style = match r.associated_value.as_ref()? {
                AssociatedValue::Style(s) => TextStyle::from_proto(*s)?,
                AssociatedValue::MentionAci(a) => TextStyle::Mention {
                    aci: aci(Some(a), None)?,
                },
                AssociatedValue::MentionAciBinary(b) => TextStyle::Mention {
                    aci: aci(None, Some(b))?,
                },
            };
            Some(BodyRange {
                // proto2: a start left out is 0
                start: r.start.unwrap_or(0),
                length: r.length?,
                style,
            })
        })
        .collect()
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
    // Not shown yet: story replies (they belong to the story, M4).
    if dm.story_context.is_some() {
        return vec![];
    }
    let view_once = dm.is_view_once == Some(true);
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
    if let Some(v) = &dm.poll_vote {
        return (|| {
            Some(Op::PollVote {
                conversation_id: conv.to_string(),
                target: MessageKey {
                    author: aci(None, v.target_author_aci_binary.as_deref())?,
                    sent_at: i64::try_from(v.target_sent_timestamp?).ok()?,
                },
                voter: author.to_string(),
                vote_count: v.vote_count.unwrap_or(0),
                options: v.option_indexes.clone(),
            })
        })()
        .into_iter()
        .collect();
    }
    if let Some(t) = &dm.poll_terminate {
        return t
            .target_sent_timestamp
            .and_then(|t| i64::try_from(t).ok())
            .map(|sent_at| {
                vec![Op::PollEnded {
                    conversation_id: conv.to_string(),
                    target: MessageKey {
                        author: author.to_string(),
                        sent_at,
                    },
                }]
            })
            .unwrap_or_default();
    }
    if dm.flags.unwrap_or(0) & presage::proto::data_message::Flags::ExpirationTimerUpdate as u32 != 0 {
        return vec![Op::SetTimer {
            conversation_id: conv.to_string(),
            author: author.to_string(),
            seconds: dm.expire_timer.unwrap_or(0),
            at: sent_at,
        }];
    }
    // what we can't show yet becomes a placeholder rather than vanishing
    let kind = if view_once {
        // we can't open what we sent
        MessageKind::ViewOnce { viewed: outgoing }
    } else if let Some(s) = &dm.sticker {
        MessageKind::Sticker { emoji: s.emoji.clone() }
    } else if let Some(c) = dm.contact.first() {
        MessageKind::Contact { name: contact_name(c) }
    } else if dm.payment.is_some() {
        MessageKind::Payment
    } else if let Some(p) = &dm.poll_create {
        MessageKind::Poll {
            question: p.question.clone(),
        }
    } else if dm.gift_badge.is_some() {
        MessageKind::GiftBadge
    } else {
        MessageKind::Normal
    };
    if dm.body.is_none() && dm.attachments.is_empty() && kind == MessageKind::Normal {
        return vec![];
    }
    let expires_in = dm.expire_timer.unwrap_or(0);
    let quote = dm.quote.as_ref().and_then(|q| {
        Some(Quote {
            author: aci(q.author_aci.as_deref(), q.author_aci_binary.as_deref())?,
            sent_at: i64::try_from(q.id?).ok()?,
            ranges: text_ranges(&q.body_ranges),
            text: q.text.clone(),
        })
    });
    let message = Message {
        conversation_id: conv.to_string(),
        author: author.to_string(),
        sent_at,
        received_at,
        // a view-once message's caption goes with the media: not kept
        ranges: if view_once {
            vec![]
        } else {
            text_ranges(&dm.body_ranges)
        },
        body: dm.body.clone().filter(|_| !view_once),
        is_outgoing: outgoing,
        status: if outgoing {
            MessageStatus::Sent
        } else {
            MessageStatus::Delivered
        },
        edited: false,
        deleted: false,
        quote,
        kind,
        expires_in,
        // ours start disappearing when sent; others' once read
        expires_at: (outgoing && expires_in > 0).then(|| sent_at + i64::from(expires_in) * 1000),
    };
    let attachments: Vec<_> = dm.attachments.iter().map(new_attachment).collect();
    let preview = dm.preview.first().and_then(|p| {
        Some(NewPreview {
            url: p.url.clone()?,
            title: p.title.clone(),
            description: p.description.clone(),
            image: p.image.as_ref().map(new_attachment),
        })
    });
    let mut ops = vec![];
    // ours, view-once: nothing we may open, so nothing to fetch
    if (!attachments.is_empty() || preview.is_some()) && !(view_once && outgoing) {
        ops.push(Op::AddAttachments {
            message: message.key(),
            attachments,
            preview,
        });
    }
    if let Some(p) = &dm.poll_create {
        ops.push(Op::PollCreated {
            message: message.key(),
            options: p.options.clone(),
            allow_multiple: p.allow_multiple.unwrap_or(false),
        });
    }
    ops.insert(0, Op::InsertMessage(message));
    ops
}

/// A shared contact's name as it reads: the nickname, else given and family name, else the
/// organization.
fn contact_name(c: &presage::proto::data_message::Contact) -> Option<String> {
    let name = c.name.as_ref();
    let full = name
        .map(|n| {
            [n.given_name.as_deref(), n.family_name.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|s| !s.is_empty());
    name.and_then(|n| n.nickname.clone())
        .filter(|s| !s.is_empty())
        .or(full)
        .or_else(|| c.organization.clone())
}

fn new_attachment(p: &AttachmentPointer) -> NewAttachment {
    NewAttachment {
        pointer: p.encode_to_vec(),
        content_type: p
            .content_type
            .clone()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "application/octet-stream".into()),
        file_name: p.file_name.clone(),
        size: u64::from(p.size.unwrap_or(0)),
        width: p.width,
        height: p.height,
        is_voice_note: p.flags.unwrap_or(0) & attachment_pointer::Flags::VoiceMessage as u32 != 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::libsignal_service::content::{
        sync_message, Content, ContentBody, DataMessage, EditMessage, GroupContextV2, Metadata, NullMessage,
        ReceiptMessage, SyncMessage, TypingMessage,
    };
    use presage::libsignal_service::prelude::ProtobufMessage;
    use presage::libsignal_service::prelude::Uuid;
    use presage::libsignal_service::protocol::{Aci, DeviceId, ServiceId};
    use presage::proto::data_message::{self, StoryContext};
    use presage::proto::{receipt_message, typing_message};
    use presage::proto::{AttachmentPointer, Preview};

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
                ranges: vec![],
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
    fn styles_and_mentions_are_carried_in_the_body_the_quote_and_edits() {
        use presage::proto::body_range::{AssociatedValue, Style};
        let range = |start, length, v| presage::proto::BodyRange {
            start: Some(start),
            length: Some(length),
            associated_value: Some(v),
        };
        let mut dm = text(400, "hi \u{fffc} bold");
        dm.body_ranges = vec![
            range(3, 1, AssociatedValue::MentionAciBinary(uuid(BOB).as_bytes().to_vec())),
            range(5, 4, AssociatedValue::Style(Style::Bold as i32)),
            // none, an unknown style and an unreadable mention are left out
            range(0, 2, AssociatedValue::Style(Style::None as i32)),
            // a start left out is 0 (proto2's default)
            presage::proto::BodyRange {
                start: None,
                length: Some(2),
                associated_value: Some(AssociatedValue::Style(Style::Spoiler as i32)),
            },
            range(0, 2, AssociatedValue::Style(99)),
            range(0, 2, AssociatedValue::MentionAci("nope".into())),
        ];
        dm.quote = Some(data_message::Quote {
            id: Some(100),
            author_aci: Some(BOB.into()),
            text: Some("orig".into()),
            body_ranges: vec![range(0, 4, AssociatedValue::Style(Style::Spoiler as i32))],
            ..Default::default()
        });
        let m = one(project(&content(ALICE, 400, ContentBody::DataMessage(dm.clone())), ME));
        assert_eq!(
            m.ranges,
            vec![
                BodyRange {
                    start: 3,
                    length: 1,
                    style: TextStyle::Mention { aci: BOB.into() }
                },
                BodyRange {
                    start: 5,
                    length: 4,
                    style: TextStyle::Bold
                },
                BodyRange {
                    start: 0,
                    length: 2,
                    style: TextStyle::Spoiler
                },
            ]
        );
        assert_eq!(
            m.quote.unwrap().ranges,
            vec![BodyRange {
                start: 0,
                length: 4,
                style: TextStyle::Spoiler
            }]
        );

        let mut fixed = text(800, "fixed");
        fixed.body_ranges = vec![range(0, 5, AssociatedValue::Style(Style::Italic as i32))];
        let em = EditMessage {
            target_sent_timestamp: Some(400),
            data_message: Some(fixed),
        };
        let ops = project(&content(ALICE, 810, ContentBody::EditMessage(em)), ME);
        assert_eq!(
            ops,
            vec![Op::Edit {
                target: key(ALICE, 400),
                new_body: Some("fixed".into()),
                new_ranges: vec![BodyRange {
                    start: 0,
                    length: 5,
                    style: TextStyle::Italic
                }],
                edited_at: 800
            }]
        );
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
        let ops = project(&content(ALICE, 450, ContentBody::DataMessage(dm)), ME);
        assert!(matches!(&ops[..], [Op::InsertMessage(m), Op::AddAttachments { .. }] if m.body.is_none()));
    }

    #[test]
    fn a_poll_brings_its_options_and_votes_and_its_end_count() {
        let poll = DataMessage {
            timestamp: Some(600),
            poll_create: Some(data_message::PollCreate {
                question: Some("Lunch?".into()),
                allow_multiple: Some(true),
                options: vec!["Pizza".into(), "Sushi".into()],
            }),
            ..Default::default()
        };
        let ops = project(
            &content(ALICE, 610, ContentBody::DataMessage(in_group(poll, KEY.to_vec()))),
            ME,
        );
        assert!(
            matches!(&ops[0], Op::InsertMessage(m) if m.kind == MessageKind::Poll { question: Some("Lunch?".into()) })
        );
        assert_eq!(
            ops[1],
            Op::PollCreated {
                message: key(ALICE, 600),
                options: vec!["Pizza".into(), "Sushi".into()],
                allow_multiple: true,
            }
        );
        let vote = DataMessage {
            timestamp: Some(620),
            poll_vote: Some(data_message::PollVote {
                target_author_aci_binary: Some(uuid(ALICE).as_bytes().to_vec()),
                target_sent_timestamp: Some(600),
                option_indexes: vec![1],
                vote_count: Some(2),
            }),
            ..Default::default()
        };
        assert_eq!(
            project(
                &content(BOB, 630, ContentBody::DataMessage(in_group(vote.clone(), KEY.to_vec()))),
                ME
            ),
            vec![Op::PollVote {
                conversation_id: format!("g:{}", hex::encode(KEY)),
                target: key(ALICE, 600),
                voter: BOB.into(),
                vote_count: 2,
                options: vec![1],
            }]
        );
        // our vote from the phone counts as ours
        assert!(matches!(
            &project(&content(ME, 640, sent_sync(ALICE, in_group(vote, KEY.to_vec()))), ME)[..],
            [Op::PollVote { voter, .. }] if voter == ME
        ));
        // only its author ends it: the target is the sender's own poll
        let end = DataMessage {
            timestamp: Some(650),
            poll_terminate: Some(data_message::PollTerminate {
                target_sent_timestamp: Some(600),
            }),
            ..Default::default()
        };
        assert_eq!(
            project(
                &content(BOB, 660, ContentBody::DataMessage(in_group(end, KEY.to_vec()))),
                ME
            ),
            vec![Op::PollEnded {
                conversation_id: format!("g:{}", hex::encode(KEY)),
                target: key(BOB, 600)
            }]
        );
    }

    #[test]
    fn story_replies_are_not_shown_yet() {
        let mut reply = text(460, "nice story");
        reply.story_context = Some(StoryContext::default());
        for dm in [reply.clone(), in_group(reply, KEY.to_vec())] {
            assert!(
                project(&content(ALICE, 470, ContentBody::DataMessage(dm.clone())), ME).is_empty(),
                "{dm:?}"
            );
        }
    }

    #[test]
    fn view_once_media_arrives_unviewed_and_ours_from_the_phone_counts_as_viewed() {
        let view_once = DataMessage {
            timestamp: Some(461),
            body: Some("caption".into()),
            attachments: vec![Default::default()],
            is_view_once: Some(true),
            ..Default::default()
        };
        let inserted = |ops: Vec<Op>| {
            ops.into_iter()
                .find_map(|o| match o {
                    Op::InsertMessage(m) => Some(m),
                    _ => None,
                })
                .unwrap()
        };
        let ops = project(&content(ALICE, 470, ContentBody::DataMessage(view_once.clone())), ME);
        let m = inserted(ops.clone());
        assert_eq!(m.kind, MessageKind::ViewOnce { viewed: false });
        assert_eq!(m.body, None, "a view-once message's caption isn't kept");
        assert!(ops.iter().any(|o| matches!(o, Op::AddAttachments { .. })));
        // we can't open what we sent
        let ops = project(&content(ME, 480, sent_sync(ALICE, view_once)), ME);
        assert!(
            !ops.iter().any(|o| matches!(o, Op::AddAttachments { .. })),
            "nothing to fetch"
        );
        let m = inserted(ops);
        assert_eq!(m.kind, MessageKind::ViewOnce { viewed: true });
    }

    #[test]
    fn opened_on_my_phone_marks_view_once_media_viewed() {
        let sm = SyncMessage {
            content: Some(sync_message::Content::ViewOnceOpen(sync_message::ViewOnceOpen {
                sender_aci_binary: Some(uuid(ALICE).as_bytes().to_vec()),
                timestamp: Some(461),
                ..Default::default()
            })),
            ..Default::default()
        };
        assert_eq!(
            project(&content(ME, 500, ContentBody::SynchronizeMessage(sm.clone())), ME),
            vec![Op::ViewOnceOpened {
                target: key(ALICE, 461)
            }]
        );
        // only our own devices say so
        assert!(project(&content(BOB, 500, ContentBody::SynchronizeMessage(sm)), ME).is_empty());
    }

    #[test]
    /// (Timer changes and stickers aren't skipped any more: see the placeholder and timer tests.)
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
                new_ranges: vec![],
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
                new_ranges: vec![],
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
            vec![Op::Read {
                targets: vec![key(ALICE, 100), key(BOB, 101)],
                at: 950,
            }],
            "read when the phone said so (disappearing timers start then)"
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

    fn sync_body(c: sync_message::Content) -> ContentBody {
        ContentBody::SynchronizeMessage(SyncMessage {
            content: Some(c),
            ..Default::default()
        })
    }

    #[test]
    fn the_phones_block_list_comes_whole_with_acis_in_either_form() {
        let blocked = sync_body(sync_message::Content::Blocked(sync_message::Blocked {
            numbers: vec!["+40700000000".into()],
            acis: vec![ALICE.to_uppercase()],
            acis_binary: vec![uuid(BOB).as_bytes().to_vec(), vec![1, 2]],
            group_ids: vec![vec![9; 32]],
        }));
        assert_eq!(
            project(&content(ME, 1, blocked.clone()), ME),
            vec![Op::BlockedList {
                acis: vec![ALICE.into(), BOB.into()],
                groups: vec![vec![9; 32]],
            }]
        );
        assert!(
            project(&content(ALICE, 2, blocked), ME).is_empty(),
            "only from my phone"
        );
    }

    #[test]
    fn the_phones_request_answers_name_a_contact_or_a_group() {
        use sync_message::message_request_response::Type;
        let mrr = |aci: Option<&str>, group: Option<Vec<u8>>, t: Type| {
            sync_body(sync_message::Content::MessageRequestResponse(
                sync_message::MessageRequestResponse {
                    thread_aci: aci.map(String::from),
                    thread_aci_binary: None,
                    group_id: group,
                    r#type: Some(t as i32),
                },
            ))
        };
        let op = |thread, response| vec![Op::RequestResponse { thread, response }];
        assert_eq!(
            project(&content(ME, 1, mrr(Some(ALICE), None, Type::Accept)), ME),
            op(RequestThread::Aci(ALICE.into()), RequestResponse::Accept)
        );
        assert_eq!(
            project(&content(ME, 1, mrr(None, Some(vec![9; 32]), Type::BlockAndDelete)), ME),
            op(RequestThread::Group(vec![9; 32]), RequestResponse::BlockAndDelete)
        );
        assert_eq!(
            project(&content(ME, 1, mrr(Some(ALICE), None, Type::BlockAndSpam)), ME),
            op(RequestThread::Aci(ALICE.into()), RequestResponse::Block)
        );
        assert_eq!(
            project(&content(ME, 1, mrr(Some(ALICE), None, Type::Delete)), ME),
            op(RequestThread::Aci(ALICE.into()), RequestResponse::Delete)
        );
        assert!(project(&content(ME, 1, mrr(Some(ALICE), None, Type::Spam)), ME).is_empty());
        assert!(project(&content(ME, 1, mrr(None, None, Type::Accept)), ME).is_empty());
        assert!(project(&content(ALICE, 1, mrr(Some(BOB), None, Type::Accept)), ME).is_empty());
    }

    fn pointer(content_type: &str, size: u32, flags: u32) -> AttachmentPointer {
        AttachmentPointer {
            content_type: Some(content_type.into()),
            size: Some(size),
            flags: Some(flags),
            file_name: Some("f.bin".into()),
            width: Some(640),
            height: Some(480),
            digest: Some(vec![9; 32]),
            ..Default::default()
        }
    }

    #[test]
    fn attachments_and_previews_come_with_their_message() {
        let mut dm = text(100, "look");
        dm.attachments = vec![pointer("image/jpeg", 1000, 0), pointer("audio/aac", 50, 1)];
        dm.preview = vec![Preview {
            url: Some("https://example.org".into()),
            title: Some("Example".into()),
            image: Some(pointer("image/png", 10, 0)),
            ..Default::default()
        }];
        let ops = project(&content(ALICE, 150, ContentBody::DataMessage(dm.clone())), ME);
        let [Op::InsertMessage(m), Op::AddAttachments {
            message,
            attachments,
            preview,
        }] = ops.as_slice()
        else {
            panic!("{ops:?}")
        };
        assert_eq!(message, &m.key());
        assert_eq!(attachments.len(), 2);
        let photo = &attachments[0];
        assert_eq!(
            (
                photo.content_type.as_str(),
                photo.size,
                photo.width,
                photo.height,
                photo.is_voice_note
            ),
            ("image/jpeg", 1000, Some(640), Some(480), false)
        );
        assert_eq!(photo.file_name.as_deref(), Some("f.bin"));
        // the stored pointer decodes back to what came in
        assert_eq!(
            AttachmentPointer::decode(photo.pointer.as_slice()).unwrap(),
            dm.attachments[0]
        );
        assert!(attachments[1].is_voice_note);
        let preview = preview.as_ref().unwrap();
        assert_eq!(
            (preview.url.as_str(), preview.title.as_deref()),
            ("https://example.org", Some("Example"))
        );
        assert_eq!(preview.image.as_ref().unwrap().content_type, "image/png");
    }

    #[test]
    fn a_message_with_only_a_file_is_kept_with_it() {
        let dm = DataMessage {
            timestamp: Some(100),
            attachments: vec![AttachmentPointer {
                content_type: None,
                ..pointer("", 7, 0)
            }],
            ..Default::default()
        };
        let ops = project(&content(ALICE, 150, ContentBody::DataMessage(dm)), ME);
        assert!(matches!(&ops[0], Op::InsertMessage(m) if m.body.is_none()));
        let Op::AddAttachments {
            attachments, preview, ..
        } = &ops[1]
        else {
            panic!("{ops:?}")
        };
        assert_eq!(attachments[0].content_type, "application/octet-stream");
        assert_eq!(preview, &None);
        // a preview without a url is dropped
        let mut dm = text(101, "hi");
        dm.preview = vec![Preview::default()];
        assert_eq!(project(&content(ALICE, 151, ContentBody::DataMessage(dm)), ME).len(), 1);
    }

    fn placeholder(dm: DataMessage) -> MessageKind {
        let ops = project(&content(ALICE, 150, ContentBody::DataMessage(dm)), ME);
        match ops.as_slice() {
            // a poll brings its options too
            [Op::InsertMessage(m)] | [Op::InsertMessage(m), Op::PollCreated { .. }] => m.kind.clone(),
            _ => panic!("{ops:?}"),
        }
    }

    #[test]
    fn what_we_cant_show_yet_becomes_a_placeholder_not_nothing() {
        let base = || DataMessage {
            timestamp: Some(100),
            ..Default::default()
        };
        assert_eq!(
            placeholder(DataMessage {
                sticker: Some(data_message::Sticker {
                    emoji: Some("🙂".into()),
                    ..Default::default()
                }),
                ..base()
            }),
            MessageKind::Sticker {
                emoji: Some("🙂".into())
            }
        );
        let contact = data_message::Contact {
            name: Some(data_message::contact::Name {
                given_name: Some("Ada".into()),
                family_name: Some("Lovelace".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            placeholder(DataMessage {
                contact: vec![contact],
                ..base()
            }),
            MessageKind::Contact {
                name: Some("Ada Lovelace".into())
            }
        );
        assert_eq!(
            placeholder(DataMessage {
                payment: Some(Default::default()),
                ..base()
            }),
            MessageKind::Payment
        );
        assert_eq!(
            placeholder(DataMessage {
                poll_create: Some(data_message::PollCreate {
                    question: Some("Pizza?".into()),
                    ..Default::default()
                }),
                ..base()
            }),
            MessageKind::Poll {
                question: Some("Pizza?".into())
            }
        );
        assert_eq!(
            placeholder(DataMessage {
                gift_badge: Some(Default::default()),
                ..base()
            }),
            MessageKind::GiftBadge
        );
        // text stays normal
        assert_eq!(placeholder(text(100, "hi")), MessageKind::Normal);
    }

    #[test]
    fn disappearing_timers_come_with_messages_and_timer_changes() {
        let mut dm = text(100, "secret");
        dm.expire_timer = Some(3600);
        let m = one(project(&content(ALICE, 150, ContentBody::DataMessage(dm.clone())), ME));
        assert_eq!((m.expires_in, m.expires_at), (3600, None), "theirs start when read");
        let mine = one(project(&content(ME, 150, sent_sync(ALICE, dm)), ME));
        assert_eq!(mine.expires_at, Some(100 + 3_600_000), "ours start when sent");
        let update = DataMessage {
            timestamp: Some(200),
            flags: Some(data_message::Flags::ExpirationTimerUpdate as u32),
            expire_timer: Some(604_800),
            ..Default::default()
        };
        assert_eq!(
            project(&content(ALICE, 250, ContentBody::DataMessage(update)), ME),
            vec![Op::SetTimer {
                conversation_id: format!("c:{ALICE}"),
                author: ALICE.into(),
                seconds: 604_800,
                at: 200
            }]
        );
    }
}
