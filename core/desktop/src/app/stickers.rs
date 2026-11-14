//! Stickers (the Mac app's `StickerPicker` and `AppStore.sendSticker`): "+" ▸ Stickers… opens a
//! picker of the installed packs, a tab each; a sticker sends at a click, as a reply if one is set.
//! A pack is added by its link (or on the phone). In the timeline a sticker stands on its own,
//! without a bubble (`files::Files::sticker`).

use std::collections::HashSet;

use async_compat::Compat;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::*;
use sig_core::projection::StickerPackInfo;

use super::{actions, describe, Hush};

mod view;

/// A sticker in the picker is 72 points (a pack's tab shows its first smaller): decoded at 2×.
const PICKER_PIXELS: u32 = 144;

#[derive(Default)]
pub(super) struct Stickers {
    /// The installed packs, as the picker last loaded them.
    packs: Vec<StickerPackInfo>,
    picker: Option<Picker>,
}

/// The picker, open over the chat open then.
struct Picker {
    generation: u64,
    /// The pack whose stickers show; None, or one no longer installed: the first.
    pack: Option<String>,
    /// A pack's link, to add it.
    link: Entity<InputState>,
    adding: bool,
    /// Why the packs didn't load, or the link didn't add one.
    problem: Option<String>,
}

/// The pack shown: the one chosen, else the first.
fn shown<'a>(packs: &'a [StickerPackInfo], chosen: Option<&str>) -> Option<&'a StickerPackInfo> {
    packs.iter().find(|p| Some(p.id.as_str()) == chosen).or(packs.first())
}

/// The pack to show once a link added one: the pack new since `before` (none new: it was
/// installed already, and the one chosen stays).
fn added(before: &HashSet<String>, packs: &[StickerPackInfo], chosen: Option<String>) -> Option<String> {
    let new = packs.iter().find(|p| !before.contains(&p.id));
    new.map(|p| p.id.clone()).or(chosen)
}

/// Add is on for a link typed, not while one is being added.
fn can_add(link: &str, adding: bool) -> bool {
    !link.trim().is_empty() && !adding
}

impl Hush {
    /// "+" ▸ Stickers…: the picker, its packs read again (as the Mac app's each time it opens).
    pub(super) fn open_stickers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_attach() {
            return;
        }
        let link = cx.new(|cx| InputState::new(window, cx).placeholder("Sticker pack link"));
        cx.subscribe_in(&link, window, |this, _, event: &InputEvent, window, cx| match event {
            InputEvent::PressEnter { .. } => this.add_sticker_pack(window, cx),
            // Add turns on or off
            InputEvent::Change => cx.notify(),
            _ => {}
        })
        .detach();
        self.stickers.picker = Some(Picker {
            generation: self.generation,
            pack: None,
            link,
            adding: false,
            problem: None,
        });
        self.load_sticker_packs(cx);
        cx.notify();
    }

    /// The picker closes (a click outside it, Esc, a sticker sent): the composer has the keys.
    pub(super) fn close_stickers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.stickers.picker.take().is_some() && self.can_attach() {
            self.composer.update(cx, |input, cx| input.focus(window, cx));
        }
        cx.notify();
    }

    /// The picker, while it is over the chat it was opened in.
    fn open_picker(&self) -> Option<&Picker> {
        let picker = self.stickers.picker.as_ref();
        picker.filter(|p| p.generation == self.generation)
    }

    /// The pictures the open picker shows: each pack's first (its tab) and its pack's.
    pub(super) fn picker_paths(&self) -> Vec<String> {
        let Some(picker) = self.open_picker() else {
            return vec![];
        };
        let packs = &self.stickers.packs;
        let tabs = packs.iter().filter_map(|p| p.stickers.first());
        let pack = shown(packs, picker.pack.as_deref())
            .into_iter()
            .flat_map(|p| &p.stickers);
        tabs.chain(pack).map(|s| s.path.clone()).collect()
    }

    fn load_sticker_packs(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        cx.spawn(async move |this, cx| {
            let packs = Compat::new(client.sticker_packs()).await;
            let _ = this.update(cx, |app, cx| {
                match packs {
                    Ok(packs) => app.stickers.packs = packs,
                    Err(e) => {
                        if let Some(p) = &mut app.stickers.picker {
                            p.problem = Some(format!("Couldn't load the sticker packs: {}", describe(&e)));
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn choose_pack(&mut self, id: String, cx: &mut Context<Self>) {
        if let Some(p) = &mut self.stickers.picker {
            p.pack = Some(id);
            cx.notify();
        }
    }

    /// Adds the pack the link names, then shows it (`StickerPicker.add`); why it couldn't is said
    /// under the link.
    fn add_sticker_pack(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        let Some(picker) = &mut self.stickers.picker else {
            return;
        };
        let link = picker.link.read(cx).value().to_string();
        if !can_add(&link, picker.adding) {
            return;
        }
        picker.adding = true;
        picker.problem = None;
        let field = picker.link.clone();
        let before: HashSet<String> = self.stickers.packs.iter().map(|p| p.id.clone()).collect();
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let added_pack = Compat::new(client.add_sticker_pack(link)).await;
            let packs = match added_pack {
                Ok(()) => Some(Compat::new(client.sticker_packs()).await),
                Err(_) => None,
            };
            let _ = this.update_in(cx, |app, window, cx| {
                if let Some(Ok(packs)) = &packs {
                    app.stickers.packs = packs.clone();
                }
                // closed meanwhile (and maybe opened again): only the packs change
                let Some(picker) = app.stickers.picker.as_mut().filter(|p| p.link == field) else {
                    return;
                };
                picker.adding = false;
                match (added_pack, packs) {
                    (Err(e), _) => picker.problem = Some(describe(&e)),
                    (Ok(()), Some(Err(e))) => {
                        picker.problem = Some(format!("Couldn't load the sticker packs: {}", describe(&e)))
                    }
                    (Ok(()), _) => {
                        picker.pack = added(&before, &app.stickers.packs, picker.pack.take());
                        field.update(cx, |f, cx| f.set_value("", window, cx));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Sends a sticker to the open chat, as a reply if one is set (`AppStore.sendSticker`), once
    /// the picker closes. The core shows it at once as a pending message; a failure is said, and
    /// the reply goes back into the composer.
    fn send_sticker(&mut self, pack: String, sticker: u32, window: &mut Window, cx: &mut Context<Self>) {
        self.close_stickers(window, cx);
        let (Some(client), Some(id)) = (self.client.clone(), self.selected.clone()) else {
            return;
        };
        let reply = self.take_reply();
        let quote = reply.as_ref().map(actions::quote);
        cx.spawn(async move |this, cx| {
            let sent = Compat::new(client.send_sticker(id.clone(), pack, sticker, quote)).await;
            let Err(e) = sent else { return };
            let _ = this.update(cx, |app, cx| {
                app.error = Some(describe(&e));
                if app.selected.as_deref() == Some(id.as_str()) {
                    app.restore_reply(reply);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// `--snapshot`'s stickers: received, sent and one still to download, and with `picker` the
    /// picker open over them.
    #[cfg(feature = "snapshot")]
    pub(super) fn demo_stickers(&mut self, picker: bool, window: &mut Window, cx: &mut Context<Self>) {
        use super::files::{picture, Files, STICKER_PIXELS};
        let made = |i: usize, max: u32| picture(&crate::demo::sticker(crate::demo::sticker_colour(i)), max);
        let (messages, files) = crate::demo::sticker_messages();
        self.files = Files::demo(files.clone());
        for (i, path) in files.iter().filter_map(|a| a.path.clone()).enumerate() {
            let shown = made(i * 3 + 2, STICKER_PIXELS).expect("the made-up sticker decodes");
            self.files.demo_picture(path, shown);
        }
        self.stickers.packs = crate::demo::sticker_packs();
        for (n, pack) in self.stickers.packs.iter().enumerate() {
            for (i, s) in pack.stickers.iter().enumerate() {
                let shown = made(i + n * 3, PICKER_PIXELS).expect("the made-up sticker decodes");
                self.files.demo_picture(s.path.clone(), shown);
            }
        }
        if picker {
            self.open_stickers(window, cx);
        }
        self.set_messages(messages, cx);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use sig_core::projection::{StickerInfo, StickerPackInfo};

    use super::{added, can_add, shown};

    fn pack(id: &str) -> StickerPackInfo {
        StickerPackInfo {
            id: id.into(),
            title: id.to_uppercase(),
            author: String::new(),
            stickers: vec![StickerInfo {
                id: 0,
                emoji: None,
                path: format!("{id}/0"),
            }],
        }
    }

    #[test]
    fn the_pack_chosen_shows_else_the_first() {
        let packs = [pack("a"), pack("b")];
        let id = |p: Option<&StickerPackInfo>| p.map(|p| p.id.clone());
        assert_eq!(id(shown(&packs, None)), Some("a".into()));
        assert_eq!(id(shown(&packs, Some("b"))), Some("b".into()));
        assert_eq!(
            id(shown(&packs, Some("gone"))),
            Some("a".into()),
            "removed on the phone"
        );
        assert!(shown(&[], Some("a")).is_none());
    }

    #[test]
    fn a_pack_added_by_its_link_shows_and_one_installed_already_changes_nothing() {
        let before: HashSet<String> = ["a".to_string(), "b".to_string()].into();
        let now = [pack("a"), pack("c"), pack("b")];
        assert_eq!(added(&before, &now, Some("b".into())), Some("c".into()));
        let same = [pack("a"), pack("b")];
        assert_eq!(added(&before, &same, Some("b".into())), Some("b".into()));
        assert_eq!(added(&before, &same, None), None);
    }

    #[test]
    fn add_wants_a_link_and_one_add_at_a_time() {
        assert!(can_add("https://signal.art/addstickers/#pack_id=…", false));
        assert!(!can_add("  \n", false));
        assert!(!can_add("sgnl://addstickers/?pack_id=…", true));
    }
}

/// Stickers in a real (headless) window: `cargo test -p hush-desktop --features snapshot`. A core
/// that isn't linked refuses at once what a linked one would send.
#[cfg(all(test, feature = "snapshot"))]
mod window_tests {
    use std::path::PathBuf;

    use gpui_kit::{gpui, AnyWindowHandle, AppContext as _, Context, Entity, TestAppContext, Window, WindowOptions};

    use super::super::attach::window_tests::with_core;
    use super::super::Hush;
    use crate::Demo;

    fn open(screen: Demo, name: &str, cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Hush>, PathBuf) {
        let (window, hush) = cx.update(|cx| {
            gpui_kit::init(cx);
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Hush::demo(screen, window, cx))
            })
            .expect("the window opens")
        });
        let data = std::env::temp_dir().join(format!("hush-desktop-stickers-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        (window, hush, data)
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
        cx.run_until_parked();
    }

    fn problem(hush: &Entity<Hush>, cx: &mut TestAppContext) -> Option<String> {
        hush.read_with(cx, |app, _| {
            app.stickers.picker.as_ref().and_then(|p| p.problem.clone())
        })
    }

    #[gpui::test]
    fn a_sticker_the_core_refuses_says_why_and_its_reply_comes_back(cx: &mut TestAppContext) {
        let (window, hush, data) = open(Demo::StickerPicker, "send", cx);
        with_core(&hush, &data, cx);
        in_window(window, &hush, cx, |app, window, cx| {
            let theirs = app.messages.iter().find(|m| !m.is_outgoing).cloned();
            app.reply(&theirs.expect("one of theirs"), window, cx);
            let pack = app.stickers.packs[0].id.clone();
            app.send_sticker(pack, 3, window, cx);
        });
        let (error, picker, bar) = hush.read_with(cx, |app, _| {
            let bar = app.composer_bar().map(|b| b.0);
            (app.error.clone(), app.stickers.picker.is_some(), bar)
        });
        assert_eq!(error.as_deref(), Some("not linked to a phone"));
        assert!(!picker, "it closed to send");
        assert_eq!(bar, Some("reply"), "the reply is back");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[gpui::test]
    fn the_picker_says_why_its_packs_or_a_link_didnt_come(cx: &mut TestAppContext) {
        // open already, its made-up pictures there: none is read from the core
        let (window, hush, data) = open(Demo::StickerPicker, "add", cx);
        with_core(&hush, &data, cx);
        // opened again: its packs are read again
        in_window(window, &hush, cx, |app, window, cx| app.open_stickers(window, cx));
        let unlinked = "Couldn't load the sticker packs: not linked to a phone";
        assert_eq!(problem(&hush, cx).as_deref(), Some(unlinked));
        // nothing typed: Add does nothing
        in_window(window, &hush, cx, |app, window, cx| app.add_sticker_pack(window, cx));
        assert_eq!(problem(&hush, cx).as_deref(), Some(unlinked));

        let link = "https://example.com/addstickers/#pack_id=00";
        in_window(window, &hush, cx, |app, window, cx| {
            let field = app.stickers.picker.as_ref().unwrap().link.clone();
            field.update(cx, |f, cx| f.set_value(link, window, cx));
            app.add_sticker_pack(window, cx);
        });
        assert_eq!(problem(&hush, cx).as_deref(), Some("not a sticker pack link"));
        let (adding, typed) = hush.read_with(cx, |app, cx| {
            let picker = app.stickers.picker.as_ref().unwrap();
            (picker.adding, picker.link.read(cx).value().to_string())
        });
        assert!(!adding);
        assert_eq!(typed, link, "kept, to be fixed");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[gpui::test]
    fn the_picker_doesnt_open_during_an_edit(cx: &mut TestAppContext) {
        let (window, hush, _) = open(Demo::Chat, "edit", cx);
        in_window(window, &hush, cx, |app, window, cx| {
            let ours = app.messages.iter().rev().find(|m| m.is_outgoing && !m.deleted).cloned();
            app.start_editing(&ours.expect("one of ours"), window, cx);
            app.open_stickers(window, cx);
        });
        assert!(hush.read_with(cx, |app, _| app.stickers.picker.is_none()));
    }

    #[gpui::test]
    fn the_open_pickers_pictures_stay_as_messages_load_and_go_once_it_closes(cx: &mut TestAppContext) {
        let (window, hush, _) = open(Demo::StickerPicker, "pictures", cx);
        let path = hush.read_with(cx, |app, _| app.stickers.packs[1].stickers[0].path.clone());
        let reload = |cx: &mut TestAppContext| {
            hush.update(cx, |app, cx| {
                let messages = app.messages.clone();
                app.set_messages(messages, cx);
            });
            hush.read_with(cx, |app, _| app.files.has_picture(&path))
        };
        // a message came, say: the messages load again
        assert!(reload(cx), "a tab's picture stays");
        in_window(window, &hush, cx, |app, window, cx| app.close_stickers(window, cx));
        assert!(!reload(cx));
    }
}
