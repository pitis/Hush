//! Settings (the Mac app's `SettingsView` and `ProfileSettings.swift`): the appearance, our name
//! and about, chat settings sync to the phone, and relinking or unlinking this computer; and the
//! unlinked screen's Link Again.

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
    /// Why the last relink, unlink, sync change or appearance change failed.
    problem: Option<String>,
}

/// Our name and about, trimmed (the core's `OwnProfile`).
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

/// The look the app takes: the system's, or always light or dark (the Mac app's `Appearance`),
/// kept in the data folder.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

impl Global for Appearance {}

/// The file in the data folder that keeps it (the core's unlink leaves it).
const APPEARANCE: &str = "appearance";

impl Appearance {
    const ALL: [(Self, &'static str); 3] = [(Self::System, "System"), (Self::Light, "Light"), (Self::Dark, "Dark")];

    fn read(folder: &Path) -> Self {
        match std::fs::read_to_string(folder.join(APPEARANCE))
            .as_deref()
            .map(str::trim)
        {
            Ok("light") => Self::Light,
            Ok("dark") => Self::Dark,
            _ => Self::System,
        }
    }

    fn save(self, folder: &Path) -> std::io::Result<()> {
        let word = match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        };
        std::fs::create_dir_all(folder)?;
        std::fs::write(folder.join(APPEARANCE), word)
    }

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

/// The appearance kept, applied now, and the system's followed while it's chosen.
pub(super) fn follow_appearance(window: &mut Window, cx: &mut Context<Hush>) {
    let chosen = cx
        .try_global::<Home>()
        .map_or(Appearance::System, |home| Appearance::read(&home.folder));
    cx.set_global(chosen);
    chosen.apply(window, cx);
    cx.observe_window_appearance(window, |_, window, cx| {
        if *cx.global::<Appearance>() == Appearance::System {
            Theme::sync_system_appearance(Some(window), cx)
        }
    })
    .detach();
}

impl Hush {
    /// Settings…: the panel over the window; our profile and the sync switch are read each time it
    /// opens (the phone changes them too).
    pub(super) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.panel.is_some() {
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
            problem: None,
        });
        if matches!(self.screen, Screen::Chats) {
            self.read_profile(window, cx);
            self.read_sync(cx);
        }
        cx.notify();
    }

    /// Done or Esc: back to the open chat's composer. A save or an unlink on its way still lands.
    pub(super) fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.panel.take().is_some() {
            if self.selected.is_some() {
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
                let Some(p) = &mut app.settings.panel else { return };
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
                    Err(e) => p.save_failure = Some(server_failure("Couldn't save your profile", &e)),
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
    pub(super) fn choose_appearance(&mut self, chosen: Appearance, window: &mut Window, cx: &mut Context<Self>) {
        cx.set_global(chosen);
        chosen.apply(window, cx);
        let folder = cx.try_global::<Home>().map(|home| home.folder.clone());
        if let Some(Err(e)) = folder.map(|folder| chosen.save(&folder)) {
            self.settings_failed(format!("Couldn't keep the appearance: {e}"), cx);
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

    /// This computer relinks as a new device; everything shown here is kept and the phone's
    /// history merges in (`AppStore.relinkForHistory`).
    fn relink(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        if std::mem::replace(&mut self.settings.working, true) {
            return;
        }
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let relinked = Compat::new(client.relink()).await;
            let _ = this.update_in(cx, |app, window, cx| match relinked {
                // the data stays, so its key does too
                Ok(()) => app.restart(false, window, cx),
                Err(e) => {
                    tracing::error!(error = %e, "relinking failed");
                    app.settings.working = false;
                    app.settings_failed(describe(&e), cx);
                }
            });
        })
        .detach();
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

    /// Starts over with a fresh core, all that showed forgotten (`AppStore.restart`); the key goes
    /// with the data it opened, unless relinking kept that.
    fn restart(&mut self, forget_key: bool, window: &mut Window, cx: &mut Context<Self>) {
        // the old core is stopped: quitting has nothing of it to wait for
        if cx.has_global::<Core>() {
            cx.remove_global::<Core>();
        }
        *self = Self::window(window, cx);
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
    use super::Appearance;

    #[test]
    fn the_appearance_chosen_is_kept_and_read_back() {
        let folder = std::env::temp_dir().join(format!("hush-desktop-appearance-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        assert_eq!(Appearance::read(&folder), Appearance::System, "none kept yet");
        for chosen in [Appearance::Dark, Appearance::Light, Appearance::System] {
            chosen.save(&folder).unwrap();
            assert_eq!(Appearance::read(&folder), chosen);
        }
        std::fs::write(folder.join(super::APPEARANCE), "sepia").unwrap();
        assert_eq!(Appearance::read(&folder), Appearance::System, "one it doesn't know");
        std::fs::remove_dir_all(&folder).unwrap();
    }
}

/// Settings in a real (headless) window, over a core that isn't linked, in a folder of its own.
#[cfg(all(test, feature = "snapshot"))]
mod window_tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use gpui_kit::component::{Theme, ThemeMode};
    use gpui_kit::{gpui, AnyWindowHandle, AppContext as _, Context, Entity, TestAppContext, Window, WindowOptions};
    use sig_core::CoreError;

    use super::super::attach::window_tests::with_core;
    use super::super::{Home, Hush, Screen};
    use super::{Appearance, Profile};
    use crate::Demo;

    /// A test's key is refused: the app doesn't open a core again (it would start linking).
    const REFUSED: &str = "not opened again in a test";

    fn refused() -> Result<String, String> {
        Err(REFUSED.into())
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
        for _ in 0..300 {
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

    /// Everything shown was forgotten: the made-up chats, the open one, the panel.
    fn forgotten(hush: &Entity<Hush>, cx: &mut TestAppContext) -> bool {
        hush.read_with(cx, |app, _| {
            app.chats.is_empty() && app.selected.is_none() && app.settings.panel.is_none() && app.client.is_none()
        })
    }

    fn exists(core: &Path, file: &str) -> bool {
        core.join(file).exists()
    }

    fn keep_key() -> Result<(), String> {
        panic!("relinking keeps the key")
    }

    #[gpui::test]
    fn unlinking_asks_then_removes_the_data_and_the_key_and_starts_over(cx: &mut TestAppContext) {
        static FORGOT: AtomicBool = AtomicBool::new(false);
        fn forget() -> Result<(), String> {
            FORGOT.store(true, Ordering::SeqCst);
            Ok(())
        }
        let (window, hush, data) = open_with_core("unlink", Demo::Chat, forget, cx);
        let core = data.join("core");
        let open_copy = core.join("open").join("1").join("menu.txt");
        std::fs::create_dir_all(open_copy.parent().unwrap()).unwrap();
        std::fs::write(&open_copy, "soup").unwrap();
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
        for gone in ["projection.db", "presage.db", "attachments"] {
            assert!(!exists(&core, gone), "{gone} is removed");
        }
        assert!(!open_copy.exists(), "an opened file's copy goes with the data");
        assert!(FORGOT.load(Ordering::SeqCst), "the key goes with the data");
        assert!(forgotten(&hush, cx));
        assert_eq!(
            hush.read_with(cx, |app, _| failed(app).map(str::to_owned)).as_deref(),
            Some(REFUSED),
            "opened again"
        );
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[gpui::test]
    fn an_unreachable_server_offers_to_unlink_anyway(cx: &mut TestAppContext) {
        let (window, hush, data) = open_with_core("unreachable", Demo::Chat, keep_key, cx);
        act(window, &hush, cx, |app, window, cx| {
            let e = CoreError::Network {
                message: "timed out".into(),
            };
            app.unlinked(true, Err(e), window, cx)
        });
        let (question, detail) = cx.pending_prompt().expect("asked");
        assert_eq!(question, "The server couldn't be reached");
        assert!(
            detail.starts_with("The server couldn't be reached: timed out\n\nUnlinking anyway"),
            "{detail}"
        );
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(exists(&data.join("core"), "projection.db"), "nothing removed");
        assert!(hush.read_with(cx, |app, _| matches!(app.screen, Screen::Chats)
            && !app.chats.is_empty()));
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[gpui::test]
    fn relinking_asks_then_keeps_the_chats_and_the_key_and_starts_over(cx: &mut TestAppContext) {
        let (window, hush, data) = open_with_core("relink", Demo::Chat, keep_key, cx);
        let core = data.join("core");
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
        assert!(forgotten(&hush, cx));
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[gpui::test]
    fn link_again_removes_the_data_without_asking_and_starts_over(cx: &mut TestAppContext) {
        static FORGOT: AtomicBool = AtomicBool::new(false);
        fn forget() -> Result<(), String> {
            FORGOT.store(true, Ordering::SeqCst);
            Ok(())
        }
        let (window, hush, data) = open_with_core("link-again", Demo::Unlinked, forget, cx);
        act(window, &hush, cx, |app, window, cx| app.unlink(false, window, cx));
        assert!(!cx.has_pending_prompt());
        until(&hush, cx, |app| failed(app).is_some());
        assert!(!exists(&data.join("core"), "projection.db"));
        assert!(FORGOT.load(Ordering::SeqCst));
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
        let (shown, _) = typed(&hush, cx);
        assert_eq!(shown.given, "Anna");
        let saved = hush.read_with(cx, |app, _| app.settings.panel.as_ref().unwrap().saved.clone());
        assert_eq!(saved, Some(ana("Climbing")));

        // a read begun before a save is stale
        act(window, &hush, cx, |app, _, _| {
            app.settings.panel.as_mut().unwrap().generation = 1
        });
        act(window, &hush, cx, |app, window, cx| {
            app.profile_read(0, Ok(ana("Old")), window, cx)
        });
        let saved = hush.read_with(cx, |app, _| app.settings.panel.as_ref().unwrap().saved.clone());
        assert_eq!(saved, Some(ana("Climbing")));

        // a first name is needed
        act(window, &hush, cx, move |_, window, cx| {
            given.update(cx, |f, cx| f.set_value("  ", window, cx))
        });
        assert!(!typed(&hush, cx).1);
        std::fs::remove_dir_all(&data).unwrap();
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
    fn the_appearance_chosen_applies_at_once_and_esc_closes_the_panel(cx: &mut TestAppContext) {
        let (window, hush) = open(Demo::Chat, cx);
        cx.simulate_keystrokes(window, "secondary-,");
        assert!(
            hush.read_with(cx, |app, _| app.settings.panel.is_some()),
            "the shortcut opens it"
        );
        act(window, &hush, cx, |app, window, cx| {
            app.choose_appearance(Appearance::Dark, window, cx)
        });
        assert_eq!(cx.update(|cx| Theme::global(cx).mode), ThemeMode::Dark);
        act(window, &hush, cx, |app, window, cx| {
            app.choose_appearance(Appearance::Light, window, cx)
        });
        assert_eq!(cx.update(|cx| Theme::global(cx).mode), ThemeMode::Light);
        // no data folder here: nothing was kept, nor failed to be
        assert!(hush.read_with(cx, |app, _| app.settings.panel.as_ref().unwrap().problem.is_none()));
        cx.simulate_keystrokes(window, "escape");
        assert!(hush.read_with(cx, |app, _| app.settings.panel.is_none()));
    }
}
