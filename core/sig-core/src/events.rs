use crate::projection::db::Applied;
use crate::projection::MessageKey;

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

#[cfg(test)]
mod tests {
    use super::*;

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
