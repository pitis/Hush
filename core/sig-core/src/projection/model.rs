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
    /// The last message's first file, so the list can say "Photo", "Voice Message"…
    pub last_message_file: Option<FileSummary>,
}

/// What kind of file a message has (its first one).
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct FileSummary {
    pub content_type: String,
    pub file_name: Option<String>,
    pub is_voice_note: bool,
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

/// Something that happened in a chat that isn't a message.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatEventKind {
    /// A message from `author` arrived but couldn't be decrypted (a broken session).
    DecryptionFailed,
}

impl ChatEventKind {
    pub fn to_i64(self) -> i64 {
        match self {
            Self::DecryptionFailed => 0,
        }
    }

    pub fn from_i64(v: i64) -> Option<Self> {
        (v == 0).then_some(Self::DecryptionFailed)
    }
}

/// Shown in a chat's timeline among its messages, at `at`.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct ChatEvent {
    pub id: i64,
    pub conversation_id: String,
    pub at: i64,
    pub kind: ChatEventKind,
    pub author: String,
}

/// Marks the matched words in a search snippet (Unicode private-use characters, so message text
/// can't contain them by accident).
pub const HIGHLIGHT_START: char = '\u{E000}';
pub const HIGHLIGHT_END: char = '\u{E001}';

/// A message found by search, with the text around the match (matches between
/// `HIGHLIGHT_START` and `HIGHLIGHT_END`).
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct SearchHit {
    pub message: Message,
    pub snippet: String,
}

/// Where a message's file is. Incoming files start `Pending` (not downloaded).
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentState {
    Pending,
    Downloading,
    Ready,
    Failed,
}

impl AttachmentState {
    pub fn to_i64(self) -> i64 {
        match self {
            Self::Pending => 0,
            Self::Downloading => 1,
            Self::Ready => 2,
            Self::Failed => 3,
        }
    }

    pub fn from_i64(v: i64) -> Option<Self> {
        [Self::Pending, Self::Downloading, Self::Ready, Self::Failed]
            .into_iter()
            .find(|s| s.to_i64() == v)
    }
}

/// A file of a message (a photo, voice note, document…).
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct Attachment {
    pub id: i64,
    pub message: MessageKey,
    pub content_type: String,
    pub file_name: Option<String>,
    pub size: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub is_voice_note: bool,
    /// The file on disk, once `Ready`.
    pub path: Option<String>,
    pub state: AttachmentState,
}

/// A received link preview (title, url, image), shown with its message.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct LinkPreview {
    pub url: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub image: Option<Attachment>,
}

/// A file to send, described by the app (it knows types and image sizes).
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct OutgoingFile {
    pub path: String,
    pub content_type: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub is_voice_note: bool,
}

/// An attachment as a message carries it, before it is stored.
#[derive(Clone, Debug, PartialEq)]
pub struct NewAttachment {
    /// The encoded `AttachmentPointer` it is downloaded with.
    pub pointer: Vec<u8>,
    pub content_type: String,
    pub file_name: Option<String>,
    pub size: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub is_voice_note: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NewPreview {
    pub url: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub image: Option<NewAttachment>,
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
