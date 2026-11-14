//! Notifications of new messages and of reactions to ours, and the unread count on the Dock or the
//! launcher (the Mac app's `Notifications.swift`, `AppStore.notify` and `ChatList.badge`), through
//! GPUI's system notifications. What GPUI offers is less than the Mac app's Notification Center:
//! no reply typed into a notification (a click opens its chat, ready to type), no sound, on macOS
//! none at all unless run from an app bundle, and on Linux nothing is taken back once shown.

use std::time::Duration;

use async_compat::Compat;
use gpui_kit::*;
use sig_core::projection::{Conversation, FileSummary, Message, MessageKey};
use sig_core::{Client, CoreError};

use super::settings::{self, NotificationContent};
use super::{files, mentioned, Hush};
use crate::text;

/// Who this app is to the system: the Windows AppUserModelID, the Linux window's app id (Wayland)
/// and WM_CLASS (X11), and the name a launcher's `.desktop` file needs for the unread count
/// (`dev.pitis.HushDesktop.desktop`; none is shipped yet).
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
    /// A reaction doesn't make its chat unread: reading the chat takes it back only along with a
    /// message's (`of_read`).
    reaction: bool,
}

/// The notifications this run showed and hasn't taken back, the unread count last set, and the
/// wait for the next mute to end.
#[derive(Default)]
pub(super) struct Notifications {
    shown: Vec<Shown>,
    /// What the Dock or launcher shows.
    badge: u32,
    /// Counts again when the next timed mute ends (then): nothing else says so.
    mute_ends: Option<(i64, Task<()>)>,
}

impl Notifications {
    /// A newer one with the same tag replaces it, as on screen (not on Linux, where both stay).
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

    /// A notification's chat: one shown in this run, else one listed whose opaque id the tag
    /// starts with (macOS and Windows keep notifications from before the app last started).
    fn chat_named(&self, tag: &str, chats: &[Conversation], opaque: impl Fn(&str) -> String) -> Option<String> {
        let named = tag.split('|').next()?;
        self.chat_of(tag)
            .or_else(|| chats.iter().map(|c| &c.id).find(|id| opaque(id) == named).cloned())
            .filter(|id| chats.iter().any(|c| c.id == *id))
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

/// A notification's tag: its chat's, the message's and the reactor's opaque ids (`opaque`), as the
/// Mac app names them (`Notifications.id`, with the chat it keeps in `userInfo`): the system keeps
/// it, so it names no one.
fn tag(opaque: impl Fn(&str) -> String, chat: &str, key: &MessageKey, reaction: Option<&(String, String)>) -> String {
    let mut tag = format!("{}|{}|{}|", opaque(chat), opaque(&key.author), key.sent_at);
    if let Some((reactor, emoji)) = reaction {
        tag.push_str(&format!("{}|{emoji}", opaque(reactor)));
    }
    tag
}

/// The message `key` of chat `chat`, unless the chat is muted or the message is gone.
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
    let file = client.attachments(key.clone()).await;
    let file = file
        .inspect_err(|e| tracing::warn!(error = %e, "a notification's files didn't read"))
        .ok();
    let file = file.and_then(|f| f.first().map(files::summary));
    let group = match chat.starts_with("g:") {
        true => Some(client.title(chat.to_owned()).await?.unwrap_or_else(|| "Group".into())),
        false => None,
    };
    let sender = reaction.map_or(&message.author, |(reactor, _)| reactor);
    let mut names = vec![];
    // one not found reads as a short id
    for aci in std::iter::once(sender.clone()).chain(mentioned(&message.ranges)) {
        match client.title(format!("c:{aci}")).await {
            Ok(Some(name)) => names.push((aci, name)),
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, "a name in a notification didn't read"),
        }
    }
    Ok(Some(About {
        tag: tag(|id| client.opaque_id(id.to_owned()), chat, key, reaction),
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
        // off: nothing is read for one (`words` says none too, for a change made meanwhile)
        if self.in_front() || settings::chosen::<NotificationContent>(cx) == NotificationContent::Off {
            return;
        }
        cx.spawn(async move |this, cx| {
            let found = Compat::new(about(&client, &chat, &key, reaction.as_ref())).await;
            let _ = this.update(cx, |app, cx| match found {
                Ok(Some(about)) => app.show(chat, key, reaction, about, cx),
                Ok(None) => {}
                // ids and errors only
                Err(e) => tracing::warn!(error = %e, "a notification failed"),
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
        let setting = settings::chosen::<NotificationContent>(cx);
        let Some((title, body)) = words(setting, &sender, about.group.as_deref(), text.as_deref(), emoji) else {
            return;
        };
        let shown = Shown {
            tag: about.tag,
            chat,
            key,
            reaction: reaction.is_some(),
        };
        self.post(shown, title, body, setting == NotificationContent::NameAndMessage, cx);
    }

    /// Shows a notification, with Mark as Read if `actions` (only with its content shown), unless
    /// the window came to the front meanwhile.
    fn post(&mut self, shown: Shown, title: String, body: String, actions: bool, cx: &mut Context<Self>) {
        if self.in_front() {
            return;
        }
        let mark_read = SystemNotificationAction {
            id: MARK_READ.into(),
            label: "Mark as Read".into(),
        };
        cx.show_system_notification(SystemNotification {
            tag: shown.tag.clone().into(),
            title: title.into(),
            body: body.into(),
            actions: if actions { vec![mark_read] } else { vec![] },
        });
        self.notifications.add(shown);
    }

    fn take_back(tags: Vec<String>, cx: &mut Context<Self>) {
        for tag in tags {
            cx.dismiss_system_notification(&tag);
        }
    }

    /// The chat `id` was opened, deleted or marked read from a notification: what was shown about
    /// it goes.
    pub(super) fn chat_seen(&mut self, id: &str, cx: &mut Context<Self>) {
        Self::take_back(self.notifications.of_chat(id), cx);
    }

    /// The message of chat `chat` disappeared or was deleted: nothing of it may stay readable on
    /// screen, not even from before the app last started where the system can take that back
    /// (macOS, as `AppStore` removes by prefix).
    pub(super) fn message_gone(&mut self, chat: &str, key: &MessageKey, cx: &mut Context<Self>) {
        let mut tags = self.notifications.of_message(key);
        if let Some(client) = &self.client {
            let own = tag(|id| client.opaque_id(id.to_owned()), chat, key, None);
            if !tags.contains(&own) {
                tags.push(own);
            }
        }
        Self::take_back(tags, cx);
    }

    /// A message changed: if one shown was deleted for everyone, its notification goes.
    pub(super) fn recheck_notified(&mut self, chat: &str, key: &MessageKey, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone().filter(|_| self.notifications.about(key)) else {
            return;
        };
        let (chat, key) = (chat.to_owned(), key.clone());
        cx.spawn(async move |this, cx| {
            let now = match Compat::new(client.message(key.clone())).await {
                Ok(now) => now,
                // one that didn't read stays: that isn't a deletion
                Err(e) => return tracing::warn!(error = %e, "a notified message couldn't be checked"),
            };
            if now.is_none_or(|m| m.deleted) {
                let _ = this.update(cx, |app, cx| app.message_gone(&chat, &key, cx));
            }
        })
        .detach();
    }

    /// The chat list changed: the chats read take their notifications with them, and the unread
    /// count is set again, and again when a timed mute ends.
    /// Starting over (`AppStore.restart`): the count goes, and what this run showed goes with the
    /// data if that's gone (the message previews in them); what the new state keeps tracking.
    /// shortcut: those from earlier runs have no tag known here, and Linux takes nothing back.
    pub(super) fn start_over(&mut self, data_gone: bool, cx: &mut Context<Self>) -> Notifications {
        if data_gone {
            Self::take_back(self.notifications.take(|_| true), cx);
        }
        show_badge(0);
        Notifications {
            shown: std::mem::take(&mut self.notifications.shown),
            ..Default::default()
        }
    }

    pub(super) fn chats_counted(&mut self, cx: &mut Context<Self>) {
        Self::take_back(self.notifications.of_read(&self.chats), cx);
        let now = chrono::Utc::now().timestamp_millis();
        let count = badge(&self.chats, now);
        if count != self.notifications.badge {
            self.notifications.badge = count;
            show_badge(count);
        }
        // a timer dropped stays queued with the system until it's due: one per mute end
        let next = next_mute_end(&self.chats, now);
        if next == self.notifications.mute_ends.as_ref().map(|(end, _)| *end) {
            return;
        }
        self.notifications.mute_ends = next.map(|end| {
            let waits = cx.spawn(async move |this, cx| {
                // a longer one waits again: the system's timer can't take any length
                let wait = end.saturating_sub(now).min(i64::from(u32::MAX)) as u64 + 1;
                cx.background_executor().timer(Duration::from_millis(wait)).await;
                // the row's bell is drawn again too
                let _ = this.update(cx, |app, cx| {
                    app.notifications.mute_ends = None;
                    app.chats_counted(cx);
                    cx.notify();
                });
            });
            (end, waits)
        });
    }

    /// A click on a notification opens its chat, ready to type; its Mark as Read marks it read.
    fn used(&mut self, response: SystemNotificationResponse, window: &mut Window, cx: &mut Context<Self>) {
        let mark_read = response.action_id.as_deref() == Some(MARK_READ);
        if !mark_read {
            cx.activate(true);
            window.activate_window();
        }
        let Some(client) = self.client.clone() else { return };
        let opaque = |id: &str| client.opaque_id(id.to_owned());
        // the chats not loaded yet (a click that launched the app), or it has left the list
        let Some(id) = self.notifications.chat_named(&response.tag, &self.chats, opaque) else {
            return tracing::warn!("a notification's chat isn't listed");
        };
        // the chat open already: `select` leaves the others
        self.chat_seen(&id, cx);
        if mark_read {
            self.change(
                "mark the chat read",
                cx,
                |c| async move { c.mark_read(id).await.map(drop) },
            );
        } else {
            self.select(id, window, cx);
        }
    }
}

/// Clicks on notifications and their buttons go to the window's chats.
pub fn responses(window: AnyWindowHandle, hush: WeakEntity<Hush>, cx: &mut App) {
    cx.on_system_notification_response(move |response, cx| {
        let done = window.update(cx, |_, window, cx| {
            hush.update(cx, |app, cx| app.used(response, window, cx))
        });
        if let Err(e) = done.and_then(|r| r) {
            tracing::warn!(error = %e, "a notification's click went nowhere");
        }
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
/// to Dock), for a launcher whose `.desktop` file is named after `APP_ID` (none is installed yet: no
/// count shows without one). Sent from a thread of its own,
/// in order, over one connection: a launcher drops the count of a sender that leaves the bus.
#[cfg(not(any(target_os = "macos", windows)))]
fn show_badge(count: u32) {
    use std::collections::HashMap;
    use std::sync::mpsc::{channel, Sender};
    use std::sync::OnceLock;
    use zbus::zvariant::Value;

    // not the launcher of whoever runs the tests
    if cfg!(test) {
        return;
    }
    static COUNTS: OnceLock<Option<Sender<u32>>> = OnceLock::new();
    let counts = COUNTS.get_or_init(|| {
        let (counts, sent) = channel::<u32>();
        let started = std::thread::Builder::new()
            .name("launcher-count".into())
            .spawn(move || {
                let bus = match zbus::blocking::Connection::session() {
                    Ok(bus) => bus,
                    Err(e) => return tracing::warn!(error = %e, "no unread count on the launcher"),
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
                        tracing::warn!(error = %e, "the launcher's unread count failed");
                    }
                }
            });
        started
            .map_err(|e| tracing::warn!(error = %e, "no unread count on the launcher"))
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

/// A notification's title and body as Signal Desktop words them, with what `setting` lets it
/// show, or none when notifications are off (`Notifications.request`); `emoji`: a reaction by
/// `sender` to our message.
fn words(
    setting: NotificationContent,
    sender: &str,
    group: Option<&str>,
    text: Option<&str>,
    emoji: Option<&str>,
) -> Option<(String, String)> {
    let title = match group {
        Some(group) => format!("{sender} in {group}"),
        None => sender.to_owned(),
    };
    let text = text.unwrap_or("Attachment");
    Some(match (setting, emoji) {
        (NotificationContent::Off, _) => return None,
        (NotificationContent::NoNameOrMessage, _) => ("Hush".into(), "New Message".into()),
        (NotificationContent::NameOnly, _) => (title, "New Message".into()),
        (NotificationContent::NameAndMessage, Some(emoji)) => (title, format!("{sender} reacted {emoji} to: {text}")),
        (NotificationContent::NameAndMessage, None) => (title, text.to_owned()),
    })
}

#[cfg(test)]
mod tests {
    use super::{badge, message_text, next_mute_end, tag, words, NotificationContent, Notifications, Shown};
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

    pub(super) fn message(body: Option<&str>, kind: MessageKind) -> Message {
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

    /// Stands in for the core's keyed hash.
    fn opaque(id: &str) -> String {
        format!("#{}", id.len() * 7 % 100)
    }

    #[test]
    fn a_tag_names_its_chat_message_and_reactor_by_opaque_ids_only() {
        let ours = key("me-aci-1234", 5);
        let message = tag(opaque, "g:group-master-key", &ours, None);
        let thumbs = |reactor: &str| (reactor.to_owned(), "👍".to_owned());
        let by_ana = tag(opaque, "g:group-master-key", &ours, Some(&thumbs("ana-aci")));
        let by_bo = tag(opaque, "g:group-master-key", &ours, Some(&thumbs("bogdan-aci-99")));
        assert_eq!(message, "#26|#77|5|");
        // each its own: one doesn't replace another on screen
        assert_ne!(message, by_ana);
        assert_ne!(by_ana, by_bo);
        for t in [&message, &by_ana, &by_bo] {
            assert!(!t.contains("aci") && !t.contains("key"), "{t}");
        }
    }

    #[test]
    fn a_notification_from_an_earlier_run_finds_its_chat_by_the_tag() {
        let mut n = Notifications::default();
        n.add(shown("now", "c:ana", key("ana", 1), false));
        let chats = [chat("c:ana", 1), chat("g:family", 1)];
        assert_eq!(n.chat_named("now", &chats, opaque).as_deref(), Some("c:ana"));
        let earlier = tag(opaque, "g:family", &key("bo", 2), None);
        assert_eq!(n.chat_named(&earlier, &chats, opaque).as_deref(), Some("g:family"));
        // a chat no longer listed
        assert_eq!(n.chat_named("now", &chats[1..], opaque), None);
        assert_eq!(
            n.chat_named(&tag(opaque, "c:gone-chat", &key("x", 1), None), &chats, opaque),
            None
        );
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
        let all = NotificationContent::NameAndMessage;
        let said = |title: &str, body: &str| Some((title.to_string(), body.to_string()));
        assert_eq!(words(all, "Ana", None, Some("hi"), None), said("Ana", "hi"));
        assert_eq!(
            words(all, "Ana", Some("Family"), None, None),
            said("Ana in Family", "Attachment")
        );
        assert_eq!(
            words(all, "Ana", Some("Family"), Some("Dinner at 8?"), Some("👍")),
            said("Ana in Family", "Ana reacted 👍 to: Dinner at 8?")
        );
    }

    #[test]
    fn a_notification_shows_only_what_settings_let_it() {
        let reaction = |setting| words(setting, "Ana", Some("Family"), Some("Dinner at 8?"), Some("👍"));
        let said = |title: &str, body: &str| Some((title.to_string(), body.to_string()));
        assert_eq!(
            reaction(NotificationContent::NameOnly),
            said("Ana in Family", "New Message")
        );
        assert_eq!(
            reaction(NotificationContent::NoNameOrMessage),
            said("Hush", "New Message")
        );
        assert_eq!(reaction(NotificationContent::Off), None);
    }
}

/// Notifications in a real (headless) window, as GPUI's test platform records them:
/// `cargo test --features hush-desktop/snapshot`. The core isn't linked: it has no messages.
#[cfg(all(test, feature = "snapshot"))]
mod window_tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use gpui_kit::{gpui, AnyWindowHandle, AppContext as _, Entity, SystemNotificationResponse, TestAppContext};
    use gpui_kit::{Context, VisualTestContext, Window, WindowOptions};
    use sig_core::projection::{BodyRange, Conversation, MessageKey, MessageKind, TextStyle};
    use sig_core::CoreEvent;

    use super::super::attach::window_tests::with_core;
    use super::super::Hush;
    use super::{About, Shown, APP_ID, MARK_READ};
    use crate::demo::{ANA, ME};
    use crate::Demo;

    /// The demo's chats, Family open, behind other windows, with a core in a data folder of its
    /// own (removed by the caller).
    fn open(name: &str, cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Hush>, PathBuf) {
        let (window, hush) = cx.update(|cx| {
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
        VisualTestContext::from_window(window, cx).deactivate_window();
        assert!(!hush.read_with(cx, |app, _| app.window_active));
        let data = std::env::temp_dir().join(format!("hush-desktop-notifications-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        with_core(&hush, &data, cx);
        // the core answers from its own threads
        cx.executor().allow_parking();
        (window, hush, data)
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
        hush.update(cx, |app, cx| {
            app.post(shown, "Ana Popescu".into(), "hi".into(), true, cx)
        });
    }

    fn on_show(cx: &TestAppContext) -> Vec<String> {
        cx.delivered_system_notifications()
            .into_iter()
            .map(|n| n.tag.to_string())
            .collect()
    }

    /// Waits for the core (on its own threads) until `done`.
    fn until(cx: &mut TestAppContext, done: impl Fn(&mut TestAppContext) -> bool) {
        for _ in 0..200 {
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("never happened");
    }

    fn click(tag: &str, action: Option<&str>, cx: &mut TestAppContext) {
        cx.simulate_system_notification_response(SystemNotificationResponse {
            tag: tag.to_owned().into(),
            action_id: action.map(|a| a.to_owned().into()),
        });
        cx.run_until_parked();
    }

    fn open_chat(hush: &Entity<Hush>, cx: &mut TestAppContext) -> Option<String> {
        hush.read_with(cx, |app, _| app.selected.clone())
    }

    #[gpui::test]
    fn none_shows_while_the_window_is_in_front_and_a_click_brings_up_its_chat(cx: &mut TestAppContext) {
        let (window, hush, data) = open("click", cx);
        let ana = format!("c:{ANA}");
        hush.update(cx, |app, _| app.window_active = true);
        post(&hush, "front", &ana, key(ANA, 1), false, cx);
        assert!(cx.shown_system_notifications().is_empty(), "in front: quiet");
        // the active window under a locked screen isn't seen
        hush.update(cx, |app, _| app.window_visible = false);
        post(&hush, "locked", &ana, key(ANA, 2), false, cx);
        assert_eq!(on_show(cx), ["locked"]);

        hush.update(cx, |app, _| {
            app.window_active = false;
            app.window_visible = true;
        });
        post(&hush, "away", &ana, key(ANA, 3), false, cx);
        let shown = cx.shown_system_notifications();
        assert_eq!(shown.len(), 2);
        assert_eq!((shown[1].title.as_ref(), shown[1].body.as_ref()), ("Ana Popescu", "hi"));
        let actions: Vec<_> = shown[1].actions.iter().map(|a| a.label.to_string()).collect();
        assert_eq!(actions, ["Mark as Read"]);

        click("away", None, cx);
        assert_eq!(open_chat(&hush, cx), Some(ana.clone()));
        assert!(hush.read_with(cx, |app, _| app.window_active), "brought up");
        assert!(on_show(cx).is_empty(), "opening the chat took its notifications back");

        // one whose chat isn't known brings up the window only
        VisualTestContext::from_window(window, cx).deactivate_window();
        click("unknown", None, cx);
        assert!(hush.read_with(cx, |app, _| app.window_active));
        assert_eq!(open_chat(&hush, cx), Some(ana));
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[gpui::test]
    fn a_click_in_the_open_chat_takes_back_its_other_notifications(cx: &mut TestAppContext) {
        let (_, hush, data) = open("open-chat", cx);
        post(&hush, "r1", "g:01", key(ME, 1), true, cx);
        post(&hush, "r2", "g:01", key(ME, 2), true, cx);
        click("r1", None, cx);
        assert_eq!(open_chat(&hush, cx).as_deref(), Some("g:01"));
        assert!(on_show(cx).is_empty());
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[gpui::test]
    fn a_reaction_names_who_reacted_and_the_people_mentioned_and_outlasts_a_read(cx: &mut TestAppContext) {
        let (window, hush, data) = open("show", cx);
        const CARMEN: &str = "44444444-4444-4444-8444-444444444444";
        let mut ours = super::tests::message(Some("hi \u{FFFC}"), MessageKind::Normal);
        ours.ranges = vec![BodyRange {
            start: 3,
            length: 1,
            style: TextStyle::Mention { aci: CARMEN.into() },
        }];
        let about = About {
            tag: "r".into(),
            message: ours,
            file: None,
            group: Some("Family".into()),
            names: vec![(CARMEN.into(), "Carmen".into())],
        };
        let reaction = Some((ANA.to_owned(), "👍".to_owned()));
        hush.update(cx, |app, cx| app.show("g:01".into(), key(ME, 5), reaction, about, cx));
        let shown = cx.shown_system_notifications();
        assert_eq!(shown[0].title.as_ref(), "Ana Popescu in Family");
        assert_eq!(shown[0].body.as_ref(), "Ana Popescu reacted 👍 to: hi @Carmen");

        let mut chats = crate::demo::chats();
        chats[0].unread_count = 0;
        act(window, &hush, cx, |app, _, cx| app.set_chats(chats, cx));
        assert_eq!(on_show(cx), ["r"], "a reaction isn't unread: reading leaves it");
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[gpui::test]
    fn starting_over_takes_back_what_was_shown_only_with_the_data(cx: &mut TestAppContext) {
        let (_, hush, data) = open("start-over", cx);
        let ana = format!("c:{ANA}");
        post(&hush, "m1", &ana, key(ANA, 1), false, cx);
        let kept = hush.update(cx, |app, cx| app.start_over(false, cx));
        assert_eq!(on_show(cx), ["m1"], "relinking keeps them");
        assert_eq!(kept.chat_of("m1"), Some(ana), "and still knows their chat");
        hush.update(cx, |app, cx| {
            app.notifications = kept;
            app.start_over(true, cx)
        });
        assert!(on_show(cx).is_empty(), "unlinking takes them back with the data");
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[gpui::test]
    fn a_notification_shows_what_the_setting_lets_it(cx: &mut TestAppContext) {
        use super::NotificationContent;
        let (_, hush, data) = open("setting", cx);
        let about = |tag: &str| About {
            tag: tag.into(),
            message: super::tests::message(Some("Dinner at 8?"), MessageKind::Normal),
            file: None,
            group: Some("Family".into()),
            names: vec![],
        };
        let show = |hush: &Entity<Hush>, setting, tag: &str, cx: &mut TestAppContext| {
            cx.update(|cx| cx.set_global(setting));
            let about = about(tag);
            hush.update(cx, |app, cx| app.show("g:01".into(), key(ANA, 1), None, about, cx));
        };
        show(&hush, NotificationContent::NameOnly, "name", cx);
        show(&hush, NotificationContent::NoNameOrMessage, "count", cx);
        show(&hush, NotificationContent::Off, "off", cx);
        let shown = cx.shown_system_notifications();
        let said: Vec<_> = shown
            .iter()
            .map(|n| (n.title.as_ref(), n.body.as_ref(), n.actions.len()))
            .collect();
        assert_eq!(
            said,
            [("Ana Popescu in Family", "New Message", 0), ("Hush", "New Message", 0)],
            "no content, no Mark as Read; none at all when off"
        );
        assert_eq!(on_show(cx), ["name", "count"]);
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[gpui::test]
    fn reading_deleting_or_losing_a_message_takes_its_notification_back(cx: &mut TestAppContext) {
        let (window, hush, data) = open("gone", cx);
        // Family (open, 3 unread), a reaction to our message in Book club, Ana's and Bogdan's
        post(&hush, "family", "g:01", key(ANA, 1), false, cx);
        post(&hush, "club", "g:02", key(ME, 2), true, cx);
        post(&hush, "ana", &format!("c:{ANA}"), key(ANA, 3), false, cx);
        post(&hush, "ana-2", &format!("c:{ANA}"), key(ANA, 4), false, cx);
        let mut chats = crate::demo::chats();
        chats[1].unread_count = 2;
        act(window, &hush, cx, |app, _, cx| app.set_chats(chats.clone(), cx));
        assert_eq!(on_show(cx), ["family", "club", "ana", "ana-2"]);
        assert_eq!(hush.read_with(cx, |app, _| app.notifications.badge), 3 + 2 + 12);

        // read on the phone
        chats[0].unread_count = 0;
        act(window, &hush, cx, |app, _, cx| app.set_chats(chats.clone(), cx));
        assert_eq!(on_show(cx), ["club", "ana", "ana-2"]);
        assert_eq!(hush.read_with(cx, |app, _| app.notifications.badge), 2 + 12);

        let expired = CoreEvent::MessageExpired {
            conversation_id: "g:02".into(),
            key: key(ME, 2),
        };
        hush.update(cx, |app, cx| app.handle(expired, cx));
        assert_eq!(on_show(cx), ["ana", "ana-2"]);
        // one shown before the app last started, by its tag
        let earlier = hush.read_with(cx, |app, _| {
            let client = app.client.clone().unwrap();
            super::tag(|id| client.opaque_id(id.to_owned()), "g:02", &key(ANA, 9), None)
        });
        let expired = CoreEvent::MessageExpired {
            conversation_id: "g:02".into(),
            key: key(ANA, 9),
        };
        hush.update(cx, |app, cx| app.handle(expired, cx));
        assert_eq!(
            cx.dismissed_system_notifications().last().map(|t| t.to_string()),
            Some(earlier)
        );

        // changed, and no longer in the core: deleted
        let changed = |sent_at| CoreEvent::MessageChanged {
            conversation_id: format!("c:{ANA}"),
            key: key(ANA, sent_at),
        };
        hush.update(cx, |app, cx| app.handle(changed(3), cx));
        until(cx, |cx| on_show(cx) == ["ana-2"]);

        act(window, &hush, cx, |app, _, cx| app.delete_chat(format!("c:{ANA}"), cx));
        assert!(on_show(cx).is_empty(), "the chat deleted");
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[gpui::test]
    fn the_badge_counts_a_chat_again_when_its_mute_ends(cx: &mut TestAppContext) {
        let (window, hush, data) = open("mute", cx);
        let now = chrono::Utc::now().timestamp_millis();
        let muted = Conversation {
            unread_count: 2,
            mute_until: now + 30,
            ..crate::app::tests::chat("c:m", "")
        };
        let chats = vec![crate::app::tests::chat("c:a", ""), muted];
        act(window, &hush, cx, |app, _, cx| app.set_chats(chats, cx));
        assert_eq!(hush.read_with(cx, |app, _| app.notifications.badge), 0);
        // the mute is over by the clock the count reads; the timer runs on the test's
        std::thread::sleep(Duration::from_millis(40));
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        assert_eq!(hush.read_with(cx, |app, _| app.notifications.badge), 2);
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[gpui::test]
    fn mark_as_read_asks_the_core_and_takes_the_chats_notifications_back(cx: &mut TestAppContext) {
        let (_, hush, data) = open("mark-read", cx);
        let ana = format!("c:{ANA}");
        post(&hush, "m", &ana, key(ANA, 1), false, cx);
        post(&hush, "r", &ana, key(ME, 2), true, cx);
        click("m", Some(MARK_READ), cx);
        assert!(on_show(cx).is_empty());
        assert_eq!(open_chat(&hush, cx).as_deref(), Some("g:01"), "not opened");
        assert!(!hush.read_with(cx, |app, _| app.window_active), "nor brought up");
        // a core that isn't linked refuses: the asking shows
        until(cx, |cx| hush.read_with(cx, |app, _| app.error.is_some()));
        let said = hush.update(cx, |app, _| app.error.take()).unwrap();
        assert!(said.starts_with("Couldn't mark the chat read"), "{said}");
        std::fs::remove_dir_all(&data).unwrap();
    }
}
