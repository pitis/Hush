use serde::{Deserialize, Serialize};

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
    /// The preview's styles and mentions (the last message's `ranges`).
    pub last_message_ranges: Vec<BodyRange>,
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
    /// Blocked: nothing it sends is shown.
    pub blocked: bool,
    /// The message request was accepted / profile sharing is on.
    pub whitelisted: bool,
    /// Not accepted yet: the app offers Accept, Delete and Block instead of the composer, and no
    /// read receipts go out. Only once the phone's account state has been read (before that,
    /// every chat counts as accepted).
    pub message_request: bool,
    /// The chat's picture file (a contact's profile avatar, a group's), when it has one.
    pub avatar: Option<String>,
    /// A contact's username, when known.
    pub username: Option<String>,
    /// The note written about a contact on the phone.
    pub note: Option<String>,
    /// Our bubbles' colour in this chat (its own, else the default set on the phone); None: the
    /// app's.
    pub chat_color: Option<ChatColor>,
}

/// A bubble colour: one colour (solid) or a gradient of several, at `angle` degrees (0: bottom to
/// top, 90: left to right, as in Signal's backups). Colours are 0xAARRGGBB.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct ChatColor {
    pub colors: Vec<u32>,
    pub angle: u32,
}

impl ChatColor {
    /// As stored: "<angle>:<AARRGGBB>[,<AARRGGBB>…]".
    pub fn to_row(&self) -> String {
        let colors: Vec<String> = self.colors.iter().map(|c| format!("{c:08X}")).collect();
        format!("{}:{}", self.angle, colors.join(","))
    }

    /// None for anything `to_row` doesn't write (and for no colours).
    pub fn from_row(row: &str) -> Option<Self> {
        let (angle, colors) = row.split_once(':')?;
        let colors = colors
            .split(',')
            .map(|c| u32::from_str_radix(c, 16).ok())
            .collect::<Option<Vec<_>>>()?;
        (!colors.is_empty()).then_some(Self {
            colors,
            angle: angle.parse().ok()?,
        })
    }
}

/// An answer to a message request, ours or one our phone reports
/// (`SyncMessage.MessageRequestResponse`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestResponse {
    Accept,
    Delete,
    Block,
    BlockAndDelete,
}

/// The chat a request response names: a contact's ACI, or a group by its identifier (derived
/// from the master key; see `ids::group_identifier`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestThread {
    Aci(String),
    Group(Vec<u8>),
}

/// The per-chat values the Storage Service carries, in our units (`mute_until` ms; `i64::MAX`
/// for good).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ChatFlags {
    pub archived: bool,
    pub marked_unread: bool,
    pub mute_until: i64,
    pub blocked: bool,
    pub whitelisted: bool,
}

/// The Storage Service manifest we last applied, and what is waiting to be written.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct StorageState {
    pub version: u64,
    pub record_ikm: Vec<u8>,
    /// SHA-256 of the storage key this state belongs to.
    pub key_fingerprint: Vec<u8>,
    /// A pin change not yet written.
    pub account_dirty: bool,
    /// Pinned-list entries we couldn't map (raw `PinnedConversation` bytes), carried unchanged.
    pub unmappable_pins: Vec<Vec<u8>>,
}

/// One identifier of the applied manifest: its key, record type, and the chat it maps to when
/// we understood the record.
#[derive(Clone, Debug, PartialEq)]
pub struct StorageItemRow {
    pub key: Vec<u8>,
    pub kind: i32,
    pub conversation_id: Option<String>,
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
    /// Styles and mentions in `text`.
    pub ranges: Vec<BodyRange>,
}

/// A style or a mention over part of a message's text (Signal's `BodyRange`). `start` and
/// `length` count UTF-16 code units, as Signal sends them. A mention covers a placeholder
/// character (U+FFFC) that the app shows as "@name".
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BodyRange {
    pub start: u32,
    pub length: u32,
    pub style: TextStyle,
}

#[derive(uniffi::Enum, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextStyle {
    Bold,
    Italic,
    Spoiler,
    Strikethrough,
    Monospace,
    Mention { aci: String },
}

impl TextStyle {
    /// A style by its number in Signal's protos (the service's and the backup's agree); `None`
    /// for NONE and unknown ones.
    pub(crate) fn from_proto(style: i32) -> Option<Self> {
        Some(match style {
            1 => Self::Bold,
            2 => Self::Italic,
            3 => Self::Spoiler,
            4 => Self::Strikethrough,
            5 => Self::Monospace,
            _ => return None,
        })
    }
}

/// How ranges are stored: JSON, NULL when there are none.
pub(crate) fn ranges_to_row(ranges: &[BodyRange]) -> Option<String> {
    (!ranges.is_empty()).then(|| serde_json::to_string(ranges).expect("ranges serialize"))
}

/// A row's ranges; unreadable JSON reads as none (the text still shows, unstyled), logged.
pub(crate) fn ranges_from_row(row: Option<String>) -> Vec<BodyRange> {
    let Some(row) = row else { return vec![] };
    serde_json::from_str(&row).unwrap_or_else(|e| {
        // the error names a JSON position or variant, never message text
        tracing::warn!("stored text ranges unreadable, shown unstyled: {e}");
        vec![]
    })
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
    Sticker {
        emoji: Option<String>,
    },
    Contact {
        name: Option<String>,
    },
    Payment,
    Poll {
        question: Option<String>,
    },
    GiftBadge,
    /// A photo or video to open once; `viewed` once opened (or ours, which we can't open).
    ViewOnce {
        viewed: bool,
    },
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
            Self::ViewOnce { viewed } => (6, viewed.then_some("viewed")),
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
            6 => Self::ViewOnce { viewed: text.is_some() },
            _ => return None,
        })
    }
}

/// A story: text, or one photo or video, shown for a day.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct Story {
    /// The author's chat, the group's, or ours (`c:<our aci>`) for our own.
    pub conversation_id: String,
    pub author: String,
    pub sent_at: i64,
    /// A text story's words and look (None for a photo or video).
    pub text: Option<StoryText>,
    /// A photo or video story's file.
    pub file: Option<Attachment>,
    pub viewed: bool,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct StoryText {
    pub text: String,
    /// Signal's TextAttachment.Style: 0 default, 1 regular, 2 bold, 3 serif, 4 script, 5 condensed.
    pub style: u32,
    /// ARGB colours.
    pub foreground: Option<u32>,
    pub text_background: Option<u32>,
    /// One colour, or a gradient's colours (empty: the default).
    pub background: Vec<u32>,
    /// The gradient's angle in degrees (0: bottom to top).
    pub angle: u32,
    pub link: Option<String>,
}

/// A story as it arrives (its file, if any, follows as `Op::AddAttachments`).
#[derive(Clone, Debug, PartialEq)]
pub struct NewStory {
    pub conversation_id: String,
    pub author: String,
    pub sent_at: i64,
    pub received_at: i64,
    pub text: Option<StoryText>,
    /// Ours (posted from the phone): shown whatever its chat.
    pub outgoing: bool,
}

/// A poll as its votes stand: each option with who chose it.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct Poll {
    pub options: Vec<PollOption>,
    pub allow_multiple: bool,
    pub ended: bool,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct PollOption {
    pub text: String,
    /// ACIs, sorted.
    pub voters: Vec<String>,
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
    /// Styles and mentions in `body`.
    pub ranges: Vec<BodyRange>,
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

/// An installed sticker pack, for the picker: its id (hex) and the stickers we have images of.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct StickerPackInfo {
    pub id: String,
    pub title: String,
    pub author: String,
    pub stickers: Vec<StickerInfo>,
}

/// A sticker in a pack: its id in the pack, its emoji, and its image as a file.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct StickerInfo {
    pub id: u32,
    pub emoji: Option<String>,
    pub path: String,
}

/// A link preview to send with a message: its link (which must be in the message's text), what
/// its page says, and the page's image as a file to send.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct PreviewDraft {
    pub url: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub image: Option<OutgoingFile>,
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
    /// Unknown until the phone says: no page is fetched for a preview until then.
    pub link_previews: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rows already stored must keep reading: a renamed variant or field breaks this, not history.
    #[test]
    fn stored_ranges_keep_their_format() {
        let stored = r#"[{"start":0,"length":1,"style":{"Mention":{"aci":"a"}}},{"start":1,"length":2,"style":"Bold"},{"start":3,"length":1,"style":"Italic"},{"start":4,"length":1,"style":"Spoiler"},{"start":5,"length":1,"style":"Strikethrough"},{"start":6,"length":1,"style":"Monospace"}]"#;
        let ranges = ranges_from_row(Some(stored.into()));
        assert_eq!(
            ranges.iter().map(|r| r.style.clone()).collect::<Vec<_>>(),
            vec![
                TextStyle::Mention { aci: "a".into() },
                TextStyle::Bold,
                TextStyle::Italic,
                TextStyle::Spoiler,
                TextStyle::Strikethrough,
                TextStyle::Monospace
            ]
        );
        assert_eq!(ranges_to_row(&ranges).as_deref(), Some(stored));
        assert_eq!(ranges_from_row(Some("not json".into())), vec![]);
        assert_eq!(ranges_to_row(&[]), None);
    }

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
