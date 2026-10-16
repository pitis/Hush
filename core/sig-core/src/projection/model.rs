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
    /// The chat's disappearing-message timer in seconds (0: off).
    pub expire_timer: u32,
    /// What the last message is (placeholders are worded by the app).
    pub last_message_kind: MessageKind,
    /// The latest thing in the chat is a message that couldn't be decrypted (the preview says
    /// so; the other `last_message_*` fields are then empty).
    pub last_message_undecryptable: bool,
    /// Out of the chat list, behind "Archived".
    pub archived: bool,
    /// Pinned chats list first, in ascending order; None: not pinned.
    pub pinned_order: Option<u32>,
    /// No notifications until this time (ms since the epoch); 0: not muted; `i64::MAX`: always.
    pub mute_until: i64,
    /// Shown unread although every message is read ("Mark as unread").
    pub marked_unread: bool,
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

/// What a message is. `Normal` is text and/or files; the others have nothing we can show yet and
/// are shown as placeholders (worded like Signal Desktop's).
#[derive(uniffi::Enum, Clone, Debug, PartialEq)]
pub enum MessageKind {
    Normal,
    Sticker { emoji: Option<String> },
    Contact { name: Option<String> },
    Payment,
    Poll { question: Option<String> },
    GiftBadge,
}

impl MessageKind {
    /// Stored as a number and an optional text.
    pub fn to_row(&self) -> (i64, Option<&str>) {
        match self {
            Self::Normal => (0, None),
            Self::Sticker { emoji } => (1, emoji.as_deref()),
            Self::Contact { name } => (2, name.as_deref()),
            Self::Payment => (3, None),
            Self::Poll { question } => (4, question.as_deref()),
            Self::GiftBadge => (5, None),
        }
    }

    pub fn from_row(kind: i64, text: Option<String>) -> Option<Self> {
        Some(match kind {
            0 => Self::Normal,
            1 => Self::Sticker { emoji: text },
            2 => Self::Contact { name: text },
            3 => Self::Payment,
            4 => Self::Poll { question: text },
            5 => Self::GiftBadge,
            _ => return None,
        })
    }
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
    pub kind: MessageKind,
    /// Disappearing-message timer in seconds (0: it stays).
    pub expires_in: u32,
    /// When it disappears (ms), once its timer started: when read, or when we sent it.
    pub expires_at: Option<i64>,
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
    /// A message between us and `author` couldn't be decrypted, by us or by them (presage
    /// doesn't tell which): a broken session.
    DecryptionFailed,
    /// We started a new secure session with `author`.
    SessionReset,
    /// `author` set the disappearing-message timer to `value` seconds (0: off).
    TimerChanged,
    /// We called `author`; `value` is the call's seconds (NULL: never answered).
    OutgoingCall,
    /// `author` called us and we talked; `value` is the call's seconds.
    IncomingCall,
    /// `author` called us and we never connected (unanswered or declined).
    MissedCall,
}

impl ChatEventKind {
    pub const ALL: [Self; 6] = [
        Self::DecryptionFailed,
        Self::SessionReset,
        Self::TimerChanged,
        Self::OutgoingCall,
        Self::IncomingCall,
        Self::MissedCall,
    ];

    pub fn to_i64(self) -> i64 {
        match self {
            Self::DecryptionFailed => 0,
            Self::SessionReset => 1,
            Self::TimerChanged => 2,
            Self::OutgoingCall => 3,
            Self::IncomingCall => 4,
            Self::MissedCall => 5,
        }
    }

    pub fn from_i64(v: i64) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.to_i64() == v)
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
    /// The new timer of a `TimerChanged` (seconds).
    pub value: Option<i64>,
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
    fn call_event_kinds_round_trip() {
        for (kind, n) in [
            (ChatEventKind::OutgoingCall, 3),
            (ChatEventKind::IncomingCall, 4),
            (ChatEventKind::MissedCall, 5),
        ] {
            assert_eq!(kind.to_i64(), n);
            assert_eq!(ChatEventKind::from_i64(n), Some(kind));
        }
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
