//! What can be done to a chat (the Mac app's `SidebarView.chatMenu` and `RequestBar`, after
//! Signal Desktop): pin, mute, mark, export, archive and block it from its row's right-click menu;
//! answer a message request, an invitation or our join request, or unblock, in the bar that takes
//! the composer's place; and the archived chats' own list.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_compat::Compat;
use futures::future::Shared;
use futures::FutureExt;
use gpui_kit::*;
use sig_core::projection::{Conversation, ConversationKind};
use sig_core::{Client, CoreError, GroupChange};

use super::{describe, files, server_failure, Hush};
use crate::text;

pub(super) mod view;

/// How many chats can be pinned (the phone's limit; the core refuses more).
const MAX_PINNED: usize = 4;

/// How long a chat can be muted for, as Desktop offers it: hours, or for good.
pub(super) const MUTES: [(&str, Option<i64>); 5] = [
    ("1 hour", Some(1)),
    ("8 hours", Some(8)),
    ("1 day", Some(24)),
    ("1 week", Some(7 * 24)),
    ("Always", None),
];

/// When a mute of `hours` from `now_ms` ends, in ms since the epoch (`i64::MAX`: never).
pub(super) fn mute_until(hours: Option<i64>, now_ms: i64) -> i64 {
    hours.map_or(i64::MAX, |h| now_ms + h * 3_600_000)
}

/// The archive, and the chat changes on their way.
#[derive(Default)]
pub(super) struct ChatActions {
    /// The sidebar lists the archived chats instead of the inbox.
    pub(super) archive: bool,
    /// The last change asked for: the next one waits for it, so two quick ones land in order.
    last: Option<Shared<Task<()>>>,
}

/// A chat's right-click menu item.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Item {
    /// Not offered while as many as can be are pinned.
    Pin {
        allowed: bool,
    },
    Unpin,
    /// A submenu of `MUTES`.
    Mute,
    Unmute,
    MarkRead,
    MarkUnread,
    Export,
    Archive,
    Unarchive,
    Block,
    Unblock,
}

impl Item {
    pub(super) fn label(self) -> &'static str {
        match self {
            Item::Pin { .. } => "Pin chat",
            Item::Unpin => "Unpin chat",
            Item::Mute => "Mute notifications",
            Item::Unmute => "Unmute notifications",
            Item::MarkRead => "Mark as read",
            Item::MarkUnread => "Mark as unread",
            Item::Export => "Export chat…",
            Item::Archive => "Archive chat",
            Item::Unarchive => "Unarchive chat",
            Item::Block => "Block…",
            Item::Unblock => "Unblock",
        }
    }
}

/// A chat's right-click menu, in the Mac app's order; `pinned`: how many chats are. Our own chat
/// can't be blocked.
pub(super) fn menu(c: &Conversation, me: Option<&str>, pinned: usize, now_ms: i64) -> Vec<Item> {
    let mut items = vec![
        match c.pinned_order {
            None => Item::Pin {
                allowed: pinned < MAX_PINNED,
            },
            Some(_) => Item::Unpin,
        },
        if c.mute_until > now_ms {
            Item::Unmute
        } else {
            Item::Mute
        },
        if c.unread_count > 0 || c.marked_unread {
            Item::MarkRead
        } else {
            Item::MarkUnread
        },
        Item::Export,
        if c.archived { Item::Unarchive } else { Item::Archive },
    ];
    if me.is_none_or(|me| c.id != format!("c:{me}")) {
        items.push(if c.blocked { Item::Unblock } else { Item::Block });
    }
    items
}

/// A button of the bar in place of the composer, or Block… and Unblock in a chat's menu.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Answer {
    Accept,
    Block,
    Delete,
    Unblock,
    Join,
    Decline,
    CancelRequest,
}

impl Answer {
    pub(super) fn label(self) -> &'static str {
        match self {
            Answer::Accept => "Accept",
            Answer::Block => "Block",
            Answer::Delete => "Delete",
            Answer::Unblock => "Unblock",
            Answer::Join => "Join",
            Answer::Decline => "Decline",
            Answer::CancelRequest => "Cancel Request",
        }
    }

    /// Asks the server: not again while it does.
    pub(super) fn waits(self) -> bool {
        matches!(self, Answer::Join | Answer::Decline | Answer::CancelRequest)
    }
}

/// The bar's buttons, the default one last: a blocked chat's Delete and Unblock, our join
/// request's Delete and Cancel Request, an invitation's Block, Decline and Join, a message
/// request's Block, Delete and Accept.
pub(super) fn answers(c: &Conversation) -> &'static [Answer] {
    use Answer::*;
    match () {
        _ if c.blocked => &[Delete, Unblock],
        _ if c.group_join_requested => &[Delete, CancelRequest],
        _ if c.group_invitation => &[Block, Decline, Join],
        _ => &[Block, Delete, Accept],
    }
}

/// What the bar says above its buttons; `name`: the chat's.
pub(super) fn prompt(c: &Conversation, name: &str) -> String {
    let group = c.kind == ConversationKind::Group;
    match () {
        _ if c.blocked && group => "You blocked this group.".into(),
        _ if c.blocked => format!("You blocked {name}."),
        _ if c.group_join_requested => "You asked to join this group. An admin has to let you in.".into(),
        _ if c.group_invitation => {
            "You're invited to join this group. Its members see your name and picture once you join.".into()
        }
        _ if group => "Join this group? They won't know you've seen their messages until you accept.".into(),
        _ => format!("Let {name} message you? They won't know you've seen their messages until you accept."),
    }
}

/// What an answer asks first: the question, what follows, and the button that goes ahead. Accept,
/// Unblock and Join don't ask.
pub(super) fn confirmation(a: Answer, c: &Conversation, name: &str) -> Option<(String, &'static str, &'static str)> {
    let group = c.kind == ConversationKind::Group;
    Some(match a {
        Answer::Block => {
            let detail = match () {
                _ if c.group_invitation => "You won't get its messages or updates. The invitation stays unanswered.",
                _ if group => "You won't get its messages or updates. You stay a member: leave it from your phone.",
                _ => "They won't be able to message or call you.",
            };
            let question = if group {
                "Block this group?".into()
            } else {
                format!("Block {name}?")
            };
            (question, detail, "Block")
        }
        Answer::Delete => {
            let detail = if group {
                "Its messages are deleted from this computer."
            } else {
                "Its messages are deleted from this computer and your phone."
            };
            ("Delete this chat?".into(), detail, "Delete")
        }
        Answer::Decline => (
            "Decline the invitation?".into(),
            "Its members see that you declined, and the chat is deleted from this computer.",
            "Decline",
        ),
        Answer::CancelRequest => (
            "Cancel your request to join?".into(),
            "The chat is deleted from this computer. You can ask again with the link.",
            "Cancel Request",
        ),
        Answer::Accept | Answer::Unblock | Answer::Join => return None,
    })
}

/// Marks each of an export's files as come from someone else, as opened ones are
/// (`Files.quarantineFiles`); the failures.
fn mark_exported(folder: &Path) -> Vec<std::io::Error> {
    match std::fs::read_dir(folder) {
        Ok(entries) => entries
            .filter_map(|e| e.and_then(|e| files::mark_from_elsewhere(&e.path())).err())
            .collect(),
        Err(e) => vec![e],
    }
}

impl Hush {
    /// The archive is on show: a search covers every chat, so it counts as the inbox.
    pub(super) fn in_archive(&self) -> bool {
        self.chat_actions.archive && self.query.is_empty()
    }

    pub(super) fn show_archive(&mut self, on: bool, cx: &mut Context<Self>) {
        self.chat_actions.archive = on;
        cx.notify();
    }

    /// An item of the menu of the chat `id`.
    pub(super) fn pick(&mut self, item: Item, id: String, window: &mut Window, cx: &mut Context<Self>) {
        match item {
            Item::Pin { .. } | Item::Unpin => {
                let pin = item != Item::Unpin;
                let what = if pin { "pin the chat" } else { "unpin the chat" };
                self.change(what, cx, move |c| async move { c.set_pinned(id, pin).await })
            }
            // its submenu's
            Item::Mute => {}
            Item::Unmute => self.mute(id, 0, cx),
            Item::MarkRead => self.change(
                "mark the chat read",
                cx,
                |c| async move { c.mark_read(id).await.map(drop) },
            ),
            Item::MarkUnread => {
                // the open chat would count as read again at once: it closes, as on Desktop
                if self.selected.as_ref() == Some(&id) {
                    self.switch(None, window, cx);
                }
                self.change("mark the chat unread", cx, |c| async move {
                    c.set_marked_unread(id, true).await
                })
            }
            Item::Export => self.export(id, cx),
            Item::Archive | Item::Unarchive => {
                let archive = item == Item::Archive;
                let what = if archive {
                    "archive the chat"
                } else {
                    "unarchive the chat"
                };
                self.change(what, cx, move |c| async move { c.set_archived(id, archive).await })
            }
            Item::Block => self.answer(Answer::Block, id, window, cx),
            Item::Unblock => self.answer(Answer::Unblock, id, window, cx),
        }
    }

    /// Mutes the chat `id` until `until` (ms since the epoch); 0 unmutes.
    pub(super) fn mute(&mut self, id: String, until: i64, cx: &mut Context<Self>) {
        let what = if until == 0 { "unmute the chat" } else { "mute the chat" };
        self.change(what, cx, move |c| async move { c.set_mute_until(id, until).await })
    }

    /// One of the bar's buttons, or Block… and Unblock in a chat's menu, for the chat `id`; one
    /// that asks first goes ahead once the answer is yes.
    pub(super) fn answer(&mut self, a: Answer, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.chats.iter().find(|c| c.id == id) else {
            return;
        };
        let name = text::title(c, self.own_aci.as_deref());
        let Some((question, detail, button)) = confirmation(a, c, &name) else {
            return self.go_ahead(a, id, cx);
        };
        let asked = window.prompt(PromptLevel::Warning, &question, Some(detail), &[button, "Cancel"], cx);
        cx.spawn(async move |this, cx| {
            if asked.await == Ok(0) {
                let _ = this.update(cx, |app, cx| app.go_ahead(a, id, cx));
            }
        })
        .detach();
    }

    fn go_ahead(&mut self, a: Answer, id: String, cx: &mut Context<Self>) {
        match a {
            // accepting a blocked chat unblocks it
            Answer::Accept | Answer::Unblock => {
                self.change("accept the chat", cx, |c| async move { c.accept_request(id).await })
            }
            Answer::Block => self.change("block the chat", cx, |c| async move { c.block(id).await }),
            Answer::Delete => self.delete_chat(id, cx),
            Answer::CancelRequest => self.cancel_join_request(id, cx),
            Answer::Join => self.answer_invitation(id, true, cx),
            Answer::Decline => self.answer_invitation(id, false, cx),
        }
    }

    /// Joins the group we're invited to, or declines and deletes its chat
    /// (`AppStore.answerInvitation`); a failure is said (the chat then stays).
    fn answer_invitation(&mut self, id: String, accept: bool, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        if self.answering {
            return;
        }
        self.answering = true;
        cx.notify();
        let change = if accept {
            GroupChange::AcceptInvitation
        } else {
            GroupChange::DeclineInvitation
        };
        cx.spawn(async move |this, cx| {
            let answered = Compat::new(client.change_group(id.clone(), change)).await;
            let _ = this.update(cx, |app, cx| {
                app.answering = false;
                match answered {
                    Ok(()) if !accept => app.delete_chat(id, cx),
                    Ok(()) => {}
                    Err(e) => {
                        let failing = if accept {
                            "Couldn't join the group"
                        } else {
                            "Couldn't decline the invitation"
                        };
                        app.error = Some(server_failure(failing, &e));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// A change to a chat (`AppStore.change`): the core tells when the list changed; a failure is
    /// said. Each waits for the one before.
    fn change<F>(&mut self, what: &'static str, cx: &mut Context<Self>, call: impl FnOnce(Arc<Client>) -> F + 'static)
    where
        F: Future<Output = Result<(), CoreError>> + 'static,
    {
        let Some(client) = self.client.clone() else { return };
        let before = self.chat_actions.last.take();
        let change = cx.spawn(async move |this, cx| {
            if let Some(before) = before {
                before.await;
            }
            if let Err(e) = Compat::new(call(client)).await {
                let _ = this.update(cx, |app, cx| {
                    app.error = Some(format!("Couldn't {what}: {}", describe(&e)));
                    cx.notify();
                });
            }
        });
        self.chat_actions.last = Some(change.shared());
    }

    /// Asks where, then exports the chat there and shows the new folder (`SidebarView.export`).
    fn export(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Export".into()),
        });
        cx.spawn(async move |this, cx| {
            let made = match chosen.await {
                Ok(Ok(Some(paths))) => match paths.into_iter().next() {
                    Some(folder) => export_to(&client, id, &folder).await,
                    None => return,
                },
                Ok(Err(e)) => Err(e.to_string()),
                // cancelled
                Ok(Ok(None)) | Err(_) => return,
            };
            match made {
                Ok(made) => {
                    let files = made.join("files");
                    // off the main thread: a chat can have thousands
                    let failed = cx.background_spawn(async move { mark_exported(&files) }).await;
                    if let Some(e) = failed.first() {
                        tracing::warn!(error = %e, count = failed.len(), "marking exported files failed");
                    }
                    cx.update(|cx| cx.reveal_path(&made));
                }
                Err(e) => {
                    let _ = this.update(cx, |app, cx| {
                        app.error = Some(format!("Couldn't export the chat: {e}"));
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }
}

/// The chat `id` exported into a new folder in `folder`: that folder, or why not (a file failure
/// as the Mac app says it, `AppStore.fileFailure`).
async fn export_to(client: &Client, id: String, folder: &Path) -> Result<PathBuf, String> {
    let at = folder.to_str().ok_or("the folder's path isn't valid text")?.to_owned();
    match Compat::new(client.export_chat(id, at)).await {
        Ok(made) => Ok(made.into()),
        Err(CoreError::Storage { message }) => Err(message),
        Err(e) => Err(describe(&e)),
    }
}

#[cfg(feature = "snapshot")]
impl Hush {
    /// `--snapshot`'s chats: a message request open; else the family's chat, the archive listed
    /// or a row's menu open (`main.rs` right-clicks it).
    pub(super) fn demo_chats(&mut self, screen: crate::Demo, cx: &mut Context<Self>) {
        if let crate::Demo::Request = screen {
            self.selected = Some(format!("c:{}", crate::demo::STRANGER));
            return self.set_messages(crate::demo::request(), cx);
        }
        self.selected = Some("g:01".into());
        self.set_messages(crate::demo::messages(), cx);
        self.chat_actions.archive = matches!(screen, crate::Demo::Archive);
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::chat;
    use super::{answers, confirmation, mark_exported, menu, mute_until, prompt, Answer, Item, MUTES};
    use sig_core::projection::Conversation;

    #[test]
    fn a_chats_menu_offers_what_can_be_done_to_it_now() {
        let now = 1_000_000;
        let labels = |c: &Conversation, pinned: usize| -> Vec<&str> {
            menu(c, Some("me"), pinned, now).into_iter().map(Item::label).collect()
        };
        assert_eq!(
            labels(&chat("c:a", "Ana"), 0),
            [
                "Pin chat",
                "Mute notifications",
                "Mark as unread",
                "Export chat…",
                "Archive chat",
                "Block…"
            ]
        );
        let changed = Conversation {
            pinned_order: Some(1),
            mute_until: now + 1,
            unread_count: 2,
            archived: true,
            blocked: true,
            ..chat("c:a", "Ana")
        };
        assert_eq!(
            labels(&changed, 4),
            [
                "Unpin chat",
                "Unmute notifications",
                "Mark as read",
                "Export chat…",
                "Unarchive chat",
                "Unblock"
            ]
        );
        // marked unread by hand counts as unread; a mute that ended doesn't
        let marked = Conversation {
            marked_unread: true,
            mute_until: now,
            ..chat("c:a", "Ana")
        };
        assert_eq!(labels(&marked, 0)[1..3], ["Mute notifications", "Mark as read"]);
        // a fifth pin isn't offered; our own chat can't be blocked
        assert_eq!(
            menu(&chat("c:a", "Ana"), Some("me"), 4, now)[0],
            Item::Pin { allowed: false }
        );
        assert_eq!(
            menu(&chat("c:a", "Ana"), Some("me"), 3, now)[0],
            Item::Pin { allowed: true }
        );
        assert!(!labels(&chat("c:me", ""), 0).contains(&"Block…"));
        assert!(labels(&chat("c:me", ""), 0).contains(&"Archive chat"));
        assert_eq!(labels(&chat("g:1", "Hikers"), 0).last(), Some(&"Block…"));
    }

    #[test]
    fn a_mute_ends_after_its_hours_or_never() {
        let titles: Vec<&str> = MUTES.iter().map(|(title, _)| *title).collect();
        assert_eq!(titles, ["1 hour", "8 hours", "1 day", "1 week", "Always"]);
        let until: Vec<i64> = MUTES.iter().map(|(_, hours)| mute_until(*hours, 1_000_000)).collect();
        let hour = 3_600_000;
        assert_eq!(
            until,
            [
                1_000_000 + hour,
                1_000_000 + 8 * hour,
                1_000_000 + 24 * hour,
                1_000_000 + 7 * 24 * hour,
                i64::MAX
            ]
        );
    }

    #[test]
    fn the_bar_answers_a_request_an_invitation_a_join_request_or_a_block() {
        use Answer::*;
        let request = Conversation {
            message_request: true,
            ..chat("c:s", "Sam")
        };
        assert_eq!(answers(&request), [Block, Delete, Accept]);
        assert_eq!(
            prompt(&request, "Sam"),
            "Let Sam message you? They won't know you've seen their messages until you accept."
        );
        let group = Conversation {
            message_request: true,
            ..chat("g:1", "Hikers")
        };
        assert!(prompt(&group, "Hikers").starts_with("Join this group?"));
        let invited = Conversation {
            group_invitation: true,
            ..chat("g:1", "Hikers")
        };
        assert_eq!(answers(&invited), [Block, Decline, Join]);
        assert!(prompt(&invited, "Hikers").starts_with("You're invited"));
        let asked = Conversation {
            group_join_requested: true,
            ..chat("g:1", "Hikers")
        };
        assert_eq!(answers(&asked), [Delete, CancelRequest]);
        assert_eq!(
            prompt(&asked, "Hikers"),
            "You asked to join this group. An admin has to let you in."
        );
        // blocked comes first: a blocked request or invitation is unblocked, not answered
        let blocked = Conversation {
            blocked: true,
            ..invited.clone()
        };
        assert_eq!(answers(&blocked), [Delete, Unblock]);
        assert_eq!(prompt(&blocked, "Hikers"), "You blocked this group.");
        let person = Conversation {
            blocked: true,
            ..chat("c:a", "Ana")
        };
        assert_eq!(prompt(&person, "Ana"), "You blocked Ana.");
        // only the ones that ask the server wait for it
        let waiting: Vec<Answer> = [Accept, Block, Delete, Unblock, Join, Decline, CancelRequest]
            .into_iter()
            .filter(|a| a.waits())
            .collect();
        assert_eq!(waiting, [Join, Decline, CancelRequest]);
    }

    #[test]
    fn block_delete_decline_and_cancel_ask_first() {
        let (ana, group) = (chat("c:a", "Ana"), chat("g:1", "Hikers"));
        let asks = |a: Answer, c: &Conversation| confirmation(a, c, "Ana").map(|(q, d, b)| (q, d.to_string(), b));
        let (question, detail, button) = asks(Answer::Block, &ana).unwrap();
        assert_eq!(
            (question.as_str(), detail.as_str(), button),
            ("Block Ana?", "They won't be able to message or call you.", "Block")
        );
        let (question, detail, _) = asks(Answer::Block, &group).unwrap();
        assert_eq!(question, "Block this group?");
        assert!(detail.contains("You stay a member"));
        let invited = Conversation {
            group_invitation: true,
            ..group.clone()
        };
        assert!(asks(Answer::Block, &invited)
            .unwrap()
            .1
            .contains("The invitation stays unanswered"));
        assert!(asks(Answer::Delete, &ana).unwrap().1.contains("and your phone"));
        assert!(!asks(Answer::Delete, &group).unwrap().1.contains("phone"));
        assert_eq!(asks(Answer::Decline, &invited).unwrap().2, "Decline");
        assert_eq!(asks(Answer::CancelRequest, &group).unwrap().2, "Cancel Request");
        for a in [Answer::Accept, Answer::Unblock, Answer::Join] {
            assert_eq!(asks(a, &ana), None, "{a:?} goes ahead");
        }
    }

    #[test]
    fn every_exported_file_is_marked() {
        let dir = std::env::temp_dir().join(format!("hush-desktop-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("1-photo.jpg"), "x").unwrap();
        std::fs::write(dir.join("2-run me.command"), "echo hi").unwrap();
        let failed = mark_exported(&dir);
        #[cfg(target_os = "macos")]
        for name in ["1-photo.jpg", "2-run me.command"] {
            let path = std::ffi::CString::new(dir.join(name).to_str().unwrap()).unwrap();
            // SAFETY: the strings are NUL-terminated; no value is read back, only its size
            let n = unsafe {
                libc::getxattr(
                    path.as_ptr(),
                    c"com.apple.quarantine".as_ptr(),
                    std::ptr::null_mut(),
                    0,
                    0,
                    0,
                )
            };
            assert!(n > 0, "{name} marked");
        }
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(failed.is_empty(), "{failed:?}");
        assert_eq!(mark_exported(&dir).len(), 1, "no folder: said");
    }
}

/// A chat's actions in a real (headless) window: `cargo test -p hush-desktop --features snapshot`.
#[cfg(all(test, feature = "snapshot"))]
mod window_tests {
    use gpui_kit::{gpui, AnyWindowHandle, AppContext as _, Context, Entity, TestAppContext, Window, WindowOptions};

    use super::super::Hush;
    use super::{Answer, Item};
    use crate::demo::{ANA, STRANGER};
    use crate::Demo;

    fn open(screen: Demo, cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Hush>) {
        let opened = cx.update(|cx| {
            gpui_kit::init(cx);
            cx.bind_keys(crate::key_bindings());
            let (window, hush) = gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Hush::demo(screen, window, cx))
            })
            .expect("the window opens");
            super::super::navigate::shortcuts(window, hush.downgrade(), cx);
            (window, hush)
        });
        cx.run_until_parked();
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

    fn listed(hush: &Entity<Hush>, cx: &mut TestAppContext) -> Vec<String> {
        hush.read_with(cx, |app, _| app.listed().iter().map(|c| c.id.clone()).collect())
    }

    #[gpui::test]
    fn the_archive_lists_the_archived_chats_until_back(cx: &mut TestAppContext) {
        let (window, hush) = open(Demo::Chat, cx);
        assert!(!listed(&hush, cx).contains(&"g:03".to_string()), "the inbox");
        act(window, &hush, cx, |app, _, cx| app.show_archive(true, cx));
        assert_eq!(listed(&hush, cx), ["g:03"]);
        // the keys move through what is listed
        cx.simulate_keystrokes(window, "alt-down");
        assert_eq!(
            hush.read_with(cx, |app, _| app.selected.clone()).as_deref(),
            Some("g:03")
        );

        // a search covers every chat, then the archive is back
        cx.simulate_keystrokes(window, "secondary-k");
        cx.simulate_input(window, "bo");
        assert_eq!(listed(&hush, cx).len(), 3, "Bogdan, Book club and Book swap");
        assert!(!hush.read_with(cx, |app, _| app.in_archive()));
        cx.simulate_keystrokes(window, "backspace backspace");
        assert_eq!(listed(&hush, cx), ["g:03"]);

        act(window, &hush, cx, |app, _, cx| app.show_archive(false, cx));
        assert_eq!(listed(&hush, cx).len(), 6);
    }

    #[gpui::test]
    fn marking_the_open_chat_unread_closes_it(cx: &mut TestAppContext) {
        let (window, hush) = open(Demo::Chat, cx);
        let ana = format!("c:{ANA}");
        act(window, &hush, cx, |app, window, cx| {
            app.pick(Item::MarkUnread, ana, window, cx)
        });
        assert_eq!(
            hush.read_with(cx, |app, _| app.selected.clone()).as_deref(),
            Some("g:01")
        );
        act(window, &hush, cx, |app, window, cx| {
            app.pick(Item::MarkUnread, "g:01".into(), window, cx)
        });
        assert_eq!(hush.read_with(cx, |app, _| app.selected.clone()), None);
    }

    #[gpui::test]
    fn block_and_delete_ask_first_and_accept_goes_ahead(cx: &mut TestAppContext) {
        let (window, hush) = open(Demo::Request, cx);
        let stranger = format!("c:{STRANGER}");
        let ask = stranger.clone();
        act(window, &hush, cx, |app, window, cx| {
            app.answer(Answer::Accept, ask, window, cx)
        });
        assert!(!cx.has_pending_prompt());

        let ask = stranger.clone();
        act(window, &hush, cx, |app, window, cx| {
            app.answer(Answer::Delete, ask, window, cx)
        });
        let (question, _) = cx.pending_prompt().expect("asked");
        assert_eq!(question, "Delete this chat?");
        cx.simulate_prompt_answer("Cancel");

        let ana = format!("c:{ANA}");
        act(window, &hush, cx, |app, window, cx| {
            app.pick(Item::Block, ana, window, cx)
        });
        let (question, detail) = cx.pending_prompt().expect("asked");
        assert_eq!(question, "Block Ana Popescu?");
        assert_eq!(detail, "They won't be able to message or call you.");
        cx.simulate_prompt_answer("Cancel");
    }
}
