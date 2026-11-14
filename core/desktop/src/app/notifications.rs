//! Notifications of new messages and of reactions to ours, and the unread count on the Dock or the
//! launcher (the Mac app's `Notifications.swift`, `AppStore.notify` and `ChatList.badge`), through
//! GPUI's system notifications. What GPUI offers is less than the Mac app's Notification Center:
//! no reply typed into a notification (a click opens its chat, ready to type), no sound, and on
//! Linux nothing is taken back once shown.

use std::time::Duration;

use async_compat::Compat;
use gpui_kit::*;
use sig_core::projection::{Conversation, FileSummary, Message, MessageKey};
use sig_core::{Client, CoreError};

use super::{files, mentioned, Hush};
use crate::text;

/// Who this app is to the system: the Windows AppUserModelID, the Wayland app id, and the
/// launcher's `.desktop` file (`dev.pitis.HushDesktop.desktop`).
pub const APP_ID: &str = "dev.pitis.HushDesktop";
/// The notification's button that marks its chat read.
const MARK_READ: &str = "mark-read";

/// One notification on show, by its tag.
#[derive(Debug)]
struct Shown {
    tag: String,
    chat: String,
    /// The message it is about (ours, for a reaction).
    key: MessageKey,
    /// A reaction doesn't count as unread: reading the chat leaves it.
    reaction: bool,
}

/// The notifications shown since the app started, and the unread count set.
#[derive(Default)]
pub(super) struct Notifications {
    shown: Vec<Shown>,
    /// What the Dock or launcher shows.
    badge: u32,
    /// Counts again when the next timed mute ends: nothing else says so.
    mute_ends: Option<Task<()>>,
}

impl Notifications {
    /// A newer one with the same tag replaces it, as on screen.
    fn add(&mut self, shown: Shown) {
        self.shown.retain(|s| s.tag != shown.tag);
        self.shown.push(shown);
    }

    /// Takes the notifications `gone` picks; their tags.
    fn take(&mut self, gone: impl Fn(&Shown) -> bool) -> Vec<String> {
        let (taken, kept) = std::mem::take(&mut self.shown).into_iter().partition(gone);
        self.shown = kept;
        taken.into_iter().map(|s: Shown| s.tag).collect()
    }

    /// Everything shown about the chat `id`, reactions too: it was opened, deleted or marked read.
    fn of_chat(&mut self, id: &str) -> Vec<String> {
        self.take(|s| s.chat == id)
    }

    /// The message's notification and those of reactions to it: it was deleted or disappeared.
    fn of_message(&mut self, key: &MessageKey) -> Vec<String> {
        self.take(|s| s.key == *key)
    }

    /// Those of each chat read now (here or on the phone) that has one about a message
    /// (`AppStore.conversations`).
    fn of_read(&mut self, chats: &[Conversation]) -> Vec<String> {
        let read: Vec<&str> = chats
            .iter()
            .filter(|c| c.unread_count == 0)
            .map(|c| c.id.as_str())
            .filter(|id| self.shown.iter().any(|s| s.chat == *id && !s.reaction))
            .collect();
        self.take(|s| read.contains(&s.chat.as_str()))
    }

    fn chat_of(&self, tag: &str) -> Option<String> {
        self.shown.iter().find(|s| s.tag == tag).map(|s| s.chat.clone())
    }

    fn about(&self, key: &MessageKey) -> bool {
        self.shown.iter().any(|s| s.key == *key)
    }
}

/// What a notification needs from the core: the message, and what to call it.
struct About {
    tag: String,
    message: Message,
    file: Option<FileSummary>,
    /// The group's name, in a group.
    group: Option<String>,
    /// The names of the sender and the people mentioned, as the core has them now.
    names: Vec<(String, String)>,
}

/// The message `key` of chat `chat`, unless the chat is muted or the message is gone; the tag
/// names no one (`Notifications.id`): the system keeps it.
async fn about(
    client: &Client,
    chat: &str,
    key: &MessageKey,
    reaction: Option<&(String, String)>,
) -> Result<Option<About>, CoreError> {
    // asked of the core now: the list here can lag a mute by a moment
    if client.is_muted(chat.to_owned()).await? {
        return Ok(None);
    }
    let Some(message) = client.message(key.clone()).await?.filter(|m| !m.deleted) else {
        return Ok(None);
    };
    // one whose files don't read is still told, with its text
    let file = client.attachments(key.clone()).await.ok();
    let file = file.and_then(|f| f.first().map(files::summary));
    let group = match chat.starts_with("g:") {
        true => Some(client.title(chat.to_owned()).await?.unwrap_or_else(|| "Group".into())),
        false => None,
    };
    let sender = reaction.map_or(&message.author, |(reactor, _)| reactor);
    let mut names = vec![];
    // one not found reads as a short id
    for aci in std::iter::once(sender.clone()).chain(mentioned(&message.ranges)) {
        if let Ok(Some(name)) = client.title(format!("c:{aci}")).await {
            names.push((aci, name));
        }
    }
    let mut tag = format!("{}|{}|", client.opaque_id(key.author.clone()), key.sent_at);
    if let Some((reactor, emoji)) = reaction {
        tag.push_str(&format!("{}|{emoji}", client.opaque_id(reactor.clone())));
    }
    Ok(Some(About {
        tag,
        message,
        file,
        group,
        names,
    }))
}

impl Hush {
    /// In front, on screen: the chat list shows what's new, and notifications stay quiet (as in
    /// Signal Desktop).
    fn in_front(&self) -> bool {
        self.window_active && self.window_visible
    }

    /// Notifies of a message come in to `chat`, or of a reaction (reactor, emoji) to our message
    /// `key` there (`AppStore.notify`).
    pub(super) fn notify(
        &mut self,
        chat: String,
        key: MessageKey,
        reaction: Option<(String, String)>,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.client.clone() else { return };
        if self.in_front() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let found = Compat::new(about(&client, &chat, &key, reaction.as_ref())).await;
            let _ = this.update(cx, |app, cx| match found {
                Ok(Some(about)) => app.show(chat, key, reaction, about, cx),
                Ok(None) => {}
                // ids and errors only
                Err(e) => tracing::warn!("a notification failed: {e}"),
            });
        })
        .detach();
    }

    fn show(
        &mut self,
        chat: String,
        key: MessageKey,
        reaction: Option<(String, String)>,
        about: About,
        cx: &mut Context<Self>,
    ) {
        self.names.extend(about.names);
        let name = |aci: &str| self.name(aci);
        let text = message_text(&about.message, about.file.as_ref(), name);
        let sender = name(reaction.as_ref().map_or(&key.author, |(reactor, _)| reactor));
        let emoji = reaction.as_ref().map(|(_, emoji)| emoji.as_str());
        let (title, body) = words(&sender, about.group.as_deref(), text.as_deref(), emoji);
        let shown = Shown {
            tag: about.tag,
            chat,
            key,
            reaction: reaction.is_some(),
        };
        self.post(shown, title, body, cx);
    }

    /// Shows a notification, unless the window came to the front meanwhile.
    fn post(&mut self, shown: Shown, title: String, body: String, cx: &mut Context<Self>) {
        if self.in_front() {
            return;
        }
        cx.show_system_notification(SystemNotification {
            tag: shown.tag.clone().into(),
            title: title.into(),
            body: body.into(),
            actions: vec![SystemNotificationAction {
                id: MARK_READ.into(),
                label: "Mark as Read".into(),
            }],
        });
        self.notifications.add(shown);
    }

    fn take_back(tags: Vec<String>, cx: &mut Context<Self>) {
        for tag in tags {
            cx.dismiss_system_notification(&tag);
        }
    }

    /// The chat `id` was opened or deleted: what was shown about it goes.
    pub(super) fn chat_seen(&mut self, id: &str, cx: &mut Context<Self>) {
        Self::take_back(self.notifications.of_chat(id), cx);
    }

    /// The message disappeared: nothing of it may stay readable on screen.
    pub(super) fn message_gone(&mut self, key: &MessageKey, cx: &mut Context<Self>) {
        Self::take_back(self.notifications.of_message(key), cx);
    }

    /// A message changed: if one shown was deleted for everyone, its notification goes.
    pub(super) fn recheck_notified(&mut self, key: &MessageKey, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone().filter(|_| self.notifications.about(key)) else {
            return;
        };
        let key = key.clone();
        cx.spawn(async move |this, cx| {
            // one that didn't read stays: that isn't a deletion
            let Ok(now) = Compat::new(client.message(key.clone())).await else {
                return;
            };
            if now.is_none_or(|m| m.deleted) {
                let _ = this.update(cx, |app, cx| app.message_gone(&key, cx));
            }
        })
        .detach();
    }

    /// The chat list changed: the chats read take their notifications with them, and the unread
    /// count is set again, and again when a timed mute ends.
    pub(super) fn chats_counted(&mut self, cx: &mut Context<Self>) {
        Self::take_back(self.notifications.of_read(&self.chats), cx);
        let now = chrono::Utc::now().timestamp_millis();
        let count = badge(&self.chats, now);
        if count != self.notifications.badge {
            self.notifications.badge = count;
            show_badge(count);
        }
        self.notifications.mute_ends = next_mute_end(&self.chats, now).map(|end| {
            cx.spawn(async move |this, cx| {
                let wait = Duration::from_millis(end.saturating_sub(now) as u64 + 1);
                cx.background_executor().timer(wait).await;
                // the row's bell is drawn again too
                let _ = this.update(cx, |app, cx| {
                    app.chats_counted(cx);
                    cx.notify();
                });
            })
        });
    }

    /// A click on a notification opens its chat, ready to type; its Mark as Read marks it read.
    fn used(&mut self, response: SystemNotificationResponse, window: &mut Window, cx: &mut Context<Self>) {
        // one from before the app last started: its chat isn't known
        let chat = self
            .notifications
            .chat_of(&response.tag)
            .filter(|id| self.chats.iter().any(|c| c.id == *id));
        match (response.action_id.as_deref(), chat) {
            (Some(MARK_READ), Some(id)) => {
                self.chat_seen(&id, cx);
                self.change(
                    "mark the chat read",
                    cx,
                    |c| async move { c.mark_read(id).await.map(drop) },
                );
            }
            (Some(MARK_READ), None) => {}
            (_, chat) => {
                cx.activate(true);
                window.activate_window();
                if let Some(id) = chat {
                    self.select(id, window, cx);
                }
            }
        }
    }
}

/// Clicks on notifications and their buttons go to the window's chats.
pub fn responses(window: AnyWindowHandle, hush: WeakEntity<Hush>, cx: &mut App) {
    cx.on_system_notification_response(move |response, cx| {
        let _ = window.update(cx, |_, window, cx| {
            hush.update(cx, |app, cx| app.used(response, window, cx))
        });
    });
}

/// The Dock's badge. Only from the main thread: AppKit's, which tests don't run on.
#[cfg(target_os = "macos")]
fn show_badge(count: u32) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;
    use objc2_foundation::NSString;
    let Some(main) = MainThreadMarker::new() else { return };
    let label = (count > 0).then(|| NSString::from_str(&count.to_string()));
    NSApplication::sharedApplication(main)
        .dockTile()
        .setBadgeLabel(label.as_deref());
}

/// Windows shows a count only for apps installed as a package (this one isn't): nothing.
/// shortcut: Signal Desktop draws the count as the taskbar button's overlay icon; do that
/// (`ITaskbarList3::SetOverlayIcon`) if the taskbar should show it.
#[cfg(windows)]
fn show_badge(_: u32) {}

/// The launcher's count through the Unity LauncherEntry signal (KDE Plasma, Ubuntu's dock, Dash
/// to Dock), for the launcher whose `.desktop` file is `APP_ID`'s. Sent from a thread of its own,
/// in order, over one connection: a launcher drops the count of a sender that leaves the bus.
#[cfg(not(any(target_os = "macos", windows)))]
fn show_badge(count: u32) {
    use std::collections::HashMap;
    use std::sync::mpsc::{channel, Sender};
    use std::sync::OnceLock;
    use zbus::zvariant::Value;

    static COUNTS: OnceLock<Option<Sender<u32>>> = OnceLock::new();
    let counts = COUNTS.get_or_init(|| {
        let (counts, sent) = channel::<u32>();
        let started = std::thread::Builder::new()
            .name("launcher-count".into())
            .spawn(move || {
                let bus = match zbus::blocking::Connection::session() {
                    Ok(bus) => bus,
                    Err(e) => return tracing::warn!("no unread count on the launcher: {e}"),
                };
                let uri = format!("application://{APP_ID}.desktop");
                for count in sent {
                    let properties = HashMap::from([
                        ("count", Value::from(i64::from(count))),
                        ("count-visible", Value::from(count > 0)),
                    ]);
                    let path = "/dev/pitis/HushDesktop";
                    let update = (uri.as_str(), properties);
                    let sent = bus.emit_signal(
                        None::<&str>,
                        path,
                        "com.canonical.Unity.LauncherEntry",
                        "Update",
                        &update,
                    );
                    if let Err(e) = sent {
                        tracing::warn!("the launcher's unread count failed: {e}");
                    }
                }
            });
        started
            .map_err(|e| tracing::warn!("no unread count on the launcher: {e}"))
            .ok()
            .map(|_| counts)
    });
    if let Some(counts) = counts {
        // the thread ended: said when it did
        let _ = counts.send(count);
    }
}

/// The unread count, as Desktop counts it: unread messages in the chats that aren't archived or
/// muted, plus one per chat marked unread (`ChatList.badge`).
fn badge(chats: &[Conversation], now_ms: i64) -> u32 {
    chats
        .iter()
        .filter(|c| !c.archived && c.mute_until <= now_ms)
        .map(|c| match c.unread_count {
            0 => u32::from(c.marked_unread),
            n => n,
        })
        .sum()
}

/// When the next timed mute ends (`AppStore.recountWhenAMuteEnds`).
fn next_mute_end(chats: &[Conversation], now_ms: i64) -> Option<i64> {
    chats
        .iter()
        .map(|c| c.mute_until)
        .filter(|&until| until > now_ms && until < i64::MAX)
        .min()
}

/// A message's words in a notification: its file's kind (and caption), a placeholder's text, or
/// its text with mentions as "@name"; None for nothing to show (`AppStore.notify`).
fn message_text(m: &Message, file: Option<&FileSummary>, name: impl Fn(&str) -> String) -> Option<String> {
    let body = m.body.as_deref().filter(|b| !b.trim().is_empty());
    let body = body.map(|b| text::plain(b, &m.ranges, name));
    match file {
        Some(file) => Some(text::describe(file, body.as_deref().unwrap_or_default())),
        None => text::placeholder(&m.kind).or(body),
    }
}

/// A notification's title and body as Signal Desktop words them, with name and content shown
/// (`Notifications.request`); `emoji`: a reaction by `sender` to our message.
fn words(sender: &str, group: Option<&str>, text: Option<&str>, emoji: Option<&str>) -> (String, String) {
    let title = match group {
        Some(group) => format!("{sender} in {group}"),
        None => sender.to_owned(),
    };
    let text = text.unwrap_or("Attachment");
    let body = match emoji {
        Some(emoji) => format!("{sender} reacted {emoji} to: {text}"),
        None => text.to_owned(),
    };
    (title, body)
}

#[cfg(test)]
mod tests {
    use super::{badge, message_text, next_mute_end, words, Notifications, Shown};
    use sig_core::projection::{
        BodyRange, Conversation, FileSummary, Message, MessageKey, MessageKind, MessageStatus, TextStyle,
    };

    const NOW: i64 = 1_000_000;

    fn chat(id: &str, unread: u32) -> Conversation {
        Conversation {
            unread_count: unread,
            ..crate::app::tests::chat(id, "")
        }
    }

    fn key(author: &str, sent_at: i64) -> MessageKey {
        MessageKey {
            author: author.into(),
            sent_at,
        }
    }

    fn shown(tag: &str, chat: &str, key: MessageKey, reaction: bool) -> Shown {
        Shown {
            tag: tag.into(),
            chat: chat.into(),
            key,
            reaction,
        }
    }

    #[test]
    fn the_badge_counts_unread_messages_in_chats_neither_archived_nor_muted() {
        let archived = Conversation {
            archived: true,
            ..chat("c:a", 5)
        };
        let muted = Conversation {
            mute_until: NOW + 1,
            ..chat("c:m", 7)
        };
        let mute_over = Conversation {
            mute_until: NOW,
            ..chat("c:o", 2)
        };
        let marked = Conversation {
            marked_unread: true,
            ..chat("c:k", 0)
        };
        // marked unread with unread messages counts them, not one more
        let both = Conversation {
            marked_unread: true,
            ..chat("c:b", 4)
        };
        let all = [chat("c:1", 3), archived, muted, mute_over, marked, both, chat("c:0", 0)];
        assert_eq!(badge(&all, NOW), 3 + 2 + 1 + 4);
        assert_eq!(badge(&[], NOW), 0);
    }

    #[test]
    fn the_badge_is_counted_again_when_the_next_timed_mute_ends() {
        let muted = |id: &str, until: i64| Conversation {
            mute_until: until,
            ..chat(id, 1)
        };
        let all = [
            muted("c:1", NOW + 500),
            muted("c:2", NOW + 100),
            muted("c:forever", i64::MAX),
            muted("c:over", NOW),
            chat("c:3", 1),
        ];
        assert_eq!(next_mute_end(&all, NOW), Some(NOW + 100));
        assert_eq!(next_mute_end(&all[2..], NOW), None, "for good, or over");
    }

    #[test]
    fn a_notification_is_taken_back_when_its_chat_opens_or_its_message_goes() {
        let mut n = Notifications::default();
        n.add(shown("m1", "c:ana", key("ana", 1), false));
        n.add(shown("r1", "c:ana", key("me", 2), true));
        n.add(shown("m2", "g:family", key("ana", 3), false));
        n.add(shown("r2", "g:family", key("me", 4), true));
        n.add(shown("r3", "g:family", key("me", 4), true));
        assert_eq!(n.chat_of("m2").as_deref(), Some("g:family"));
        assert_eq!(n.chat_of("gone"), None);

        // a reaction to our message goes with it
        assert!(n.about(&key("me", 4)));
        assert_eq!(n.of_message(&key("me", 4)), ["r2", "r3"]);
        assert!(!n.about(&key("me", 4)));
        // opening a chat takes back its reactions too
        assert_eq!(n.of_chat("c:ana"), ["m1", "r1"]);
        assert!(n.of_chat("c:ana").is_empty(), "once");
        assert_eq!(n.chat_of("m1"), None);
        assert_eq!(n.of_chat("g:family"), ["m2"]);
    }

    #[test]
    fn a_newer_notification_with_the_same_tag_replaces_the_older() {
        let mut n = Notifications::default();
        n.add(shown("r", "c:ana", key("me", 1), true));
        n.add(shown("r", "c:ana", key("me", 1), true));
        assert_eq!(n.of_chat("c:ana"), ["r"]);
    }

    #[test]
    fn reading_a_chat_takes_back_its_notifications_if_one_is_about_a_message() {
        let mut n = Notifications::default();
        n.add(shown("m", "c:ana", key("ana", 1), false));
        n.add(shown("r", "c:ana", key("me", 2), true));
        // reactions don't count as unread: a chat with only those has none
        n.add(shown("r2", "c:bo", key("me", 3), true));
        n.add(shown("m3", "g:family", key("ana", 4), false));
        let chats = [chat("c:ana", 0), chat("c:bo", 0), chat("g:family", 2)];
        assert_eq!(n.of_read(&chats), ["m", "r"]);
        assert!(n.of_read(&chats).is_empty());
        assert_eq!(n.of_read(&[chat("g:family", 0)]), ["m3"]);
        assert_eq!(n.of_chat("c:bo"), ["r2"], "left until the chat opens");
    }

    fn message(body: Option<&str>, kind: MessageKind) -> Message {
        Message {
            conversation_id: "c:ana".into(),
            author: "ana".into(),
            sent_at: 1,
            received_at: 1,
            body: body.map(Into::into),
            ranges: vec![],
            is_outgoing: false,
            status: MessageStatus::Delivered,
            edited: false,
            deleted: false,
            quote: None,
            kind,
            expires_in: 0,
            expires_at: None,
            story: None,
        }
    }

    #[test]
    fn a_message_reads_as_its_file_its_placeholder_or_its_text_with_names() {
        let name = |aci: &str| {
            if aci == "bo" {
                "Bogdan".to_owned()
            } else {
                aci.to_owned()
            }
        };
        let photo = FileSummary {
            content_type: "image/jpeg".into(),
            file_name: None,
            is_voice_note: false,
        };
        let mut hi = message(Some("hi \u{FFFC}"), MessageKind::Normal);
        hi.ranges = vec![BodyRange {
            start: 3,
            length: 1,
            style: TextStyle::Mention { aci: "bo".into() },
        }];
        assert_eq!(message_text(&hi, None, name).as_deref(), Some("hi @Bogdan"));
        assert_eq!(message_text(&hi, Some(&photo), name).as_deref(), Some("📷 hi @Bogdan"));
        let bare = message(None, MessageKind::Normal);
        assert_eq!(message_text(&bare, Some(&photo), name).as_deref(), Some("📷 Photo"));
        assert_eq!(message_text(&bare, None, name), None);
        assert_eq!(
            message_text(&message(Some("  "), MessageKind::Normal), None, name),
            None
        );
        let poll = MessageKind::Poll {
            question: Some("Pizza?".into()),
        };
        assert_eq!(
            message_text(&message(None, poll), None, name).as_deref(),
            Some("📊 Poll: Pizza?")
        );
    }

    #[test]
    fn a_notification_names_the_sender_and_the_group_and_quotes_what_was_reacted_to() {
        assert_eq!(words("Ana", None, Some("hi"), None), ("Ana".into(), "hi".into()));
        assert_eq!(
            words("Ana", Some("Family"), None, None),
            ("Ana in Family".into(), "Attachment".into())
        );
        assert_eq!(
            words("Ana", Some("Family"), Some("Dinner at 8?"), Some("👍")),
            ("Ana in Family".into(), "Ana reacted 👍 to: Dinner at 8?".into())
        );
    }
}

/// Notifications in a real (headless) window, as GPUI's test platform records them:
/// `cargo test --features hush-desktop/snapshot`.
#[cfg(all(test, feature = "snapshot"))]
mod window_tests {
    use std::time::Duration;

    use gpui_kit::{gpui, AnyWindowHandle, AppContext as _, Entity, SystemNotificationResponse, TestAppContext};
    use gpui_kit::{Context, Window, WindowOptions};
    use sig_core::projection::MessageKey;
    use sig_core::CoreEvent;

    use super::super::attach::window_tests::with_core;
    use super::super::Hush;
    use super::{Shown, APP_ID, MARK_READ};
    use crate::demo::{ANA, ME};
    use crate::Demo;

    fn open(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Hush>) {
        let opened = cx.update(|cx| {
            gpui_kit::init(cx);
            // the test platform shows none without it, as Windows wouldn't
            cx.set_app_identity(APP_ID, "Hush");
            let (window, hush) = gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Hush::demo(Demo::Chat, window, cx))
            })
            .expect("the window opens");
            super::responses(window, hush.downgrade(), cx);
            (window, hush)
        });
        cx.run_until_parked();
        // behind other windows
        opened.1.update(cx, |app, _| app.window_active = false);
        opened
    }

    fn act(
        window: AnyWindowHandle,
        hush: &Entity<Hush>,
        cx: &mut TestAppContext,
        f: impl FnOnce(&mut Hush, &mut Window, &mut Context<Hush>),
    ) {
        window
            .update(cx, |_, window, cx| hush.update(cx, |app, cx| f(app, window, cx)))
            .expect("the window is open");
        cx.run_until_parked();
    }

    fn key(author: &str, sent_at: i64) -> MessageKey {
        MessageKey {
            author: author.into(),
            sent_at,
        }
    }

    /// Posts a notification as a message (or reaction) come in would.
    fn post(hush: &Entity<Hush>, tag: &str, chat: &str, key: MessageKey, reaction: bool, cx: &mut TestAppContext) {
        let shown = Shown {
            tag: tag.into(),
            chat: chat.into(),
            key,
            reaction,
        };
        hush.update(cx, |app, cx| app.post(shown, "Ana Popescu".into(), "hi".into(), cx));
    }

    fn on_show(cx: &TestAppContext) -> Vec<String> {
        cx.delivered_system_notifications()
            .into_iter()
            .map(|n| n.tag.to_string())
            .collect()
    }

    fn click(tag: &str, action: Option<&str>, cx: &mut TestAppContext) {
        cx.simulate_system_notification_response(SystemNotificationResponse {
            tag: tag.to_owned().into(),
            action_id: action.map(|a| a.to_owned().into()),
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn none_shows_while_the_window_is_in_front_and_a_click_opens_its_chat(cx: &mut TestAppContext) {
        let (_, hush) = open(cx);
        let ana = format!("c:{ANA}");
        hush.update(cx, |app, _| app.window_active = true);
        post(&hush, "front", &ana, key(ANA, 1), false, cx);
        assert!(cx.shown_system_notifications().is_empty(), "in front: quiet");

        hush.update(cx, |app, _| app.window_active = false);
        post(&hush, "away", &ana, key(ANA, 2), false, cx);
        let shown = cx.shown_system_notifications();
        assert_eq!(shown.len(), 1);
        assert_eq!((shown[0].title.as_ref(), shown[0].body.as_ref()), ("Ana Popescu", "hi"));
        let actions: Vec<_> = shown[0].actions.iter().map(|a| a.label.to_string()).collect();
        assert_eq!(actions, ["Mark as Read"]);

        click("away", None, cx);
        assert_eq!(hush.read_with(cx, |app, _| app.selected.clone()), Some(ana));
        assert!(on_show(cx).is_empty(), "opening the chat took it back");
        // one from before the app started names no chat it knows
        click("unknown", None, cx);
        assert_eq!(
            hush.read_with(cx, |app, _| app.selected.clone()),
            Some(format!("c:{ANA}"))
        );
    }

    #[gpui::test]
    fn reading_a_chat_or_its_message_disappearing_takes_its_notification_back(cx: &mut TestAppContext) {
        let (window, hush) = open(cx);
        // Family (open, 3 unread) and a reaction to our message in Book club
        post(&hush, "family", "g:01", key(ANA, 1), false, cx);
        post(&hush, "club", "g:02", key(ME, 2), true, cx);
        let mut chats = crate::demo::chats();
        act(window, &hush, cx, |app, _, cx| app.set_chats(chats.clone(), cx));
        assert_eq!(on_show(cx), ["family", "club"]);
        assert_eq!(hush.read_with(cx, |app, _| app.notifications.badge), 3 + 12);

        // read on the phone
        chats[0].unread_count = 0;
        act(window, &hush, cx, |app, _, cx| app.set_chats(chats.clone(), cx));
        assert_eq!(on_show(cx), ["club"]);
        assert_eq!(hush.read_with(cx, |app, _| app.notifications.badge), 12);

        let expired = CoreEvent::MessageExpired {
            conversation_id: "g:02".into(),
            key: key(ME, 2),
        };
        hush.update(cx, |app, cx| app.handle(expired, cx));
        assert!(on_show(cx).is_empty());
    }

    #[gpui::test]
    fn mark_as_read_asks_the_core_and_takes_the_chats_notifications_back(cx: &mut TestAppContext) {
        let (_, hush) = open(cx);
        let data = std::env::temp_dir().join(format!("hush-desktop-notifications-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        with_core(&hush, &data, cx);
        cx.executor().allow_parking();
        let ana = format!("c:{ANA}");
        post(&hush, "m", &ana, key(ANA, 1), false, cx);
        post(&hush, "r", &ana, key(ME, 2), true, cx);
        click("m", Some(MARK_READ), cx);
        assert!(on_show(cx).is_empty());
        assert_eq!(
            hush.read_with(cx, |app, _| app.selected.clone()),
            Some("g:01".into()),
            "not opened"
        );
        // a core that isn't linked refuses: the asking shows
        let mut said = None;
        for _ in 0..200 {
            cx.run_until_parked();
            said = hush.update(cx, |app, _| app.error.take());
            if said.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(said.unwrap_or_default().starts_with("Couldn't mark the chat read"));
        std::fs::remove_dir_all(&data).unwrap();
    }
}
