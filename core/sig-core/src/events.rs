use crate::projection::db::Applied;
use crate::projection::{MessageKey, Op};

#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum ConnectionState {
    Connecting,
    Online,
    Offline,
    /// Not linked to a phone (never linked, or linking failed).
    Unlinked,
}

/// Everything the core tells the app, delivered on the core thread through `EventListener`.
#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum CoreEvent {
    /// Show this `sgnl://linkdevice?…` URL as a QR code for the phone to scan.
    LinkUrl {
        url: String,
    },
    Linked,
    /// The server queue is drained: the app is caught up.
    Ready,
    ConversationsChanged,
    MessageChanged {
        conversation_id: String,
        key: MessageKey,
    },
    Typing {
        conversation_id: String,
        who: String,
        started: bool,
    },
    /// Someone's message arrived while we are live (not while catching up after being offline):
    /// worth a notification. Also comes with `MessageChanged`.
    NewMessage {
        conversation_id: String,
        key: MessageKey,
    },
    /// Someone reacted to one of our messages, live.
    NewReaction {
        conversation_id: String,
        target: MessageKey,
        reactor: String,
        emoji: String,
    },
    Connection {
        state: ConnectionState,
    },
    Error {
        message: String,
    },
}

#[uniffi::export(with_foreign)]
pub trait EventListener: Send + Sync {
    fn on_event(&self, event: CoreEvent);
}

pub fn from_applied(a: Applied) -> CoreEvent {
    match a {
        Applied::MessageChanged { conversation_id, key } => CoreEvent::MessageChanged { conversation_id, key },
        Applied::Typing {
            conversation_id,
            who,
            started,
        } => CoreEvent::Typing {
            conversation_id,
            who,
            started,
        },
    }
}

/// The notification-worthy event of an op that was just applied (`applied` is what it changed),
/// if any: a new message from someone else, or someone's reaction to our message.
pub fn alert(op: &Op, applied: &[Applied], me: &str) -> Option<CoreEvent> {
    let conversation_id = applied.iter().find_map(|a| match a {
        Applied::MessageChanged { conversation_id, .. } => Some(conversation_id.clone()),
        Applied::Typing { .. } => None,
    })?;
    match op {
        Op::InsertMessage(m) if !m.is_outgoing && m.author != me => Some(CoreEvent::NewMessage {
            conversation_id,
            key: m.key(),
        }),
        Op::React {
            target,
            reactor,
            emoji: Some(emoji),
        } if target.author == me && reactor != me => Some(CoreEvent::NewReaction {
            conversation_id,
            target: target.clone(),
            reactor: reactor.clone(),
            emoji: emoji.clone(),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::{Message, MessageStatus, Op};

    const ME: &str = "me";

    fn incoming(author: &str, outgoing: bool) -> Op {
        Op::InsertMessage(Message {
            conversation_id: "c:alice".into(),
            author: author.into(),
            sent_at: 5,
            received_at: 5,
            body: Some("hi".into()),
            is_outgoing: outgoing,
            status: MessageStatus::Delivered,
            edited: false,
            deleted: false,
            quote: None,
        })
    }

    fn changed(sent_at: i64, author: &str) -> Vec<Applied> {
        vec![Applied::MessageChanged {
            conversation_id: "c:alice".into(),
            key: MessageKey {
                author: author.into(),
                sent_at,
            },
        }]
    }

    #[test]
    fn new_messages_from_others_alert_once_stored() {
        assert_eq!(
            alert(&incoming("alice", false), &changed(5, "alice"), ME),
            Some(CoreEvent::NewMessage {
                conversation_id: "c:alice".into(),
                key: MessageKey {
                    author: "alice".into(),
                    sent_at: 5
                }
            })
        );
        // a duplicate changed nothing; our own (sent from the phone) never alerts
        assert_eq!(alert(&incoming("alice", false), &[], ME), None);
        assert_eq!(alert(&incoming(ME, true), &changed(5, ME), ME), None);
    }

    #[test]
    fn reactions_alert_only_on_our_messages_by_others() {
        let react = |target: &str, reactor: &str, emoji: Option<&str>| Op::React {
            target: MessageKey {
                author: target.into(),
                sent_at: 1,
            },
            reactor: reactor.into(),
            emoji: emoji.map(Into::into),
        };
        assert_eq!(
            alert(&react(ME, "alice", Some("👍")), &changed(1, ME), ME),
            Some(CoreEvent::NewReaction {
                conversation_id: "c:alice".into(),
                target: MessageKey {
                    author: ME.into(),
                    sent_at: 1
                },
                reactor: "alice".into(),
                emoji: "👍".into()
            })
        );
        assert_eq!(alert(&react(ME, "alice", None), &changed(1, ME), ME), None);
        assert_eq!(alert(&react("bob", "alice", Some("👍")), &changed(1, "bob"), ME), None);
        assert_eq!(alert(&react(ME, ME, Some("👍")), &changed(1, ME), ME), None);
        assert_eq!(alert(&react(ME, "alice", Some("👍")), &[], ME), None);
    }

    #[test]
    fn applied_maps_to_events() {
        let key = MessageKey {
            author: "a".into(),
            sent_at: 1,
        };
        let e = from_applied(Applied::MessageChanged {
            conversation_id: "c:a".into(),
            key: key.clone(),
        });
        assert_eq!(
            e,
            CoreEvent::MessageChanged {
                conversation_id: "c:a".into(),
                key
            }
        );
        let e = from_applied(Applied::Typing {
            conversation_id: "c:a".into(),
            who: "a".into(),
            started: true,
        });
        assert_eq!(
            e,
            CoreEvent::Typing {
                conversation_id: "c:a".into(),
                who: "a".into(),
                started: true
            }
        );
    }
}
