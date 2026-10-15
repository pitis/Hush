#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConversationKind {
    Direct,
    Group,
}

impl ConversationKind {
    /// The kind encoded in a conversation id: `c:<aci uuid>` is direct, `g:<hex master key>` is a group.
    pub fn from_id(id: &str) -> Option<Self> {
        match id.get(..2) {
            Some("c:") if id.len() > 2 => Some(Self::Direct),
            Some("g:") if id.len() > 2 => Some(Self::Group),
            _ => None,
        }
    }
}

#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct Conversation {
    pub id: String,
    pub kind: ConversationKind,
    /// Empty when the name is not known yet.
    pub title: String,
    pub last_message_at: i64,
    pub last_message_preview: String,
    pub unread_count: u32,
    /// The last message's author (empty without messages).
    pub last_message_author: String,
    pub last_message_outgoing: bool,
    /// Meaningful for our own last message: its ticks.
    pub last_message_status: MessageStatus,
    pub last_message_deleted: bool,
}

/// Delivery state of a message. The variant order matches the stored integers, and a later state
/// never gets downgraded (receipts apply `MAX`). `Failed` is only reached from `Pending`.
/// For incoming messages, `Read` means read on this account.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MessageStatus {
    Failed,
    Pending,
    Sent,
    Delivered,
    Read,
}

impl MessageStatus {
    pub const ALL: [Self; 5] = [Self::Failed, Self::Pending, Self::Sent, Self::Delivered, Self::Read];

    pub fn to_i64(self) -> i64 {
        match self {
            Self::Failed => -1,
            Self::Pending => 0,
            Self::Sent => 1,
            Self::Delivered => 2,
            Self::Read => 3,
        }
    }

    pub fn from_i64(v: i64) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.to_i64() == v)
    }
}

#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct Quote {
    pub author: String,
    pub sent_at: i64,
    pub text: Option<String>,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct Reaction {
    pub reactor: String,
    pub emoji: String,
}

/// Identity of a message, and the paging cursor for `messages`.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct MessageKey {
    pub author: String,
    pub sent_at: i64,
}

/// A message, identified by `(author, sent_at)`. Own messages use our own ACI as `author`.
/// A deleted message has no `body` (and no quote text).
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct Message {
    pub conversation_id: String,
    pub author: String,
    pub sent_at: i64,
    pub received_at: i64,
    pub body: Option<String>,
    pub is_outgoing: bool,
    pub status: MessageStatus,
    pub edited: bool,
    pub deleted: bool,
    pub quote: Option<Quote>,
}

impl Message {
    pub fn key(&self) -> MessageKey {
        MessageKey {
            author: self.author.clone(),
            sent_at: self.sent_at,
        }
    }
}

/// Privacy settings synced from the phone; `None` until it has told us.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    pub read_receipts: Option<bool>,
    pub typing_indicators: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_order_matches_stored_integers_and_round_trips() {
        for a in MessageStatus::ALL {
            assert_eq!(MessageStatus::from_i64(a.to_i64()), Some(a));
            for b in MessageStatus::ALL {
                assert_eq!(a < b, a.to_i64() < b.to_i64(), "{a:?} vs {b:?}");
            }
        }
        assert_eq!(MessageStatus::from_i64(4), None);
        assert_eq!(MessageStatus::from_i64(-2), None);
    }

    #[test]
    fn conversation_kind_from_id() {
        assert_eq!(ConversationKind::from_id("c:abc"), Some(ConversationKind::Direct));
        assert_eq!(ConversationKind::from_id("g:00ff"), Some(ConversationKind::Group));
        for bad in ["", "c:", "g:", "G:00", "x:1", "c", "é:1"] {
            assert_eq!(ConversationKind::from_id(bad), None, "{bad:?}");
        }
    }
}
