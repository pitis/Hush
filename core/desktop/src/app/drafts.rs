//! Unsent text, kept by the core in its encrypted database so it outlives the app
//! (`AppStore.saveDraft`, `loadDrafts`): each change is written in turn, and quitting waits for
//! the writes on their way. A draft keeps the words, not their styles (the Mac app's too).

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
    /// Where changes go to be written: none until the core's drafts were read.
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

    /// The core's drafts, read: what is typed from now on is written.
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
    /// Reads the drafts the core keeps, then writes each change. A failed read is said, and
    /// nothing typed is kept until the next launch (`AppStore.loadDrafts`).
    pub(super) fn load_drafts(&mut self, client: Arc<Client>, cx: &mut Context<Self>) -> Writer {
        let (queue, queued) = unbounded();
        let writes = queue.clone();
        let done = cx.spawn(async move |this, cx| {
            let stored = Compat::new(client.drafts()).await;
            let _ = this.update(cx, |app, cx| {
                match stored {
                    Ok(stored) => app.drafts.loaded(stored, writes),
                    Err(e) => {
                        app.error = Some(format!(
                            "Couldn't read your drafts ({}). What you type isn't kept as a draft.",
                            describe(&e)
                        ))
                    }
                }
                cx.notify();
            });
            write(&client, queued, |e| {
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

    /// The composer changed: what it holds is the open chat's draft, but an edit (the draft from
    /// before it stays).
    pub(super) fn keep_draft(&mut self, cx: &App) {
        let Some(id) = self.selected.clone().filter(|_| !self.actions.editing()) else {
            return;
        };
        let words = self.typed(cx).words();
        self.drafts.keep(&id, words);
    }
}

/// Writes each change in turn, until the queue is closed and empty.
async fn write(client: &Client, mut queued: UnboundedReceiver<Change>, mut failed: impl FnMut(CoreError)) {
    while let Some((id, text)) = queued.next().await {
        if let Err(e) = Compat::new(client.set_draft(id, text)).await {
            failed(e);
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
    fn the_drafts_written_before_quitting_are_there_at_the_next_launch() {
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
    use std::sync::Arc;

    use futures::channel::mpsc::{unbounded, UnboundedReceiver};
    use gpui_kit::{gpui, AnyWindowHandle, AppContext as _, Context, Entity, TestAppContext, Window, WindowOptions};

    use super::super::Hush;
    use super::tests::written;
    use super::Change;
    use crate::demo::ANA;
    use crate::Demo;

    struct Quiet;

    impl sig_core::EventListener for Quiet {
        fn on_event(&self, _: sig_core::CoreEvent) {}
    }

    /// The demo's chat, Ana's draft read from the core, the composer with the keys; and the
    /// writes asked for.
    fn open(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Hush>, UnboundedReceiver<Change>) {
        let (window, hush) = cx.update(|cx| {
            gpui_kit::init(cx);
            cx.bind_keys(crate::key_bindings());
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Hush::demo(Demo::Chat, window, cx))
            })
            .expect("the window opens")
        });
        let (writes, queued) = unbounded();
        in_window(window, &hush, cx, |app, window, cx| {
            app.drafts
                .loaded(HashMap::from([(format!("c:{ANA}"), "for ana".into())]), writes);
            app.composer.update(cx, |c, cx| c.focus(window, cx));
        });
        cx.run_until_parked();
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

    fn change(id: &str, text: &str) -> Change {
        (id.into(), text.into())
    }

    #[gpui::test]
    fn what_is_typed_is_kept_and_sending_or_clearing_it_removes_it(cx: &mut TestAppContext) {
        let (window, hush, mut queued) = open(cx);
        let ana = format!("c:{ANA}");
        cx.simulate_input(window, "half");
        assert_eq!(written(&mut queued).last(), Some(&change("g:01", "half")));

        // an edit isn't a draft: the one from before it stays
        in_window(window, &hush, cx, |app, window, cx| {
            let ours = app.messages.iter().rev().find(|m| m.is_outgoing && !m.deleted).cloned();
            app.start_editing(&ours.expect("one of ours"), window, cx);
        });
        cx.simulate_input(window, " more");
        in_window(window, &hush, cx, |app, window, cx| {
            app.cancel(window, cx);
        });
        assert_eq!(typed(&hush, cx), "half");
        assert!(written(&mut queued).is_empty());

        // another chat: its draft in the composer, this one's in the list
        in_window(window, &hush, cx, |app, window, cx| app.select(ana.clone(), window, cx));
        assert_eq!(typed(&hush, cx), "for ana");
        let family = hush.read_with(cx, |app, _| app.drafts.get("g:01").cloned());
        assert_eq!(family.as_deref(), Some("half"));
        assert!(written(&mut queued).is_empty(), "nothing changed");

        cx.simulate_keystrokes(window, "secondary-a backspace");
        assert_eq!(written(&mut queued), [change(&ana, "")], "cleared by hand");

        // sent: gone; an unlinked core fails the send at once, which brings it back
        cx.simulate_input(window, "see you");
        written(&mut queued);
        let dir = std::env::temp_dir().join(format!("hush-desktop-drafts-window-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let client = sig_core::Client::open(dir.to_str().unwrap().into(), "ab".repeat(32), Arc::new(Quiet)).unwrap();
        hush.update(cx, |app, _| app.client = Some(client));
        in_window(window, &hush, cx, |app, window, cx| app.send(window, cx));
        cx.run_until_parked();
        assert_eq!(typed(&hush, cx), "see you");
        assert_eq!(written(&mut queued), [change(&ana, ""), change(&ana, "see you")]);
        hush.update(cx, |app, _| app.client = None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
