//! What the screen says, ported from the Mac app (`MessageText.swift`, `ChatList.swift`,
//! `Placeholder.swift`, `Files.swift`, `AppStore.swift`'s titles) so both clients word things
//! alike. Times and delivery states are simpler than the Mac app's.

use sig_core::projection::{
    BodyRange, Conversation, ConversationKind, FileSummary, Message, MessageKind, MessageStatus, TextStyle,
};

const HIDDEN_SPOILER: &str = "■■■■";

/// `body` as plain text: mentions read "@name", spoilers are hidden. Ranges count UTF-16 units,
/// as the phone sends them; one that doesn't fit the text is skipped, except a spoiler, which
/// hides up to the end of the text.
pub fn plain(body: &str, ranges: &[BodyRange], name: impl Fn(&str) -> String) -> String {
    let units: Vec<u16> = body.encode_utf16().collect();
    let span = |r: &BodyRange| (r.start as usize, r.start as usize + r.length as usize);
    let spoilers: Vec<(usize, usize, Option<&str>)> = ranges
        .iter()
        .filter(|r| r.style == TextStyle::Spoiler && (r.start as usize) < units.len())
        .map(|r| (span(r).0, span(r).1.min(units.len()), None))
        .collect();
    // a mention a hidden spoiler covers goes with it
    let mentions = ranges.iter().filter_map(|r| match &r.style {
        TextStyle::Mention { aci } => Some((span(r).0, span(r).1, Some(aci.as_str()))),
        _ => None,
    });
    let mentions = mentions.filter(|&(s, e, _)| !spoilers.iter().any(|&(ss, se, _)| s < se && ss < e));
    let mut replaced: Vec<_> = spoilers.iter().copied().chain(mentions).collect();
    // last first; a range overlapping one already replaced is skipped
    replaced.sort_by_key(|r| std::cmp::Reverse(r.0));
    let fits = |i: usize| i == units.len() || !(0xDC00..=0xDFFF).contains(&units[i]);
    let mut text = units.clone();
    let mut end = units.len();
    for (start, stop, aci) in replaced {
        if stop > end || stop == start || !fits(start) || !fits(stop) {
            continue;
        }
        let with = aci.map_or_else(|| HIDDEN_SPOILER.to_string(), |aci| format!("@{}", name(aci)));
        text.splice(start..stop, with.encode_utf16());
        end = start;
    }
    String::from_utf16_lossy(&text)
}

/// A chat's name ("Note to Self" for our own), or until it is known "Group", the contact's
/// "@username", or a short form of its id.
pub fn title(c: &Conversation, own_aci: Option<&str>) -> String {
    if own_aci.is_some_and(|me| c.id.strip_prefix("c:") == Some(me)) {
        return "Note to Self".into();
    }
    if !c.title.is_empty() {
        return c.title.clone();
    }
    if c.kind == ConversationKind::Group {
        return "Group".into();
    }
    match &c.username {
        Some(username) => format!("@{username}"),
        None => short_id(c.id.get(2..).unwrap_or_default()),
    }
}

pub fn short_id(aci: &str) -> String {
    aci.chars().take(8).collect()
}

/// A chat row's second line. `author` is the last message's author's name.
pub fn preview(c: &Conversation, author: &str, name: impl Fn(&str) -> String) -> String {
    // what the chat is, not what was said in it
    if c.blocked {
        return "Blocked".into();
    }
    if c.group_invitation {
        return "Group invitation".into();
    }
    if c.group_join_requested {
        return "Asked to join".into();
    }
    if c.message_request {
        return "Message request".into();
    }
    if c.last_message_at <= 0 {
        return String::new();
    }
    // the core's notice; the older message's author and files don't belong to it
    if c.last_message_undecryptable {
        return c.last_message_preview.clone();
    }
    let author = if c.last_message_outgoing { "You" } else { author };
    if c.last_message_deleted {
        return format!("{author} deleted this message");
    }
    let text = plain(&c.last_message_preview, &c.last_message_ranges, name);
    let text = if let Some(placeholder) = placeholder(&c.last_message_kind) {
        placeholder
    } else if let Some(file) = &c.last_message_file {
        one_line(&describe(file, &text))
    } else if text.is_empty() {
        "Attachment".into()
    } else {
        one_line(&text)
    };
    match c.kind {
        ConversationKind::Group => format!("{author}: {text}"),
        ConversationKind::Direct => text,
    }
}

/// A message's text in the timeline.
pub fn message(m: &Message, name: impl Fn(&str) -> String) -> String {
    if m.deleted {
        return "This message was deleted".into();
    }
    if let Some(placeholder) = placeholder(&m.kind) {
        return placeholder;
    }
    match m.body.as_deref() {
        Some(body) if !body.trim().is_empty() => plain(body, &m.ranges, name),
        // shortcut: files aren't shown, and a captioned one shows only its caption: `Message`
        // carries no file, so show them with `Client::attachments`
        _ => "📎 Attachment".into(),
    }
}

/// When something happened (`ms` since the epoch): the time today, else the day.
pub fn time(ms: i64) -> String {
    use chrono::{Datelike, Local, TimeZone};
    let Some(at) = Local.timestamp_millis_opt(ms).single() else {
        return String::new();
    };
    let now = Local::now();
    let format = match () {
        _ if at.date_naive() == now.date_naive() => "%H:%M",
        _ if at.year() == now.year() => "%b %-d",
        _ => "%Y-%m-%d",
    };
    at.format(format).to_string()
}

/// What our own message's delivery says.
pub fn status(s: MessageStatus) -> &'static str {
    match s {
        MessageStatus::Failed => "Not sent",
        MessageStatus::Pending => "Sending",
        MessageStatus::Sent => "Sent",
        MessageStatus::Delivered => "Delivered",
        MessageStatus::Read => "Read",
    }
}

/// A placeholder message's text, or None for a normal one (Signal Desktop's words).
fn placeholder(kind: &MessageKind) -> Option<String> {
    Some(match kind {
        MessageKind::Normal => return None,
        MessageKind::Sticker { emoji } => match emoji {
            Some(emoji) => format!("{emoji} Sticker message"),
            None => "Sticker message".into(),
        },
        MessageKind::Contact { name } => format!("👤 {}", name.as_deref().unwrap_or("Contact")),
        MessageKind::Payment => "💳 Payment".into(),
        MessageKind::Poll { question } => format!("📊 Poll: {}", question.as_deref().unwrap_or_default()),
        MessageKind::GiftBadge => "✨ Gift badge".into(),
        MessageKind::ViewOnce { .. } => "View-once media".into(),
    })
}

fn describe(file: &FileSummary, caption: &str) -> String {
    let t = file.content_type.as_str();
    let (emoji, word) = match t {
        "image/gif" => ("🎡", "GIF"),
        _ if t.starts_with("image/") => ("📷", "Photo"),
        _ if t.starts_with("video/") => ("🎥", "Video"),
        _ if file.is_voice_note => ("🎤", "Voice Message"),
        _ if t.starts_with("audio/") => ("🔈", "Audio Message"),
        _ => ("📎", "File"),
    };
    let caption = caption.trim();
    format!("{emoji} {}", if caption.is_empty() { word } else { caption })
}

/// Lines joined by spaces; every Unicode line break counts, as in Swift's `isNewline`.
fn one_line(text: &str) -> String {
    let newline = |c: char| {
        matches!(
            c,
            '\n' | '\r' | '\u{0B}' | '\u{0C}' | '\u{85}' | '\u{2028}' | '\u{2029}'
        )
    };
    text.split(newline)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mention(start: u32, aci: &str) -> BodyRange {
        BodyRange {
            start,
            length: 1,
            style: TextStyle::Mention { aci: aci.into() },
        }
    }

    fn name(aci: &str) -> String {
        format!("<{aci}>")
    }

    #[test]
    fn mentions_read_as_names_after_wide_characters() {
        // 👋 is two UTF-16 units: the mention starts at unit 3
        let body = "👋 \u{FFFC} and \u{FFFC}";
        let ranges = [mention(3, "a"), mention(9, "b")];
        assert_eq!(plain(body, &ranges, name), "👋 @<a> and @<b>");
    }

    #[test]
    fn spoilers_are_hidden_with_the_mentions_inside_them() {
        let spoiler = BodyRange {
            start: 4,
            length: 5,
            style: TextStyle::Spoiler,
        };
        let body = "the \u{FFFC} did it";
        assert_eq!(plain(body, &[spoiler, mention(4, "a")], name), "the ■■■■ it");
    }

    fn spoiler(start: u32, length: u32) -> BodyRange {
        BodyRange {
            start,
            length,
            style: TextStyle::Spoiler,
        }
    }

    #[test]
    fn a_spoiler_hides_a_mention_in_its_middle_and_runs_to_the_end_at_most() {
        assert_eq!(plain("a \u{FFFC} b", &[spoiler(0, 5), mention(2, "bob")], name), "■■■■");
        assert_eq!(plain("hi there", &[spoiler(3, 50)], name), "hi ■■■■");
    }

    #[test]
    fn hostile_ranges_dont_crash() {
        let huge = [
            mention(1, "a"),
            BodyRange {
                start: 1,
                length: u32::MAX,
                style: TextStyle::Mention { aci: "a".into() },
            },
            BodyRange {
                start: u32::MAX,
                length: u32::MAX,
                style: TextStyle::Mention { aci: "a".into() },
            },
            spoiler(u32::MAX, 1),
            spoiler(1, u32::MAX),
        ];
        for r in huge {
            plain("a\u{FFFC}", std::slice::from_ref(&r), name);
        }
    }

    #[test]
    fn ranges_splitting_an_emoji_leave_it_whole() {
        for body in ["👍", "👍👍 hi", "🇷🇴🇩🇪", "👨‍👩‍👧 z", "a👍b"] {
            for start in 0..6 {
                for length in 0..4 {
                    for r in [mention(start, "a"), spoiler(start, length)] {
                        let r = BodyRange { length, ..r };
                        let out = plain(body, &[r], name);
                        assert!(!out.contains('\u{FFFD}'), "{body:?} {start} {length}: {out:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn every_line_break_joins_the_preview_into_one_line() {
        assert_eq!(one_line("a\r\nb\u{2028}c\rd"), "a b c d");
    }

    #[test]
    fn ranges_that_dont_fit_the_text_are_skipped() {
        let past_the_end = mention(10, "a");
        // inside 👋: it would split the surrogate pair
        let splitting = mention(1, "b");
        assert_eq!(plain("👋 hi", &[past_the_end, splitting], name), "👋 hi");
    }

    fn chat(kind: ConversationKind, preview: &str) -> Conversation {
        Conversation {
            id: "g:00".into(),
            kind,
            title: String::new(),
            last_message_at: 1,
            last_message_preview: preview.into(),
            last_message_ranges: vec![],
            unread_count: 0,
            last_message_author: "a".into(),
            last_message_outgoing: false,
            last_message_status: MessageStatus::Read,
            last_message_deleted: false,
            last_message_file: None,
            expire_timer: 0,
            last_message_kind: MessageKind::Normal,
            last_message_undecryptable: false,
            archived: false,
            pinned_order: None,
            mute_until: 0,
            marked_unread: false,
            blocked: false,
            whitelisted: true,
            message_request: false,
            group_invitation: false,
            group_join_requested: false,
            avatar: None,
            username: None,
            note: None,
            chat_color: None,
            wallpaper: None,
        }
    }

    #[test]
    fn group_previews_say_who_wrote_and_files_say_what_they_are() {
        let mut c = chat(ConversationKind::Group, "line one\nline two");
        assert_eq!(preview(&c, "Ana", name), "Ana: line one line two");
        c.last_message_outgoing = true;
        c.last_message_file = Some(FileSummary {
            content_type: "image/jpeg".into(),
            file_name: None,
            is_voice_note: false,
        });
        c.last_message_preview = String::new();
        assert_eq!(preview(&c, "Ana", name), "You: 📷 Photo");
    }

    #[test]
    fn what_a_chat_is_comes_before_what_was_said() {
        let mut c = chat(ConversationKind::Direct, "hello");
        c.message_request = true;
        assert_eq!(preview(&c, "Ana", name), "Message request");
    }

    #[test]
    fn our_own_chat_is_note_to_self() {
        let mut c = chat(ConversationKind::Direct, "");
        c.id = "c:me".into();
        assert_eq!(title(&c, Some("me")), "Note to Self");
        assert_eq!(title(&c, Some("someone")), "me");
    }
}
