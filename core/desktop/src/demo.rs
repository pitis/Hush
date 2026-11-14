//! Made-up chats for `--snapshot`: the screens render without a core or anyone's real messages.

use sig_core::projection::{
    BodyRange, Conversation, ConversationKind, FileSummary, Message, MessageKind, MessageStatus, SearchHit, TextStyle,
    HIGHLIGHT_END, HIGHLIGHT_START,
};

pub const ME: &str = "00000000-0000-4000-8000-000000000000";
const ANA: &str = "11111111-1111-4111-8111-111111111111";
const BOGDAN: &str = "22222222-2222-4222-8222-222222222222";
const HOUR: i64 = 3_600_000;

pub fn names() -> Vec<(String, String)> {
    [(ANA, "Ana Popescu"), (BOGDAN, "Bogdan Ionescu")]
        .into_iter()
        .map(|(aci, name)| (aci.to_owned(), name.to_owned()))
        .collect()
}

fn chat(id: &str, kind: ConversationKind, title: &str, ago_hours: i64, preview: &str) -> Conversation {
    let now = chrono::Utc::now().timestamp_millis();
    Conversation {
        id: id.into(),
        kind,
        title: title.into(),
        last_message_at: now - ago_hours * HOUR,
        last_message_preview: preview.into(),
        last_message_ranges: vec![],
        unread_count: 0,
        last_message_author: ANA.into(),
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

pub fn chats() -> Vec<Conversation> {
    let mut family = chat(
        "g:01",
        ConversationKind::Group,
        "Family",
        0,
        "Dinner at 8? I'll bring dessert 🍰",
    );
    family.unread_count = 3;
    family.pinned_order = Some(0);
    let ana = chat(
        &format!("c:{ANA}"),
        ConversationKind::Direct,
        "Ana Popescu",
        1,
        "See you tomorrow!",
    );
    let mut bogdan = chat(
        &format!("c:{BOGDAN}"),
        ConversationKind::Direct,
        "Bogdan Ionescu",
        5,
        "",
    );
    bogdan.last_message_outgoing = true;
    bogdan.last_message_file = Some(FileSummary {
        content_type: "image/jpeg".into(),
        file_name: None,
        is_voice_note: false,
    });
    let note = chat(
        &format!("c:{ME}"),
        ConversationKind::Direct,
        "",
        30,
        "Passport number is in the drawer",
    );
    let mut club = chat(
        "g:02",
        ConversationKind::Group,
        "Book club",
        80,
        "Next one: The Left Hand of Darkness",
    );
    club.last_message_author = BOGDAN.into();
    club.unread_count = 12;
    let mut stranger = chat(
        "c:33333333-3333-4333-8333-333333333333",
        ConversationKind::Direct,
        "",
        200,
        "Hi!",
    );
    stranger.message_request = true;
    stranger.username = Some("radu.42".into());
    // only a search shows it
    let mut swap = chat("g:03", ConversationKind::Group, "Book swap", 900, "Who has Dune?");
    swap.archived = true;
    vec![family, ana, bogdan, note, club, stranger, swap]
}

fn message(author: &str, ago_minutes: i64, body: &str) -> Message {
    let at = chrono::Utc::now().timestamp_millis() - ago_minutes * 60_000;
    Message {
        conversation_id: "g:01".into(),
        author: author.into(),
        sent_at: at,
        received_at: at,
        body: Some(body.into()),
        ranges: vec![],
        is_outgoing: author == ME,
        status: MessageStatus::Read,
        edited: false,
        deleted: false,
        quote: None,
        kind: MessageKind::Normal,
        expires_in: 0,
        expires_at: None,
        story: None,
    }
}

/// Messages found for "book" in every chat, newest first.
pub fn hits() -> Vec<SearchHit> {
    let hit = |chat: &str, author: &str, ago_minutes: i64, snippet: &str| {
        let mut message = message(author, ago_minutes, "");
        message.conversation_id = chat.into();
        SearchHit {
            message,
            snippet: snippet
                .replace('[', &HIGHLIGHT_START.to_string())
                .replace(']', &HIGHLIGHT_END.to_string()),
        }
    };
    vec![
        hit(
            "g:01",
            ANA,
            44,
            "…says the table is [book]ed for 8, the usual place near the park…",
        ),
        hit(&format!("c:{ANA}"), ME, 3 * 60, "I'll bring back your [book] tomorrow"),
        hit("g:02", BOGDAN, 80 * 60, "Next [book]: The Left Hand of Darkness"),
        hit(
            "g:03",
            ANA,
            900 * 60,
            "Anyone want my old [book]s? Dune, Solaris and a few more",
        ),
    ]
}

/// The messages with `word` in them, newest first, as the core finds them.
pub fn found(messages: &[Message], word: &str) -> Vec<SearchHit> {
    let marked = format!("{HIGHLIGHT_START}{word}{HIGHLIGHT_END}");
    let with = messages
        .iter()
        .rev()
        .filter(|m| m.body.as_deref().is_some_and(|b| b.contains(word)));
    with.map(|m| SearchHit {
        message: m.clone(),
        snippet: m.body.clone().unwrap_or_default().replace(word, &marked),
    })
    .collect()
}

/// The "Family" group's newest messages, oldest first.
pub fn messages() -> Vec<Message> {
    let mut mention = message(BOGDAN, 26 * 60, "\u{FFFC} can you bring the big pot?");
    mention.ranges = vec![BodyRange {
        start: 0,
        length: 1,
        style: TextStyle::Mention { aci: ME.into() },
    }];
    let mut delivered = message(ME, 40, "On my way, 20 minutes");
    delivered.status = MessageStatus::Delivered;
    let mut sending = message(ME, 1, "Dinner at 8? I'll bring dessert 🍰");
    sending.status = MessageStatus::Pending;
    let mut deleted = message(ANA, 30, "");
    deleted.deleted = true;
    deleted.body = None;
    vec![
        message(ANA, 26 * 60 + 5, "Who's coming on Sunday?"),
        mention,
        message(ME, 26 * 60 - 3, "Sure! And I'll make the salad."),
        message(ANA, 45, "Great, everyone's in 🎉"),
        message(ANA, 44, "Mum says the table is booked for 8, the usual place near the park. Parking is tricky, so come early or take the tram."),
        delivered,
        deleted,
        message(BOGDAN, 5, "Traffic is terrible today"),
        sending,
    ]
}
