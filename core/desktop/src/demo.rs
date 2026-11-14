//! Made-up chats for `--snapshot`: the screens render without a core or anyone's real messages.

use sig_core::projection::{
    Attachment, AttachmentState, BodyRange, Conversation, ConversationKind, FileSummary, Message, MessageKey,
    MessageKind, MessageStatus, Quote, Reaction, TextStyle,
};

pub const ME: &str = "00000000-0000-4000-8000-000000000000";
const ANA: &str = "11111111-1111-4111-8111-111111111111";
pub const BOGDAN: &str = "22222222-2222-4222-8222-222222222222";
const HOUR: i64 = 3_600_000;

pub fn names() -> Vec<(String, String)> {
    [(ANA, "Ana Popescu"), (BOGDAN, "Bogdan Ionescu")]
        .into_iter()
        .map(|(aci, name)| (aci.to_owned(), name.to_owned()))
        .collect()
}

/// The Family group's members but us.
pub fn members() -> Vec<String> {
    vec![ANA.into(), BOGDAN.into()]
}

/// What `Hush::demo_compose` types: "Thanks" bold, a mention of Ana, "big pot" italic.
pub fn compose_ranges() -> Vec<BodyRange> {
    vec![
        styled(0, 6, TextStyle::Bold),
        BodyRange {
            start: 7,
            length: 1,
            style: TextStyle::Mention { aci: ANA.into() },
        },
        styled(25, 7, TextStyle::Italic),
    ]
}

fn styled(start: u32, length: u32, style: TextStyle) -> BodyRange {
    BodyRange { start, length, style }
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
    delivered.ranges = vec![styled(11, 10, TextStyle::Monospace)];
    let mut booked = message(ANA, 44, "Mum says the table is booked for 8, the usual place near the park. Parking is tricky, so come early or take the tram.");
    booked.ranges = vec![styled(33, 1, TextStyle::Bold), styled(36, 15, TextStyle::Italic)];
    let mut surprise = message(ANA, 42, "Don't tell Mum: the surprise is a cake from Dulce 🎂");
    surprise.ranges = vec![styled(32, 20, TextStyle::Spoiler)];
    let mut traffic = message(BOGDAN, 5, "Traffic is terrible today");
    traffic.ranges = vec![styled(11, 8, TextStyle::Strikethrough)];
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
        booked,
        surprise,
        delivered,
        deleted,
        traffic,
        failed,
        sending,
    ]
}

/// Who sent the messages with files in Bogdan's chat, and how many minutes ago, oldest first.
const FILES_AGO: [(&str, i64); 7] = [
    (BOGDAN, 90),
    (BOGDAN, 80),
    (BOGDAN, 70),
    (BOGDAN, 60),
    (BOGDAN, 50),
    (BOGDAN, 40),
    (ME, 20),
];

/// Bogdan's chat: a voice message, a file, files still to download, on their way or failed, a
/// captioned photo, and one of ours (the pictures last, where the timeline opens).
pub fn file_messages() -> Vec<Message> {
    let captions = [None, None, None, None, None, Some("The view from the hotel"), None];
    FILES_AGO
        .iter()
        .zip(captions)
        .map(|(&(author, ago), caption)| {
            let mut m = message(author, ago, caption.unwrap_or_default());
            m.conversation_id = format!("c:{BOGDAN}");
            m.body = caption.map(Into::into);
            m
        })
        .collect()
}

/// The files of `file_messages`, one each.
pub fn files(messages: &[Message]) -> Vec<Attachment> {
    let file = |i: usize, content_type: &str, name: Option<&str>, size: u64, state: AttachmentState| Attachment {
        id: i as i64 + 1,
        message: messages[i].key(),
        content_type: content_type.into(),
        file_name: name.map(Into::into),
        size,
        width: None,
        height: None,
        is_voice_note: false,
        path: (state == AttachmentState::Ready).then(|| format!("demo-{i}")),
        state,
    };
    let mut voice = file(0, "audio/aac", None, 160_000, AttachmentState::Ready);
    voice.is_voice_note = true;
    let mut later = file(2, "image/jpeg", None, 3_100_000, AttachmentState::Pending);
    (later.width, later.height) = (Some(4032), Some(3024));
    let mut hotel = file(5, "image/jpeg", None, 2_400_000, AttachmentState::Ready);
    (hotel.width, hotel.height) = (Some(1600), Some(1200));
    let mut ours = file(6, "image/jpeg", None, 1_800_000, AttachmentState::Ready);
    (ours.width, ours.height) = (Some(1200), Some(800));
    vec![
        voice,
        file(
            1,
            "application/pdf",
            Some("Menu.pdf"),
            1_234_567,
            AttachmentState::Ready,
        ),
        later,
        file(
            3,
            "video/mp4",
            Some("Beach.mp4"),
            18_500_000,
            AttachmentState::Downloading,
        ),
        file(
            4,
            "application/zip",
            Some("Tickets.zip"),
            340_000,
            AttachmentState::Failed,
        ),
        hotel,
        ours,
    ]
}

/// A made-up photo, as PNG: a dusk sky over the sea.
pub fn picture() -> Vec<u8> {
    let sky = image::RgbaImage::from_fn(800, 600, |_, y| {
        let t = y as f32 / 600.;
        let (top, bottom) = if t < 0.62 {
            ([64., 92., 170.], [244., 164., 120.])
        } else {
            ([40., 70., 110.], [20., 36., 60.])
        };
        let t = if t < 0.62 { t / 0.62 } else { (t - 0.62) / 0.38 };
        let mix = |i: usize| (top[i] + (bottom[i] - top[i]) * t) as u8;
        image::Rgba([mix(0), mix(1), mix(2), 255])
    });
    let mut out = std::io::Cursor::new(vec![]);
    sky.write_to(&mut out, image::ImageFormat::Png)
        .expect("a picture in memory is written");
    out.into_inner()
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
