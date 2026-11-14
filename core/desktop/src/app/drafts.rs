//! Unsent text, kept by the core in its encrypted database so it outlives the app
//! (`AppStore.saveDraft`, `loadDrafts`): each chat's last change is written, and quitting waits
//! for the writes on their way. A draft keeps the words, not their styles (the Mac app's too).

use std::collections::HashMap;
use std::sync::Arc;

use async_compat::Compat;
use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::future::Shared;
use futures::{FutureExt, StreamExt};
use gpui_kit::*;
use sig_core::{Client, CoreError};

use super::{describe, Hush};

/// A chat and its draft (blank: none).
type Change = (String, String);

/// Every chat's unsent text: the open chat's as typed, unless the composer holds an edit.
/// shortcut: read once, where the Mac app reads them again when the chat list changes (a chat
/// deleted on the phone takes its draft in the core): such a draft stays here until the next
/// launch. Read them again on `ConversationsChanged` if a chat comes back with its old draft.
#[derive(Default)]
pub(super) struct Drafts {
    kept: HashMap<String, String>,
    /// Where changes go to be written: none until the core's drafts were read, and for good if
    /// that failed.
    writes: Option<UnboundedSender<Change>>,
}

impl Drafts {
    pub(super) fn get(&self, id: &str) -> Option<&String> {
        self.kept.get(id)
    }

    /// `text` is chat `id`'s draft now (blank: it has none); written if that changed.
    pub(super) fn keep(&mut self, id: &str, text: String) {
        let blank = text.trim().is_empty();
        if self.kept.get(id).map_or(blank, |kept| *kept == text) {
            return;
        }
        if blank {
            self.kept.remove(id);
        } else {
            self.kept.insert(id.to_owned(), text.clone());
        }
        if let Some(writes) = &self.writes {
            // closed: quitting, the core is stopping
            let _ = writes.unbounded_send((id.to_owned(), text));
        }
    }

    /// The core's drafts, read: they replace any typed before (the Mac app keeps none until then),
    /// and what is typed from now on is written.
    fn loaded(&mut self, stored: HashMap<String, String>, writes: UnboundedSender<Change>) {
        self.kept = stored;
        self.writes = Some(writes);
    }
}

/// The draft writes on their way, for `quit`.
pub(super) struct Writer {
    queue: UnboundedSender<Change>,
    done: Shared<Task<()>>,
}

impl Writer {
    /// No more writes: ends once those queued are written.
    pub(super) fn finish(&self) -> Shared<Task<()>> {
        self.queue.close_channel();
        self.done.clone()
    }
}

impl Hush {
    /// Reads the drafts the core keeps, then writes the changes. A failed read is said; what is
    /// typed then is kept only while the app runs, never written over drafts it couldn't read
    /// (`AppStore.loadDrafts`).
    pub(super) fn load_drafts(&mut self, client: Arc<Client>, cx: &mut Context<Self>) -> Writer {
        let (queue, queued) = unbounded();
        let writes = queue.clone();
        let done = cx.spawn(async move |this, cx| {
            let stored = Compat::new(client.drafts()).await;
            let _ = this.update(cx, |app, cx| {
                match stored {
                    Ok(stored) => app.drafts.loaded(stored, writes),
                    Err(e) => {
                        tracing::warn!(error = %e, "the drafts couldn't be read");
                        app.error = Some(format!(
                            "Couldn't read your drafts ({}). What you type isn't kept after Hush quits.",
                            describe(&e)
                        ))
                    }
                }
                cx.notify();
            });
            write(&client, queued, |e| {
                tracing::warn!(error = %e, "a draft couldn't be kept");
                // the window may be gone (quitting): the writes go on
                let _ = this.update(cx, |app, cx| {
                    app.error = Some(format!("Couldn't keep the draft: {}", describe(&e)));
                    cx.notify();
                });
            })
            .await;
        });
        Writer {
            queue,
            done: done.shared(),
        }
    }

    /// The draft a chat's row shows: none for the open chat, whose draft is in its composer.
    pub(super) fn listed_draft(&self, id: &str) -> Option<&str> {
        let open = self.selected.as_deref() == Some(id);
        self.drafts.get(id).filter(|_| !open).map(String::as_str)
    }

    /// The composer changed: what it holds is now the open chat's draft, unless it is an edit (then
    /// the draft from before the edit stays).
    pub(super) fn keep_draft(&mut self, cx: &App) {
        let Some(id) = self.selected.clone().filter(|_| !self.actions.editing()) else {
            return;
        };
        let words = self.typed(cx).words();
        self.drafts.keep(&id, words);
    }
}

/// Writes the changes until the queue is closed and empty; of those queued meanwhile (the database
/// busy), only each chat's last, so quitting waits for one write a chat at most.
async fn write(client: &Client, mut queued: UnboundedReceiver<Change>, mut failed: impl FnMut(CoreError)) {
    while let Some(first) = queued.next().await {
        let mut latest = HashMap::from([first]);
        while let Ok((id, text)) = queued.try_recv() {
            latest.insert(id, text);
        }
        for (id, text) in latest {
            if let Err(e) = Compat::new(client.set_draft(id, text)).await {
                failed(e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use async_compat::Compat;
    use futures::channel::mpsc::{unbounded, UnboundedReceiver};
    use sig_core::Client;

    use super::{write, Change, Drafts};

    /// The writes asked for since last time.
    pub(super) fn written(queued: &mut UnboundedReceiver<Change>) -> Vec<Change> {
        std::iter::from_fn(|| queued.try_recv().ok()).collect()
    }

    fn change(id: &str, text: &str) -> Change {
        (id.into(), text.into())
    }

    #[test]
    fn a_change_is_written_once_read_and_blank_text_clears() {
        let mut drafts = Drafts::default();
        drafts.keep("c:a", "before reading".into());
        let (writes, mut queued) = unbounded();
        drafts.loaded(HashMap::from([change("c:b", "for bob")]), writes);
        assert_eq!(drafts.get("c:a"), None, "the core's drafts replace those typed before");
        drafts.keep("c:b", "for bob".into());
        assert!(written(&mut queued).is_empty(), "no change");
        drafts.keep("c:b", "for bob, later".into());
        drafts.keep("c:b", " \n".into());
        drafts.keep("c:c", String::new());
        assert_eq!(
            written(&mut queued),
            [change("c:b", "for bob, later"), change("c:b", " \n")],
            "nothing to clear in c:c"
        );
        assert_eq!(drafts.get("c:b"), None);
    }

    struct Quiet;

    impl sig_core::EventListener for Quiet {
        fn on_event(&self, _: sig_core::CoreEvent) {}
    }

    #[test]
    fn each_chats_last_queued_text_is_in_the_core_when_reopened() {
        let dir = std::env::temp_dir().join(format!("hush-desktop-drafts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let open = || Client::open(dir.to_str().unwrap().into(), "ab".repeat(32), Arc::new(Quiet)).unwrap();
        let client = open();
        let (queue, queued) = unbounded();
        for (id, text) in [
            ("c:a", "half"),
            ("c:a", "half-written"),
            ("c:b", "for bob"),
            ("c:b", ""),
        ] {
            queue.unbounded_send(change(id, text)).unwrap();
        }
        // quitting: those queued are written, then the writer ends
        queue.close_channel();
        let mut failed = vec![];
        futures::executor::block_on(write(&client, queued, |e| failed.push(e.to_string())));
        assert!(failed.is_empty(), "{failed:?}");
        drop(client);

        let again = open();
        let kept = futures::executor::block_on(Compat::new(again.drafts())).unwrap();
        assert_eq!(kept, HashMap::from([change("c:a", "half-written")]));
        drop(again);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// In a real (headless) window: `cargo test -p hush-desktop --features snapshot`.
#[cfg(all(test, feature = "snapshot"))]
mod window_tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use async_compat::Compat;
    use futures::channel::mpsc::{unbounded, UnboundedReceiver};
    use futures::FutureExt as _;
    use gpui_kit::{gpui, AnyWindowHandle, AppContext as _, Context, Entity, TestAppContext, Window, WindowOptions};
    use sig_core::Client;

    use super::super::compose::Typed;
    use super::super::Hush;
    use super::tests::written;
    use super::Change;
    use crate::demo::ANA;
    use crate::Demo;

    struct Quiet;

    impl sig_core::EventListener for Quiet {
        fn on_event(&self, _: sig_core::CoreEvent) {}
    }

    /// A core over an empty folder of its own; not linked, so a send fails at once.
    fn core(name: &str) -> (Arc<Client>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("hush-desktop-drafts-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (reopen(&dir), dir)
    }

    fn reopen(dir: &std::path::Path) -> Arc<Client> {
        Client::open(dir.to_str().unwrap().into(), "ab".repeat(32), Arc::new(Quiet)).unwrap()
    }

    /// The demo's chat (Family) with the composer focused.
    fn window(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Hush>) {
        let (window, hush) = cx.update(|cx| {
            gpui_kit::init(cx);
            cx.bind_keys(crate::key_bindings());
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Hush::demo(Demo::Chat, window, cx))
            })
            .expect("the window opens")
        });
        in_window(window, &hush, cx, |app, window, cx| {
            app.composer.update(cx, |c, cx| c.focus(window, cx))
        });
        cx.run_until_parked();
        (window, hush)
    }

    /// `window`, Ana's draft as if read from the core; and the writes asked for.
    fn open(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Hush>, UnboundedReceiver<Change>) {
        let (window, hush) = window(cx);
        let (writes, queued) = unbounded();
        hush.update(cx, |app, _| {
            app.drafts
                .loaded(HashMap::from([(format!("c:{ANA}"), "for ana".into())]), writes)
        });
        (window, hush, queued)
    }

    fn in_window(
        window: AnyWindowHandle,
        hush: &Entity<Hush>,
        cx: &mut TestAppContext,
        f: impl FnOnce(&mut Hush, &mut Window, &mut Context<Hush>),
    ) {
        window
            .update(cx, |_, window, cx| hush.update(cx, |app, cx| f(app, window, cx)))
            .unwrap();
    }

    fn typed(hush: &Entity<Hush>, cx: &mut TestAppContext) -> String {
        hush.read_with(cx, |app, cx| app.composer.read(cx).value().to_string())
    }

    fn listed(hush: &Entity<Hush>, id: &str, cx: &mut TestAppContext) -> Option<String> {
        hush.read_with(cx, |app, _| app.listed_draft(id).map(str::to_owned))
    }

    fn error(hush: &Entity<Hush>, cx: &mut TestAppContext) -> String {
        hush.read_with(cx, |app, _| app.error.clone()).unwrap_or_default()
    }

    fn edit_ours(window: AnyWindowHandle, hush: &Entity<Hush>, cx: &mut TestAppContext) {
        in_window(window, hush, cx, |app, window, cx| {
            let ours = app.messages.iter().rev().find(|m| m.is_outgoing && !m.deleted).cloned();
            app.start_editing(&ours.expect("one of ours"), window, cx);
        });
    }

    fn change(id: &str, text: &str) -> Change {
        (id.into(), text.into())
    }

    /// Runs the window's tasks until `done`, letting the core answer from its own threads.
    fn wait(what: &str, cx: &mut TestAppContext, mut done: impl FnMut(&mut TestAppContext) -> bool) {
        cx.executor().allow_parking();
        for _ in 0..500 {
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("{what}: the core didn't answer in 5 s");
    }

    #[gpui::test]
    fn what_is_typed_is_kept_and_sending_or_clearing_it_removes_it(cx: &mut TestAppContext) {
        let (window, hush, mut queued) = open(cx);
        let ana = format!("c:{ANA}");
        cx.simulate_input(window, "half");
        assert_eq!(written(&mut queued).last(), Some(&change("g:01", "half")));

        // an edit isn't a draft: the one from before it stays
        edit_ours(window, &hush, cx);
        cx.simulate_input(window, " more");
        in_window(window, &hush, cx, |app, window, cx| {
            app.cancel(window, cx);
        });
        assert_eq!(typed(&hush, cx), "half");
        assert!(written(&mut queued).is_empty());

        // sent, then an edit started before it failed: its words come back when the edit ends
        let (client, dir) = core("window");
        hush.update(cx, |app, _| app.client = Some(client.clone()));
        in_window(window, &hush, cx, |app, window, cx| app.send(window, cx));
        edit_ours(window, &hush, cx);
        cx.run_until_parked();
        assert!(
            error(&hush, cx).ends_with("Its text is back in its chat."),
            "{}",
            error(&hush, cx)
        );
        assert_eq!(written(&mut queued), [change("g:01", ""), change("g:01", "half")]);
        in_window(window, &hush, cx, |app, window, cx| {
            app.cancel(window, cx);
        });
        assert_eq!(typed(&hush, cx), "half");
        hush.update(cx, |app, _| app.client = None);

        // another chat: its draft in the composer, this one's in the list
        in_window(window, &hush, cx, |app, window, cx| app.select(ana.clone(), window, cx));
        assert_eq!(typed(&hush, cx), "for ana");
        assert_eq!(listed(&hush, "g:01", cx).as_deref(), Some("half"));
        assert_eq!(listed(&hush, &ana, cx), None, "the open chat's is in its composer");
        assert!(written(&mut queued).is_empty(), "nothing changed");

        cx.simulate_keystrokes(window, "secondary-a backspace");
        assert_eq!(written(&mut queued), [change(&ana, "")], "cleared by hand");

        // sent: gone; the failed send brings it back
        cx.simulate_input(window, "see you");
        written(&mut queued);
        hush.update(cx, |app, _| app.client = Some(client));
        in_window(window, &hush, cx, |app, window, cx| app.send(window, cx));
        cx.run_until_parked();
        assert_eq!(typed(&hush, cx), "see you");
        assert_eq!(written(&mut queued), [change(&ana, ""), change(&ana, "see you")]);

        // sent, then something new typed before it failed: that stays, and isn't said to be back
        in_window(window, &hush, cx, |app, window, cx| {
            app.send(window, cx);
            app.set_typed(Typed::plain("newer".into()), window, cx);
        });
        cx.run_until_parked();
        assert_eq!(typed(&hush, cx), "newer");
        assert_eq!(written(&mut queued), [change(&ana, ""), change(&ana, "newer")]);
        assert!(!error(&hush, cx).contains("back in its chat"), "{}", error(&hush, cx));
        hush.update(cx, |app, _| app.client = None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[gpui::test]
    fn a_draft_typed_before_quitting_is_there_at_the_next_launch(cx: &mut TestAppContext) {
        let ana = format!("c:{ANA}");
        let (client, dir) = core("launch");
        // from the last launch
        futures::executor::block_on(Compat::new(client.set_draft(ana.clone(), "for ana".into()))).unwrap();
        let (window, hush) = window(cx);
        let writer = hush.update(cx, |app, cx| app.load_drafts(client.clone(), cx));
        wait("read", cx, |cx| listed(&hush, &ana, cx).is_some());
        assert_eq!(listed(&hush, &ana, cx).as_deref(), Some("for ana"));

        // a delete that fails keeps the draft (the core knows no such chat)
        hush.update(cx, |app, cx| {
            app.client = Some(client.clone());
            app.delete_chat(ana.clone(), cx);
        });
        wait("delete", cx, |cx| !error(&hush, cx).is_empty());
        assert_eq!(listed(&hush, &ana, cx).as_deref(), Some("for ana"));
        hush.update(cx, |app, _| app.client = None);

        cx.simulate_input(window, "half");
        // quitting: what was typed is written first
        let written = writer.finish();
        wait("written", cx, |_| written.clone().now_or_never().is_some());
        drop((writer, written, client));
        let again = reopen(&dir);
        let kept = futures::executor::block_on(Compat::new(again.drafts())).unwrap();
        assert_eq!(kept, HashMap::from([change(&ana, "for ana"), change("g:01", "half")]));
        drop(again);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
