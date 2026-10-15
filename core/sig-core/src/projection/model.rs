#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConversationKind {
    Direct,
    Group,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct Conversation {
    pub id: String,
    pub kind: ConversationKind,
    pub title: String,
    pub last_message_at: i64,
    pub last_message_preview: String,
    pub unread_count: u32,
}

/// Delivery state of a message; ordered so a later state never gets downgraded.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MessageStatus {
    Failed,
    Pending,
    Sent,
    Delivered,
    Read,
}

impl MessageStatus {
    pub fn to_i64(self) -> i64 {
        match self {
            Self::Failed => -1,
            Self::Pending => 0,
            Self::Sent => 1,
            Self::Delivered => 2,
            Self::Read => 3,
        }
    }

    pub fn from_i64(v: i64) -> Self {
        match v {
            -1 => Self::Failed,
            0 => Self::Pending,
            1 => Self::Sent,
            2 => Self::Delivered,
            _ => Self::Read,
        }
    }
}

#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct Quote {
    pub author: String,
    pub sent_at: i64,
    pub text: Option<String>,
}

/// A message is identified by `(author, sent_at)`, as in the Signal protocol.
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
