//! Settings (the Mac app's `SettingsView` and `ProfileSettings.swift`): the appearance and what
//! notifications show, our name and about, chat settings sync to the phone, and relinking or
//! unlinking this computer; and the unlinked screen's Link Again.

use std::path::Path;

use async_compat::Compat;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::*;
use sig_core::CoreError;

use super::{attach, describe, files, navigate, server_failure, Core, Home, Hush, Screen};

pub(super) mod view;

actions!(hush, [OpenSettings]);

/// ⌘, (Ctrl-, off macOS).
pub fn keys() -> Vec<KeyBinding> {
    // not `secondary-`: `KeyBinding::new` doesn't know the system and panics on it
    let key = if cfg!(target_os = "macos") { "cmd-," } else { "ctrl-," };
    vec![KeyBinding::new(key, OpenSettings, None)]
}

/// What `keys` and the Hush menu's Settings… do, wherever the window's focus is.
pub fn shortcut(window: AnyWindowHandle, hush: WeakEntity<Hush>, cx: &mut App) {
    navigate::on(window, &hush, cx, |app, _: &OpenSettings, window, cx| {
        app.open_settings(window, cx)
    });
}

/// The Settings panel, and an unlink or relink on its way (from it, or the unlinked screen's).
#[derive(Default)]
pub(super) struct Settings {
    panel: Option<Panel>,
    /// The buttons that unlink or relink wait.
    working: bool,
}

/// The open panel (the Mac app's Settings window).
struct Panel {
    /// Tab and Shift-Tab stay inside it.
    focus: FocusHandle,
    given: Entity<InputState>,
    family: Entity<InputState>,
    about: Entity<InputState>,
    /// Our profile as the server has it, once read.
    saved: Option<Profile>,
    read_failure: Option<String>,
    save_failure: Option<String>,
    saving: bool,
    /// Bumped by each save: a read begun before it is stale.
    generation: u64,
    /// Chat settings sync to the phone, once read.
    sync: Option<bool>,
    /// Opened over the chats: the account's part is read and shown (opened before, it would wait
    /// for reads never asked for).
    account: bool,
    /// Why the last thing done here failed, but the profile's own.
    problem: Option<String>,
}

/// Our name and about (the core's `OwnProfile`).
#[derive(Clone, Debug, Default, PartialEq)]
struct Profile {
    given: String,
    family: String,
    about: String,
}

impl Panel {
    /// What the fields hold, trimmed.
    fn typed(&self, cx: &App) -> Profile {
        let value = |f: &Entity<InputState>| f.read(cx).value().trim().to_string();
        Profile {
            given: value(&self.given),
            family: value(&self.family),
            about: value(&self.about),
        }
    }

    /// The fields hold something not saved yet.
    fn edited(&self, cx: &App) -> bool {
        self.saved.as_ref().is_some_and(|saved| *saved != self.typed(cx))
    }

    /// Save Profile: a change, with a first name.
    fn can_save(&self, cx: &App) -> bool {
        !self.saving && self.edited(cx) && !self.typed(cx).given.is_empty()
    }
}

/// A choice in Settings, kept as one word in a file of its own in the data folder (the Mac app's
/// user defaults); the core's unlink leaves it. One not kept, or not known, is the default.
pub(super) trait Choice: Global + Copy + Default + PartialEq {
    const FILE: &'static str;
    /// Each choice, its word in the file (the Mac app's) and its label.
    const ALL: &'static [(Self, &'static str, &'static str)];

    fn read(folder: &Path) -> Self {
        let kept = match std::fs::read_to_string(folder.join(Self::FILE)) {
            Ok(kept) => kept,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                tracing::warn!(error = %e, file = Self::FILE, "a setting kept couldn't be read");
                return Self::default();
            }
        };
        let found = Self::ALL.iter().find(|(_, word, _)| *word == kept.trim());
        found.map_or_else(
            || {
                tracing::warn!(file = Self::FILE, "a setting kept isn't one known");
                Self::default()
            },
            |(choice, _, _)| *choice,
        )
    }

    fn save(self, folder: &Path) -> std::io::Result<()> {
        let word = Self::ALL
            .iter()
            .find(|(choice, _, _)| *choice == self)
            .map_or("", |(_, word, _)| word);
        std::fs::create_dir_all(folder)?;
        std::fs::write(folder.join(Self::FILE), word)
    }
}

/// The choice made, or the default (none kept: made-up screens, tests).
pub(super) fn chosen<C: Choice>(cx: &App) -> C {
    cx.try_global::<C>().copied().unwrap_or_default()
}

/// The look the app takes: the system's, or always light or dark (the Mac app's `Appearance`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

impl Global for Appearance {}

impl Choice for Appearance {
    const FILE: &'static str = "appearance";
    const ALL: &'static [(Self, &'static str, &'static str)] = &[
        (Self::System, "system", "System"),
        (Self::Light, "light", "Light"),
        (Self::Dark, "dark", "Dark"),
    ];
}

impl Appearance {
    /// The colours, and on macOS the window's own chrome too (`NSApp.appearance`).
    fn apply(self, window: &mut Window, cx: &mut App) {
        let (forced, mode) = match self {
            Self::System => (None, None),
            Self::Light => (Some(WindowAppearance::Light), Some(ThemeMode::Light)),
            Self::Dark => (Some(WindowAppearance::Dark), Some(ThemeMode::Dark)),
        };
        cx.set_window_appearance(forced);
        match mode {
            Some(mode) => Theme::change(mode, Some(window), cx),
            None => Theme::sync_system_appearance(Some(window), cx),
        }
    }
}

/// What notifications show (the Mac app's `NotificationContent`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) enum NotificationContent {
    #[default]
    NameAndMessage,
    NameOnly,
    NoNameOrMessage,
    Off,
}

impl Global for NotificationContent {}

impl Choice for NotificationContent {
    const FILE: &'static str = "notifications";
    const ALL: &'static [(Self, &'static str, &'static str)] = &[
        (Self::NameAndMessage, "message", "Name, content, and actions"),
        (Self::NameOnly, "name", "Name only"),
        (Self::NoNameOrMessage, "count", "No name or content"),
        (Self::Off, "off", "Off"),
    ];
}

/// The choices kept, the appearance applied now, and the system's followed while it's chosen.
pub(super) fn load(window: &mut Window, cx: &mut Context<Hush>) {
    if let Some(folder) = cx.try_global::<Home>().map(|home| home.folder.clone()) {
        cx.set_global(NotificationContent::read(&folder));
        cx.set_global(Appearance::read(&folder));
    }
    chosen::<Appearance>(cx).apply(window, cx);
    cx.observe_window_appearance(window, |_, window, cx| {
        if chosen::<Appearance>(cx) == Appearance::System {
            Theme::sync_system_appearance(Some(window), cx)
        }
    })
    .detach();
}

impl Hush {
    /// Settings…: the panel over the window (not over another one: the keys stay with that); our
    /// profile is read each time it opens (the phone changes it too), and the sync switch.
    pub(super) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let over = self.dialog.is_some() || self.group.is_some() || self.view_once.showing();
        if self.settings.panel.is_some() || over {
            return;
        }
        let mut field = || {
            let field = cx.new(|cx| InputState::new(window, cx));
            // Save Profile waits for a change
            cx.subscribe(&field, |_, _, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    cx.notify();
                }
            })
            .detach();
            field
        };
        let (given, family, about) = (field(), field(), field());
        let focus = cx.focus_handle();
        // the composer behind doesn't keep the keys
        focus.focus(window, cx);
        self.settings.panel = Some(Panel {
            focus,
            given,
            family,
            about,
            saved: None,
            read_failure: None,
            save_failure: None,
            saving: false,
            generation: 0,
            sync: None,
            account: matches!(self.screen, Screen::Chats),
            problem: None,
        });
        if matches!(self.screen, Screen::Chats) {
            self.read_profile(window, cx);
            self.read_sync(cx);
        }
        cx.notify();
    }

    /// The panel is over the window: the chats' shortcuts wait.
    pub(super) fn settings_open(&self) -> bool {
        self.settings.panel.is_some()
    }

    /// Done or Esc: back to the open chat's composer. A save or an unlink on its way still lands.
    pub(super) fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.panel.take().is_some() {
            if matches!(self.screen, Screen::Chats) && self.selected.is_some() {
                self.composer.update(cx, |input, cx| input.focus(window, cx));
            }
            cx.notify();
        }
    }

    /// Reads our profile from the server (`ProfileSettings.load`).
    fn read_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(client), Some(p)) = (self.client.clone(), self.settings.panel.as_mut()) else {
            return;
        };
        p.read_failure = None;
        let started = p.generation;
        cx.spawn_in(window, async move |this, cx| {
            let read = Compat::new(client.own_profile()).await;
            let read = read
                .map(|p| Profile {
                    given: p.given_name,
                    family: p.family_name,
                    about: p.about,
                })
                .map_err(|e| server_failure("Couldn't read your profile", &e));
            let _ = this.update_in(cx, |app, window, cx| app.profile_read(started, read, window, cx));
        })
        .detach();
    }

    fn profile_read(
        &mut self,
        started: u64,
        read: Result<Profile, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(p) = &mut self.settings.panel else { return };
        // a save since (or under way) knows better
        if started != p.generation || p.saving {
            return;
        }
        match read {
            Ok(saved) => self.show_profile(saved, window, cx),
            Err(why) => p.read_failure = Some(why),
        }
        cx.notify();
    }

    /// `saved` as the server has it: the fields follow, unless they hold edits not yet saved.
    fn show_profile(&mut self, saved: Profile, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = &mut self.settings.panel else { return };
        if !p.edited(cx) {
            for (field, value) in [
                (&p.given, &saved.given),
                (&p.family, &saved.family),
                (&p.about, &saved.about),
            ] {
                field.update(cx, |f, cx| f.set_value(value.clone(), window, cx));
            }
        }
        p.saved = Some(saved);
    }

    /// Save Profile: our name and about, trimmed; the fields then show what was saved.
    fn save_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        let Some(p) = self.settings.panel.as_mut().filter(|p| p.can_save(cx)) else {
            return;
        };
        let typed = p.typed(cx);
        p.generation += 1;
        p.saving = true;
        p.save_failure = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let saved = Compat::new(client.set_profile(typed.given, typed.family, typed.about)).await;
            let _ = this.update_in(cx, |app, window, cx| {
                let saved = saved.map_err(|e| server_failure("Couldn't save your profile", &e));
                let Some(p) = &mut app.settings.panel else {
                    // closed meanwhile: a failure is still said
                    app.error = saved.err().or(app.error.take());
                    return cx.notify();
                };
                p.saving = false;
                match saved {
                    Ok(s) => {
                        // the fields take what was saved
                        p.saved = None;
                        let saved = Profile {
                            given: s.given_name,
                            family: s.family_name,
                            about: s.about,
                        };
                        app.show_profile(saved, window, cx);
                    }
                    Err(why) => p.save_failure = Some(why),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Reads whether chat settings changed here go to the phone.
    fn read_sync(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        cx.spawn(async move |this, cx| {
            let on = Compat::new(client.storage_writes()).await;
            let _ = this.update(cx, |app, cx| {
                let Some(p) = &mut app.settings.panel else { return };
                match on {
                    Ok(on) => p.sync = Some(on),
                    Err(e) => {
                        p.problem = Some(format!("Couldn't read the chat settings sync switch: {}", describe(&e)))
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Turns writing chat settings to the phone's account on or off; a refusal puts it back
    /// (`AppStore.setStorageWrites`).
    pub(super) fn set_sync(&mut self, on: bool, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        let Some(p) = self.settings.panel.as_mut().filter(|p| p.sync.is_some_and(|s| s != on)) else {
            return;
        };
        p.sync = Some(on);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let set = Compat::new(client.set_storage_writes(on)).await;
            let Err(e) = set else { return };
            let _ = this.update(cx, |app, cx| {
                if let Some(p) = &mut app.settings.panel {
                    p.sync = Some(!on);
                }
                app.settings_failed(format!("Couldn't change chat settings sync: {}", describe(&e)), cx);
            });
        })
        .detach();
    }

    /// Takes the look chosen, at once and from the next launch on.
    pub(super) fn choose_appearance(&mut self, look: Appearance, window: &mut Window, cx: &mut Context<Self>) {
        look.apply(window, cx);
        self.choose(look, cx);
    }

    /// Takes a choice from now on, and keeps it for the next launch.
    pub(super) fn choose<C: Choice>(&mut self, choice: C, cx: &mut Context<Self>) {
        cx.set_global(choice);
        let folder = cx.try_global::<Home>().map(|home| home.folder.clone());
        if let Some(Err(e)) = folder.map(|folder| choice.save(&folder)) {
            self.settings_failed(format!("Couldn't keep the setting: {e}"), cx);
        }
        cx.notify();
    }

    /// Says why in the panel, or above the chat list (or on the unlinked screen) once it's closed.
    fn settings_failed(&mut self, why: String, cx: &mut Context<Self>) {
        match &mut self.settings.panel {
            Some(p) => p.problem = Some(why),
            None => self.error = Some(why),
        }
        cx.notify();
    }

    /// Transfer Message History…: asks first, as the Mac app does.
    pub(super) fn confirm_relink(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(
            PromptLevel::Info,
            "Transfer message history from your phone?",
            Some(
                "This computer is relinked as a new device: you scan a QR code again, and your phone offers \
                 to transfer its message history. Everything already here is kept.",
            ),
            &["Relink and Transfer", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                let _ = this.update_in(cx, |app, window, cx| app.relink(window, cx));
            }
        })
        .detach();
    }

    /// This computer relinks as a new device; the chats here are kept and the phone's history
    /// merges in (`AppStore.relinkForHistory`).
    fn relink(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        if std::mem::replace(&mut self.settings.working, true) {
            return;
        }
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let relinked = Compat::new(client.relink()).await;
            let _ = this.update_in(cx, |app, window, cx| app.relinked(relinked, window, cx));
        })
        .detach();
    }

    /// The data stays, so its key does too. A relink fails only once the core has stopped: the
    /// app opens it again either way (linked still if the old link wasn't removed yet), where the
    /// Mac app stays with a stopped core.
    fn relinked(&mut self, relinked: Result<(), CoreError>, window: &mut Window, cx: &mut Context<Self>) {
        self.restart(false, window, cx);
        if let Err(e) = relinked {
            tracing::error!(error = %e, "relinking failed");
            self.error = Some(format!("Couldn't relink: {}", describe(&e)));
        }
    }

    /// Unlink This Computer…: asks first, as the Mac app does.
    pub(super) fn confirm_unlink(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(
            PromptLevel::Warning,
            "Unlink this computer?",
            Some("Your messages and files are removed from this computer. Your phone keeps them."),
            &["Unlink", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                let _ = this.update_in(cx, |app, window, cx| app.unlink(true, window, cx));
            }
        })
        .detach();
    }

    /// Unlinks this computer and removes all its data, telling the server first if `tell_server`
    /// (`AppStore.unlinkThisMac`); Link Again doesn't (the phone already removed it).
    pub(super) fn unlink(&mut self, tell_server: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        if std::mem::replace(&mut self.settings.working, true) {
            return;
        }
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let unlinked = Compat::new(client.unlink(tell_server)).await;
            let _ = this.update_in(cx, |app, window, cx| app.unlinked(tell_server, unlinked, window, cx));
        })
        .detach();
    }

    fn unlinked(
        &mut self,
        tell_server: bool,
        unlinked: Result<(), CoreError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings.working = false;
        match unlinked {
            Ok(()) => {
                // the decrypted copies of opened and pasted files go with the data, even if
                // starting over fails
                if let Some(home) = cx.try_global::<Home>() {
                    files::clear_open_copies(&home.folder);
                    attach::clear_pasted(&home.folder);
                }
                return self.restart(true, window, cx);
            }
            // nothing was removed, and the core runs on
            Err(e @ CoreError::Network { .. }) if tell_server => self.ask_unlink_anyway(describe(&e), window, cx),
            Err(e @ CoreError::Network { .. }) => self.settings_failed(describe(&e), cx),
            Err(e) => {
                // the core has stopped: there is no going back to the chats
                tracing::error!(error = %e, "removing the data failed");
                self.settings.panel = None;
                self.screen = Screen::Failed(format!(
                    "Hush couldn't remove its data ({}). Quit Hush, delete {}, and open it again.",
                    describe(&e),
                    self.data_dir.display()
                ));
            }
        }
        cx.notify();
    }

    /// The server couldn't be told: unlinking here only is offered.
    fn ask_unlink_anyway(&mut self, why: String, window: &mut Window, cx: &mut Context<Self>) {
        let detail = format!(
            "{why}\n\nUnlinking anyway removes everything here, but your phone keeps listing this computer until \
             you remove it there."
        );
        let answer = window.prompt(
            PromptLevel::Critical,
            "The server couldn't be reached",
            Some(&detail),
            &["Unlink Anyway", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                let _ = this.update_in(cx, |app, window, cx| app.unlink(false, window, cx));
            }
        })
        .detach();
    }

    /// Starts over with a fresh core, all that showed forgotten (`AppStore.restart`); the key and
    /// the notifications shown go with the data they belong to, unless relinking kept that.
    fn restart(&mut self, forget_key: bool, window: &mut Window, cx: &mut Context<Self>) {
        // the old core is stopped: quitting has nothing of it to wait for
        if cx.has_global::<Core>() {
            cx.remove_global::<Core>();
        }
        let notifications = self.start_over(forget_key, cx);
        *self = Self::window(window, cx);
        self.notifications = notifications;
        self.boot(forget_key, cx);
        cx.notify();
    }
}

#[cfg(feature = "snapshot")]
impl Hush {
    /// `--snapshot`'s Settings, with a made-up profile; or the unlinked screen.
    pub(super) fn demo_settings(&mut self, unlinked: bool, window: &mut Window, cx: &mut Context<Self>) {
        if unlinked {
            self.screen = Screen::Unlinked;
            return;
        }
        self.open_settings(window, cx);
        let me = Profile {
            given: "Maria".into(),
            family: "Ionescu".into(),
            about: "Out hiking until Sunday".into(),
        };
        self.show_profile(me, window, cx);
        if let Some(p) = &mut self.settings.panel {
            p.sync = Some(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Appearance, Choice, NotificationContent};

    #[test]
    fn a_choice_is_kept_and_read_back() {
        let folder = std::env::temp_dir().join(format!("hush-desktop-choices-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        assert_eq!(Appearance::read(&folder), Appearance::System, "none kept yet");
        assert_eq!(NotificationContent::read(&folder), NotificationContent::NameAndMessage);
        for (look, _, _) in Appearance::ALL {
            look.save(&folder).unwrap();
            assert_eq!(Appearance::read(&folder), *look);
        }
        for (content, _, _) in NotificationContent::ALL {
            content.save(&folder).unwrap();
            assert_eq!(NotificationContent::read(&folder), *content);
        }
        assert_eq!(Appearance::read(&folder), Appearance::Dark, "each in its own file");
        std::fs::write(folder.join(Appearance::FILE), "sepia\n").unwrap();
        assert_eq!(Appearance::read(&folder), Appearance::System, "one it doesn't know");
        std::fs::remove_dir_all(&folder).unwrap();
    }
}

/// Settings in a real (headless) window, over a core that isn't linked, in a folder of its own.
#[cfg(all(test, feature = "snapshot"))]
mod window_tests {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use gpui_kit::component::{Theme, ThemeMode};
    use gpui_kit::{gpui, AnyWindowHandle, AppContext as _, Context, Entity, TestAppContext, Window, WindowOptions};
    use sig_core::CoreError;

    use super::super::attach::window_tests::with_core;
    use super::super::{Home, Hush, Screen};
    use super::{chosen, Appearance, Choice, NotificationContent, Profile};
    use crate::Demo;

    /// A test's key is refused: the app doesn't open a core again (it would start linking).
    const REFUSED: &str = "not opened again in a test";

    fn refused() -> Result<String, String> {
        Err(REFUSED.into())
    }

    thread_local! {
        /// This test's core folder, and whether its key was forgotten (the app's background work
        /// runs on the test's thread).
        static KEY: RefCell<(Option<PathBuf>, bool)> = const { RefCell::new((None, false)) };
    }

    /// Forgets the key, which must outlive no data.
    fn forget() -> Result<(), String> {
        KEY.with_borrow_mut(|(core, forgot)| {
            let core = core.as_ref().expect("the test's folder");
            assert!(!core.join("projection.db").exists(), "the key outlives its data");
            *forgot = true;
        });
        Ok(())
    }

    fn forgot() -> bool {
        KEY.with_borrow(|(_, forgot)| *forgot)
    }

    fn keep_key() -> Result<(), String> {
        panic!("the key is kept")
    }

    fn open(screen: Demo, cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Hush>) {
        let opened = cx.update(|cx| {
            gpui_kit::init(cx);
            cx.bind_keys(crate::key_bindings());
            let (window, hush) = gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Hush::demo(screen, window, cx))
            })
            .expect("the window opens");
            super::shortcut(window, hush.downgrade(), cx);
            super::super::navigate::shortcuts(window, hush.downgrade(), cx);
            (window, hush)
        });
        cx.run_until_parked();
        opened
    }

    /// `open`, over a core in a data folder of its own (removed by the caller), which is the
    /// app's data folder; `forget_key` is called for its key.
    fn open_with_core(
        name: &str,
        screen: Demo,
        forget_key: fn() -> Result<(), String>,
        cx: &mut TestAppContext,
    ) -> (AnyWindowHandle, Entity<Hush>, PathBuf) {
        let data = std::env::temp_dir().join(format!("hush-desktop-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&data).unwrap();
        let (window, hush) = open(screen, cx);
        with_core(&hush, &data, cx);
        let folder = data.join("core");
        KEY.set((Some(folder.clone()), false));
        hush.update(cx, |app, _| app.data_dir = folder.clone());
        cx.update(|cx| {
            cx.set_global(Home {
                folder,
                key: refused,
                forget_key,
            })
        });
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

    /// Waits for the core's answer (on its own threads) until `done`.
    fn until(hush: &Entity<Hush>, cx: &mut TestAppContext, done: impl Fn(&Hush) -> bool) {
        for _ in 0..1000 {
            cx.run_until_parked();
            if hush.read_with(cx, |app, _| done(app)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the core didn't answer");
    }

    fn failed(app: &Hush) -> Option<&str> {
        match &app.screen {
            Screen::Failed(why) => Some(why),
            _ => None,
        }
    }

    /// Started over: everything shown was forgotten (the made-up chats, the open one, the panel),
    /// and the core opened again.
    fn started_over(hush: &Entity<Hush>, cx: &mut TestAppContext) -> bool {
        hush.read_with(cx, |app, _| {
            let forgotten = app.chats.is_empty() && app.selected.is_none() && app.settings.panel.is_none();
            forgotten && app.client.is_none() && failed(app) == Some(REFUSED)
        })
    }

    fn exists(core: &Path, file: &str) -> bool {
        core.join(file).exists()
    }

    fn put(path: PathBuf) -> PathBuf {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "soup").unwrap();
        path
    }

    #[gpui::test]
    fn unlinking_asks_then_removes_the_data_and_then_the_key_and_starts_over(cx: &mut TestAppContext) {
        let (window, hush, data) = open_with_core("unlink", Demo::Chat, forget, cx);
        let core = data.join("core");
        let kept = [
            put(core.join("open").join("1").join("menu.txt")),
            put(core.join("paste").join("shot.png")),
            put(core.join("attachments").join("photo")),
        ];
        assert!(exists(&core, "presage.db"));
        act(window, &hush, cx, |app, window, cx| app.open_settings(window, cx));
        act(window, &hush, cx, |app, window, cx| app.confirm_unlink(window, cx));
        assert_eq!(cx.pending_prompt().expect("asked").0, "Unlink this computer?");
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(exists(&core, "projection.db"), "kept");
        assert!(hush.read_with(cx, |app, _| app.settings.panel.is_some()));

        act(window, &hush, cx, |app, window, cx| app.confirm_unlink(window, cx));
        cx.simulate_prompt_answer("Unlink");
        until(&hush, cx, |app| failed(app).is_some());
        for gone in ["projection.db", "presage.db"] {
            assert!(!exists(&core, gone), "{gone} is removed");
        }
        for file in kept {
            assert!(!file.exists(), "{} goes with the data", file.display());
        }
        assert!(forgot(), "the key goes with the data");
        assert!(started_over(&hush, cx));
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[gpui::test]
    fn an_unreachable_server_offers_to_unlink_anyway(cx: &mut TestAppContext) {
        let (window, hush, data) = open_with_core("unreachable", Demo::Chat, forget, cx);
        let unreachable = |app: &mut Hush, window: &mut Window, cx: &mut Context<Hush>| {
            // as `unlink` leaves it
            app.settings.working = true;
            let e = CoreError::Network {
                message: "timed out".into(),
            };
            app.unlinked(true, Err(e), window, cx)
        };
        act(window, &hush, cx, unreachable);
        let (question, detail) = cx.pending_prompt().expect("asked");
        assert_eq!(question, "The server couldn't be reached");
        assert!(
            detail.starts_with("The server couldn't be reached: timed out\n\nUnlinking anyway"),
            "{detail}"
        );
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(exists(&data.join("core"), "projection.db"), "nothing removed");
        assert!(!forgot());
        assert!(hush.read_with(cx, |app, _| matches!(app.screen, Screen::Chats)
            && !app.chats.is_empty()));

        act(window, &hush, cx, unreachable);
        cx.simulate_prompt_answer("Unlink Anyway");
        until(&hush, cx, |app| failed(app).is_some());
        assert!(!exists(&data.join("core"), "projection.db"));
        assert!(forgot());
        assert!(started_over(&hush, cx));
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[gpui::test]
    fn data_that_couldnt_be_removed_says_which_folder_and_keeps_the_key(cx: &mut TestAppContext) {
        let (window, hush, data) = open_with_core("stuck", Demo::Chat, keep_key, cx);
        act(window, &hush, cx, |app, window, cx| {
            app.open_settings(window, cx);
            app.settings.working = true;
            let e = CoreError::Storage {
                message: "in use".into(),
            };
            app.unlinked(true, Err(e), window, cx)
        });
        let (why, panel, working) = hush.read_with(cx, |app, _| {
            let why = failed(app).map(str::to_owned).unwrap_or_default();
            (why, app.settings.panel.is_some(), app.settings.working)
        });
        let folder = data.join("core").display().to_string();
        assert!(why.starts_with("Hush couldn't remove its data ("), "{why}");
        assert!(why.contains(&folder), "{why}");
        assert!(!panel && !working, "the failure isn't under the panel");
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[gpui::test]
    fn relinking_asks_then_keeps_the_chats_and_the_key_and_starts_over(cx: &mut TestAppContext) {
        let (window, hush, data) = open_with_core("relink", Demo::Chat, keep_key, cx);
        let core = data.join("core");
        assert!(exists(&core, "presage.db"));
        act(window, &hush, cx, |app, window, cx| app.open_settings(window, cx));
        act(window, &hush, cx, |app, window, cx| app.confirm_relink(window, cx));
        assert_eq!(
            cx.pending_prompt().expect("asked").0,
            "Transfer message history from your phone?"
        );
        cx.simulate_prompt_answer("Relink and Transfer");
        until(&hush, cx, |app| failed(app).is_some());
        assert!(!exists(&core, "presage.db"), "the old link is gone");
        assert!(exists(&core, "projection.db"), "the chats stay");
        assert!(started_over(&hush, cx));
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[gpui::test]
    fn a_failed_relink_starts_over_too_and_says_so(cx: &mut TestAppContext) {
        let (window, hush, data) = open_with_core("relink-failed", Demo::Chat, keep_key, cx);
        act(window, &hush, cx, |app, window, cx| {
            app.relinked(Err(CoreError::Closed), window, cx)
        });
        until(&hush, cx, |app| failed(app).is_some());
        assert!(started_over(&hush, cx), "not left with a stopped core");
        let error = hush.read_with(cx, |app, _| app.error.clone()).unwrap_or_default();
        assert!(error.starts_with("Couldn't relink: "), "{error}");
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[gpui::test]
    fn link_again_removes_the_data_without_asking_and_starts_over(cx: &mut TestAppContext) {
        let (window, hush, data) = open_with_core("link-again", Demo::Unlinked, forget, cx);
        act(window, &hush, cx, |app, window, cx| app.unlink(false, window, cx));
        until(&hush, cx, |app| failed(app).is_some());
        assert!(!exists(&data.join("core"), "projection.db"));
        assert!(forgot());
        assert!(started_over(&hush, cx));
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[gpui::test]
    fn the_fields_follow_the_profile_read_unless_edited(cx: &mut TestAppContext) {
        let (window, hush, data) = open_with_core("profile", Demo::Chat, keep_key, cx);
        act(window, &hush, cx, |app, window, cx| app.open_settings(window, cx));
        // not linked: the read fails, and says so
        until(&hush, cx, |app| {
            app.settings.panel.as_ref().is_some_and(|p| p.read_failure.is_some())
        });
        let why = hush.read_with(cx, |app, _| app.settings.panel.as_ref().unwrap().read_failure.clone());
        assert!(why.is_some_and(|w| w.starts_with("Couldn't read your profile")));

        let ana = |about: &str| Profile {
            given: "Ana".into(),
            family: "Pop".into(),
            about: about.into(),
        };
        let typed = |hush: &Entity<Hush>, cx: &mut TestAppContext| {
            hush.read_with(cx, |app, cx| {
                let p = app.settings.panel.as_ref().unwrap();
                (p.typed(cx), p.can_save(cx))
            })
        };
        let saved = |hush: &Entity<Hush>, cx: &mut TestAppContext| {
            hush.read_with(cx, |app, _| app.settings.panel.as_ref().unwrap().saved.clone())
        };
        let read = ana("Hiking");
        act(window, &hush, cx, move |app, window, cx| {
            app.profile_read(0, Ok(read), window, cx)
        });
        assert_eq!(typed(&hush, cx), (ana("Hiking"), false), "shown; nothing to save");

        let given = hush.read_with(cx, |app, _| app.settings.panel.as_ref().unwrap().given.clone());
        let edit = given.clone();
        act(window, &hush, cx, move |_, window, cx| {
            edit.update(cx, |f, cx| f.set_value(" Ana ", window, cx))
        });
        assert!(!typed(&hush, cx).1, "spaces aren't a change");
        let edit = given.clone();
        act(window, &hush, cx, move |_, window, cx| {
            edit.update(cx, |f, cx| f.set_value("Anna", window, cx))
        });
        assert!(typed(&hush, cx).1, "a change to save");

        // the phone changed the about meanwhile: the edit stays, the change is what's saved now
        let read = ana("Climbing");
        act(window, &hush, cx, move |app, window, cx| {
            app.profile_read(0, Ok(read), window, cx)
        });
        assert_eq!(typed(&hush, cx).0.given, "Anna");
        assert_eq!(saved(&hush, cx), Some(ana("Climbing")));

        // a read that comes while a save is on its way, or that began before it, is stale; the
        // save fails (not linked) and says so, the edit kept to try again
        act(window, &hush, cx, |app, window, cx| {
            app.save_profile(window, cx);
            app.profile_read(1, Ok(ana("Old")), window, cx)
        });
        until(&hush, cx, |app| {
            app.settings.panel.as_ref().is_some_and(|p| p.save_failure.is_some())
        });
        let failure = hush.read_with(cx, |app, _| app.settings.panel.as_ref().unwrap().save_failure.clone());
        assert!(failure.is_some_and(|f| f.starts_with("Couldn't save your profile")));
        act(window, &hush, cx, |app, window, cx| {
            app.profile_read(0, Ok(ana("Old")), window, cx)
        });
        assert_eq!(saved(&hush, cx), Some(ana("Climbing")));
        assert_eq!(typed(&hush, cx).0.given, "Anna");
        assert!(typed(&hush, cx).1, "saved again");

        // a first name is needed
        act(window, &hush, cx, move |_, window, cx| {
            given.update(cx, |f, cx| f.set_value("  ", window, cx))
        });
        assert!(!typed(&hush, cx).1);
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[gpui::test]
    fn the_account_shows_only_when_opened_over_the_chats(cx: &mut TestAppContext) {
        let (window, hush) = open(Demo::Unlinked, cx);
        act(window, &hush, cx, |app, window, cx| app.open_settings(window, cx));
        let account = |hush: &Entity<Hush>, cx: &mut TestAppContext| {
            hush.read_with(cx, |app, _| app.settings.panel.as_ref().map(|p| p.account))
        };
        assert_eq!(account(&hush, cx), Some(false));
        // linked meanwhile: no read was asked for, so none waits to be shown
        act(window, &hush, cx, |app, _, _| app.screen = Screen::Chats);
        assert_eq!(account(&hush, cx), Some(false));
        act(window, &hush, cx, |app, window, cx| {
            app.close_settings(window, cx);
            app.open_settings(window, cx)
        });
        assert_eq!(account(&hush, cx), Some(true));
    }

    #[gpui::test]
    fn the_sync_switch_is_read_and_changed_in_the_core(cx: &mut TestAppContext) {
        let (window, hush, data) = open_with_core("sync", Demo::Chat, keep_key, cx);
        act(window, &hush, cx, |app, window, cx| app.open_settings(window, cx));
        let sync = |app: &Hush| app.settings.panel.as_ref().and_then(|p| p.sync);
        until(&hush, cx, |app| sync(app).is_some());
        assert_eq!(
            hush.read_with(cx, |app, _| sync(app)),
            Some(true),
            "on unless turned off"
        );
        act(window, &hush, cx, |app, _, cx| app.set_sync(false, cx));
        // read again, as each opening does: after the change (the core's one database connection)
        act(window, &hush, cx, |app, window, cx| {
            app.close_settings(window, cx);
            app.open_settings(window, cx)
        });
        until(&hush, cx, |app| sync(app).is_some());
        assert_eq!(hush.read_with(cx, |app, _| sync(app)), Some(false));
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[gpui::test]
    fn the_panel_keeps_the_keys_and_esc_closes_it(cx: &mut TestAppContext) {
        let (window, hush) = open(Demo::Chat, cx);
        cx.simulate_keystrokes(window, "secondary-,");
        assert!(
            hush.read_with(cx, |app, _| app.settings.panel.is_some()),
            "the shortcut opens it"
        );
        // the chats behind wait
        cx.simulate_keystrokes(window, "alt-down secondary-2 secondary-shift-c");
        assert_eq!(
            hush.read_with(cx, |app, _| app.selected.clone()).as_deref(),
            Some("g:01")
        );
        cx.simulate_keystrokes(window, "escape");
        assert!(hush.read_with(cx, |app, _| app.settings.panel.is_none()));
    }

    #[gpui::test]
    fn choices_apply_at_once_and_are_kept_for_the_next_launch(cx: &mut TestAppContext) {
        let (window, hush) = open(Demo::Chat, cx);
        let folder = std::env::temp_dir().join(format!("hush-desktop-settings-choices-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join(NotificationContent::FILE), "count\n").unwrap();
        std::fs::write(folder.join(Appearance::FILE), "dark").unwrap();
        let home = Home {
            folder: folder.clone(),
            key: refused,
            forget_key: keep_key,
        };
        cx.update(|cx| cx.set_global(home));
        act(window, &hush, cx, |_, window, cx| super::load(window, cx));
        let content = cx.update(|cx| chosen::<NotificationContent>(cx));
        assert_eq!(content, NotificationContent::NoNameOrMessage, "the Mac app's words");
        assert_eq!(cx.update(|cx| Theme::global(cx).mode), ThemeMode::Dark);

        act(window, &hush, cx, |app, window, cx| {
            app.open_settings(window, cx);
            app.choose_appearance(Appearance::Light, window, cx);
            app.choose(NotificationContent::Off, cx)
        });
        assert_eq!(cx.update(|cx| Theme::global(cx).mode), ThemeMode::Light);
        let content = cx.update(|cx| chosen::<NotificationContent>(cx));
        assert_eq!(content, NotificationContent::Off, "what notifications show from now on");
        let kept = |file: &str| std::fs::read_to_string(folder.join(file)).unwrap();
        assert_eq!(
            (kept(Appearance::FILE), kept(NotificationContent::FILE)),
            ("light".into(), "off".into())
        );
        assert!(hush.read_with(cx, |app, _| app.settings.panel.as_ref().unwrap().problem.is_none()));
        std::fs::remove_dir_all(&folder).unwrap();
    }
}
