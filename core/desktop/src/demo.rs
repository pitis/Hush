//! Made-up chats for `--snapshot`: the screens render without a core or anyone's real messages.

use sig_core::projection::{
    BodyRange, Conversation, ConversationKind, FileSummary, Message, MessageKey, MessageKind, MessageStatus, Quote,
    Reaction, TextStyle,
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
    vec![family, ana, bogdan, note, club, stranger]
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

/// The "Family" group's newest messages, oldest first.
pub fn messages() -> Vec<Message> {
    let mut mention = message(BOGDAN, 26 * 60, "\u{FFFC} can you bring the big pot?");
    mention.ranges = vec![BodyRange {
        start: 0,
        length: 1,
        style: TextStyle::Mention { aci: ME.into() },
    }];
    let mut salad = message(ME, 26 * 60 - 3, "Sure! And I'll make the salad.");
    salad.quote = Some(Quote {
        author: BOGDAN.into(),
        sent_at: mention.sent_at,
        text: mention.body.clone(),
        ranges: mention.ranges.clone(),
    });
    let mut delivered = message(ME, 40, "On my way, 20 minutes");
    delivered.status = MessageStatus::Delivered;
    delivered.edited = true;
    let mut failed = message(ME, 3, "Running a bit late");
    failed.status = MessageStatus::Failed;
    let mut sending = message(ME, 1, "Dinner at 8? I'll bring dessert 🍰");
    sending.status = MessageStatus::Pending;
    let mut deleted = message(ANA, 30, "");
    deleted.deleted = true;
    deleted.body = None;
    vec![
        message(ANA, 26 * 60 + 5, "Who's coming on Sunday?"),
        mention,
        salad,
        message(ANA, 45, "Great, everyone's in 🎉"),
        message(ANA, 44, "Mum says the table is booked for 8, the usual place near the park. Parking is tricky, so come early or take the tram."),
        delivered,
        deleted,
        message(BOGDAN, 5, "Traffic is terrible today"),
        failed,
        sending,
    ]
}

/// Reactions to "everyone's in" (ours among them) and to the salad.
pub fn reactions(messages: &[Message]) -> Vec<(MessageKey, Vec<Reaction>)> {
    let react = |reactor: &str, emoji: &str| Reaction {
        reactor: reactor.into(),
        emoji: emoji.into(),
    };
    let to = |body: &str| {
        messages
            .iter()
            .find(|m| m.body.as_deref() == Some(body))
            .map(Message::key)
    };
    let everyone = [react(BOGDAN, "❤️"), react(ME, "❤️"), react(ANA, "🎉")];
    let salad = [react(ANA, "👍")];
    [
        ("Great, everyone's in 🎉", everyone.to_vec()),
        ("Sure! And I'll make the salad.", salad.to_vec()),
    ]
    .into_iter()
    .filter_map(|(body, r)| Some((to(body)?, r)))
    .collect()
}
