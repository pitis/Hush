//! Back Up… and Restore from a Backup… (the Mac app's `BackupViews.swift`, `AppStore.backUp`
//! and `restoreBackup`): every chat and file here in one file, encrypted with a passphrase, put
//! back before linking. The core writes and reads the file (`Client::back_up`, `restore_backup`),
//! so a backup made by either app restores in the other. A passphrase is read from its field only
//! for the call, and the field is cleared then.

use std::path::PathBuf;

use async_compat::Compat;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::*;
use sig_core::CoreError;

use super::{describe, Hush};

mod view;

/// Backups' extension: the Mac app's pickers show no other files (`BackupFile.type`).
const EXTENSION: &str = "hushbackup";
/// The shortest passphrase the core takes.
const MIN_PASSPHRASE: usize = 8;

#[derive(Default)]
pub(super) struct Backup {
    dialog: Option<Dialog>,
    /// How the last restore went, on the link screen (`AppStore.restoreNote`).
    note: Option<String>,
}

impl Backup {
    /// Its dialog is over the window.
    pub(super) fn showing(&self) -> bool {
        self.dialog.is_some()
    }
}

/// Back Up's or Restore's dialog (the Mac app's sheets).
struct Dialog {
    kind: Kind,
    passphrase: Entity<InputState>,
    /// Tab and Shift-Tab stay inside it.
    focus: FocusHandle,
    /// The core is at it: the dialog stays until it's done.
    working: bool,
    failure: Option<String>,
}

enum Kind {
    /// Its second field, and the file's name once written.
    BackUp {
        again: Entity<InputState>,
        saved: Option<String>,
    },
    /// The backup chosen.
    Restore(PathBuf),
}

/// The Save dialog's suggestion (`BackupFile.defaultName`).
fn default_name(today: chrono::NaiveDate) -> String {
    format!("Hush Backup {}.{EXTENSION}", today.format("%Y-%m-%d"))
}

/// `path`, ending in the backups' extension (the Save dialog here doesn't add it).
fn with_extension(path: PathBuf) -> PathBuf {
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case(EXTENSION)) {
        return path;
    }
    let mut named = path.into_os_string();
    named.push(format!(".{EXTENSION}"));
    named.into()
}

/// Back Up… can go ahead: long enough, and typed the same twice (`BackupSheet.acceptable`).
fn acceptable(passphrase: &str, again: &str) -> bool {
    passphrase.chars().count() >= MIN_PASSPHRASE && again == passphrase
}

/// A backup's file failure as it is ("disk full"), not as one to open Hush's data
/// (`AppStore.fileFailure`).
fn file_failure(e: &CoreError) -> String {
    match e {
        CoreError::Storage { message } => message.clone(),
        e => describe(e),
    }
}

impl Hush {
    /// Settings ▸ Backup ▸ Back Up…: the passphrase twice, then where.
    pub(super) fn open_backup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.client.is_none() || self.backup.dialog.as_ref().is_some_and(|d| d.working) {
            return;
        }
        let passphrase = self.passphrase_field("Passphrase (at least 8 characters)", window, cx);
        let again = self.passphrase_field("Passphrase again", window, cx);
        self.show_backup(Kind::BackUp { again, saved: None }, passphrase, window, cx);
    }

    /// The link screen's Restore from a Backup…: picks the file, then asks for its passphrase.
    pub(super) fn choose_restore(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Restore".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let file = match chosen.await {
                Ok(Ok(Some(paths))) => paths
                    .into_iter()
                    .next()
                    .ok_or_else(|| "That file isn't on this computer's disk.".to_string()),
                Ok(Err(e)) => Err(format!("Couldn't open the file picker: {e}")),
                // cancelled
                Ok(Ok(None)) | Err(_) => return,
            };
            let _ = this.update_in(cx, |app, window, cx| match file {
                Ok(file) => {
                    let passphrase = app.passphrase_field("Passphrase", window, cx);
                    app.show_backup(Kind::Restore(file), passphrase, window, cx);
                }
                Err(why) => {
                    app.backup.note = Some(why);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn passphrase_field(
        &mut self,
        placeholder: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        let field = cx.new(|cx| InputState::new(window, cx).masked(true).placeholder(placeholder));
        cx.subscribe_in(&field, window, |this, _, event: &InputEvent, window, cx| match event {
            InputEvent::PressEnter { .. } => this.submit_backup(window, cx),
            InputEvent::Change => cx.notify(),
            _ => {}
        })
        .detach();
        field
    }

    fn show_backup(&mut self, kind: Kind, passphrase: Entity<InputState>, window: &mut Window, cx: &mut Context<Self>) {
        passphrase.update(cx, |f, cx| f.focus(window, cx));
        self.backup.dialog = Some(Dialog {
            kind,
            passphrase,
            focus: cx.focus_handle(),
            working: false,
            failure: None,
        });
        cx.notify();
    }

    /// Cancel or Done: not while the core is at it.
    fn close_backup(&mut self, cx: &mut Context<Self>) {
        if self.backup.dialog.as_ref().is_some_and(|d| !d.working) {
            self.backup.dialog = None;
            cx.notify();
        }
    }

    /// The dialog's default button: Back Up… (asks where), Done, or Restore.
    fn submit_backup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = &self.backup.dialog else { return };
        if d.working {
            return;
        }
        match &d.kind {
            Kind::BackUp { saved: Some(_), .. } => self.close_backup(cx),
            Kind::BackUp { again, saved: None } => {
                if acceptable(&d.passphrase.read(cx).value(), &again.read(cx).value()) {
                    self.choose_backup_file(window, cx);
                }
            }
            Kind::Restore(_) => self.restore(window, cx),
        }
    }

    fn choose_backup_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let folder = dirs::document_dir().or_else(dirs::home_dir).unwrap_or_default();
        let name = default_name(chrono::Local::now().date_naive());
        let chosen = cx.prompt_for_new_path(&folder, Some(&name));
        cx.spawn_in(window, async move |this, cx| {
            let file = match chosen.await {
                Ok(Ok(Some(file))) => Ok(with_extension(file)),
                Ok(Err(e)) => Err(format!("Couldn't open the file picker: {e}")),
                // cancelled
                Ok(Ok(None)) | Err(_) => return,
            };
            let _ = this.update_in(cx, |app, window, cx| match file {
                Ok(file) => app.back_up(file, window, cx),
                Err(why) => {
                    if let Some(d) = &mut app.backup.dialog {
                        d.failure = Some(why);
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Writes the backup to `file` (replaced), encrypted with the passphrase typed.
    fn back_up(&mut self, file: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        // closed while the Save dialog was up, or the fields changed since
        let Some(d) = &mut self.backup.dialog else { return };
        let Kind::BackUp { again, saved: None } = &d.kind else {
            return;
        };
        let passphrase = d.passphrase.read(cx).value().to_string();
        if d.working || !acceptable(&passphrase, &again.read(cx).value()) {
            return;
        }
        let Some(at) = file.to_str().map(str::to_owned) else {
            d.failure = Some("Couldn't back up: the file's path isn't valid text".into());
            return cx.notify();
        };
        for field in [&d.passphrase, again] {
            field.update(cx, |f, cx| f.set_value("", window, cx));
        }
        d.working = true;
        d.failure = None;
        cx.notify();
        let name = file.file_name().unwrap_or_default().to_string_lossy().into_owned();
        cx.spawn(async move |this, cx| {
            let written = Compat::new(client.back_up(at, passphrase)).await;
            let _ = this.update(cx, |app, cx| {
                let Some(d) = &mut app.backup.dialog else { return };
                d.working = false;
                match written {
                    Ok(()) => {
                        if let Kind::BackUp { saved, .. } = &mut d.kind {
                            *saved = Some(name);
                        }
                    }
                    Err(e) => d.failure = Some(format!("Couldn't back up: {}", file_failure(&e))),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The chosen backup replaces everything here, then a new linking code follows. A wrong
    /// passphrase, a damaged file or one that can't be read is said in the dialog, and the code
    /// stays; otherwise the core has stopped and starts over (finishing a swap cut short), and the
    /// link screen says how it went.
    fn restore(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        let Some(d) = &mut self.backup.dialog else { return };
        let Kind::Restore(file) = &d.kind else { return };
        let passphrase = d.passphrase.read(cx).value().to_string();
        if d.working || passphrase.is_empty() {
            return;
        }
        let Some(at) = file.to_str().map(str::to_owned) else {
            d.failure = Some("the file's path isn't valid text".into());
            return cx.notify();
        };
        d.passphrase.update(cx, |f, cx| f.set_value("", window, cx));
        d.working = true;
        d.failure = None;
        self.backup.note = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let restored = Compat::new(client.restore_backup(at, passphrase)).await;
            let _ = this.update_in(cx, |app, window, cx| app.restored(restored, window, cx));
        })
        .detach();
    }

    fn restored(&mut self, restored: Result<(), CoreError>, window: &mut Window, cx: &mut Context<Self>) {
        let note = match restored {
            Ok(()) => "Your backup is restored. Link this computer to your phone to finish.".to_string(),
            // nothing changed, and the core runs on
            Err(CoreError::InvalidInput { message }) => {
                if let Some(d) = &mut self.backup.dialog {
                    d.working = false;
                    d.failure = Some(message);
                }
                return cx.notify();
            }
            Err(e) => format!("Couldn't restore the backup: {}", file_failure(&e)),
        };
        // the data stays, so its key does too
        self.restart(false, window, cx);
        // the new code doesn't hide it
        self.backup.note = Some(note);
    }
}

#[cfg(feature = "snapshot")]
impl Hush {
    /// `--snapshot`'s Back Up dialog over Settings, or the link screen after a restore.
    pub(super) fn demo_backup(&mut self, restored: bool, window: &mut Window, cx: &mut Context<Self>) {
        if restored {
            self.backup.note = Some("Your backup is restored. Link this computer to your phone to finish.".into());
            return;
        }
        self.demo_settings(false, window, cx);
        let passphrase = self.passphrase_field("Passphrase (at least 8 characters)", window, cx);
        let again = self.passphrase_field("Passphrase again", window, cx);
        passphrase.update(cx, |f, cx| f.set_value("correct horse", window, cx));
        self.show_backup(Kind::BackUp { again, saved: None }, passphrase, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{acceptable, default_name, with_extension};

    #[test]
    fn a_backup_is_named_and_ends_as_the_mac_app_names_it() {
        let day = chrono::NaiveDate::from_ymd_opt(2026, 3, 7).unwrap();
        assert_eq!(default_name(day), "Hush Backup 2026-03-07.hushbackup");
        let named = |path: &str| with_extension(PathBuf::from(path));
        assert_eq!(named("/b/Hush Backup"), PathBuf::from("/b/Hush Backup.hushbackup"));
        assert_eq!(named("/b/x.hushbackup"), PathBuf::from("/b/x.hushbackup"));
        assert_eq!(named("/b/x.HushBackup"), PathBuf::from("/b/x.HushBackup"));
        assert_eq!(named("/b/x.zip"), PathBuf::from("/b/x.zip.hushbackup"), "not replaced");
    }

    #[test]
    fn a_passphrase_needs_eight_characters_typed_the_same_twice() {
        assert!(!acceptable("1234567", "1234567"));
        assert!(acceptable("12345678", "12345678"));
        assert!(!acceptable("12345678", "12345679"));
        assert!(acceptable("pâté brûlé", "pâté brûlé"), "characters, not bytes");
        assert!(!acceptable("ăîșțâ", "ăîșțâ"));
    }
}

/// Backing up and restoring in a real (headless) window, over real cores that aren't linked, in
/// temporary folders: `cargo test --features hush-desktop/snapshot`. The app's own data folder and
/// keychain are never touched: the test's `Home` refuses the key, so a restore's new core never
/// opens (it would start linking), and never forgets it.
#[cfg(all(test, feature = "snapshot"))]
mod window_tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use async_compat::Compat;
    use gpui_kit::{gpui, AnyWindowHandle, AppContext as _, Context, Entity, TestAppContext, Window, WindowOptions};
    use sig_core::CoreError;

    use super::super::attach::window_tests::with_core;
    use super::super::{Home, Hush, Screen};
    use super::Kind;
    use crate::Demo;

    const REFUSED: &str = "not opened again in a test";
    const RESTORED: &str = "Your backup is restored. Link this computer to your phone to finish.";

    fn refused() -> Result<String, String> {
        Err(REFUSED.into())
    }

    fn keep_key() -> Result<(), String> {
        panic!("a restore keeps the key")
    }

    fn folder(name: &str) -> PathBuf {
        let data = std::env::temp_dir().join(format!("hush-desktop-backup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&data).unwrap();
        data
    }

    /// A window showing `screen`, over a core in `data` (gpui_kit is set up already).
    fn open(screen: Demo, data: &Path, cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Hush>) {
        let (window, hush) = cx.update(|cx| {
            let (window, hush) = gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Hush::demo(screen, window, cx))
            })
            .expect("the window opens");
            (window, hush)
        });
        with_core(&hush, data, cx);
        cx.run_until_parked();
        (window, hush)
    }

    /// The app's data folder is `data`'s core, which never opens again.
    fn home(data: &Path, cx: &mut TestAppContext) {
        let folder = data.join("core");
        cx.update(|cx| {
            cx.set_global(Home {
                folder,
                key: refused,
                forget_key: keep_key,
            })
        });
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

    /// Types `first` (and `again`, in Back Up's second field), then presses the default button.
    fn submit(window: AnyWindowHandle, hush: &Entity<Hush>, first: &str, again: &str, cx: &mut TestAppContext) {
        act(window, hush, cx, |app, window, cx| {
            let d = app.backup.dialog.as_ref().expect("the dialog is open");
            d.passphrase
                .update(cx, |f, cx| f.set_value(first.to_string(), window, cx));
            if let Kind::BackUp { again: field, .. } = &d.kind {
                field.update(cx, |f, cx| f.set_value(again.to_string(), window, cx));
            }
            app.submit_backup(window, cx);
        });
    }

    /// Waits for the core's answer (on its own threads) until `done`.
    fn until(hush: &Entity<Hush>, cx: &mut TestAppContext, done: impl Fn(&Hush) -> bool) {
        for _ in 0..500 {
            cx.run_until_parked();
            if hush.read_with(cx, |app, _| done(app)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the core didn't answer");
    }

    fn working(app: &Hush) -> bool {
        app.backup.dialog.as_ref().is_some_and(|d| d.working)
    }

    fn failure(hush: &Entity<Hush>, cx: &mut TestAppContext) -> Option<String> {
        hush.read_with(cx, |app, _| app.backup.dialog.as_ref().and_then(|d| d.failure.clone()))
    }

    /// Every field of the open dialog is empty.
    fn cleared(hush: &Entity<Hush>, cx: &mut TestAppContext) -> bool {
        hush.read_with(cx, |app, cx| {
            let d = app.backup.dialog.as_ref().expect("the dialog is open");
            let again = match &d.kind {
                Kind::BackUp { again, .. } => again.read(cx).value().is_empty(),
                Kind::Restore(_) => true,
            };
            d.passphrase.read(cx).value().is_empty() && again
        })
    }

    #[gpui::test]
    fn a_backup_made_here_restores_into_a_fresh_folder(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (from, to) = (folder("from"), folder("to"));
        let (window, hush) = open(Demo::Chat, &from, cx);
        home(&to, cx);
        // the core answers from its own threads
        cx.executor().allow_parking();
        let client = hush.read_with(cx, |app, _| app.client.clone().unwrap());
        let marker = || ("c:someone".to_string(), "kept in the backup".to_string());
        let (chat, text) = marker();
        futures::executor::block_on(Compat::new(client.set_draft(chat, text))).unwrap();

        // Settings ▸ Back Up…: the passphrase twice, then where
        act(window, &hush, cx, |app, window, cx| app.open_backup(window, cx));
        submit(window, &hush, "correct horse", "correct horsy", cx);
        assert!(!cx.did_prompt_for_new_path(), "typed differently: nothing asked");
        submit(window, &hush, "short", "short", cx);
        assert!(!cx.did_prompt_for_new_path(), "too short: nothing asked");
        submit(window, &hush, "correct horse", "correct horse", cx);
        assert!(cx.did_prompt_for_new_path());
        // named without the extension: the Mac app's picker shows only `.hushbackup` files
        cx.simulate_new_path_selection(|_| Some(from.join("Hush Backup")));
        until(&hush, cx, |app| !working(app));
        assert!(cleared(&hush, cx), "the passphrase isn't kept");
        assert_eq!(failure(&hush, cx), None);
        let saved = hush.read_with(cx, |app, _| match &app.backup.dialog.as_ref().unwrap().kind {
            Kind::BackUp { saved, .. } => saved.clone(),
            Kind::Restore(_) => None,
        });
        assert_eq!(saved.as_deref(), Some("Hush Backup.hushbackup"));
        let file = from.join("Hush Backup.hushbackup");
        assert!(file.exists());
        // Done
        act(window, &hush, cx, |app, window, cx| app.submit_backup(window, cx));
        assert!(hush.read_with(cx, |app, _| app.backup.dialog.is_none()));

        // another computer, not linked: Restore from a Backup… on the link screen
        let (window, hush) = open(Demo::Link, &to, cx);
        act(window, &hush, cx, |app, window, cx| app.choose_restore(window, cx));
        assert!(cx.did_prompt_for_paths());
        let picked = file.clone();
        cx.simulate_path_prompt_response(move |options| {
            assert!(options.files && !options.directories && !options.multiple);
            Some(vec![picked])
        });
        cx.run_until_parked();
        let chosen = hush.read_with(cx, |app, _| match &app.backup.dialog.as_ref().unwrap().kind {
            Kind::Restore(file) => Some(file.clone()),
            Kind::BackUp { .. } => None,
        });
        assert_eq!(chosen, Some(file.clone()));

        // a wrong passphrase is said, and nothing changed: the core runs on
        submit(window, &hush, "battery staple", "", cx);
        until(&hush, cx, |app| !working(app));
        assert_eq!(
            failure(&hush, cx).as_deref(),
            Some("the passphrase is wrong, or the backup is damaged")
        );
        assert!(cleared(&hush, cx), "the passphrase isn't kept");
        hush.read_with(cx, |app, _| {
            assert!(app.client.is_some() && matches!(app.screen, Screen::Link { .. }));
            assert_eq!(app.backup.note, None);
        });

        // the right one: restored, and the app starts over over the same folder (whose key the
        // test refuses), saying so
        submit(window, &hush, "correct horse", "", cx);
        until(&hush, cx, |app| app.backup.note.is_some());
        hush.read_with(cx, |app, _| {
            assert_eq!(app.backup.note.as_deref(), Some(RESTORED));
            assert!(app.backup.dialog.is_none());
            assert!(
                matches!(&app.screen, Screen::Failed(why) if why == REFUSED),
                "a new core was asked for"
            );
        });

        // the chats are there for the next core, under this computer's key
        struct Quiet;
        impl sig_core::EventListener for Quiet {
            fn on_event(&self, _: sig_core::CoreEvent) {}
        }
        let dir = to.join("core").to_str().unwrap().to_owned();
        let reopened = sig_core::Client::open(dir, "ab".repeat(32), Arc::new(Quiet)).unwrap();
        // never dropped (as `with_core`'s): a database closing as the test binary exits crashes it
        std::mem::forget(reopened.clone());
        let drafts = futures::executor::block_on(Compat::new(reopened.drafts())).unwrap();
        assert_eq!(drafts.into_iter().collect::<Vec<_>>(), [marker()]);
        std::fs::remove_dir_all(&from).unwrap();
        std::fs::remove_dir_all(&to).unwrap();
    }

    #[gpui::test]
    fn a_restore_that_failed_once_the_core_stopped_starts_over_and_says_why(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let data = folder("failed");
        let (window, hush) = open(Demo::Link, &data, cx);
        home(&data, cx);
        act(window, &hush, cx, |app, window, cx| {
            let failed = CoreError::Storage {
                message: "disk full".into(),
            };
            app.restored(Err(failed), window, cx)
        });
        hush.read_with(cx, |app, _| {
            assert_eq!(
                app.backup.note.as_deref(),
                Some("Couldn't restore the backup: disk full")
            );
            assert!(
                matches!(&app.screen, Screen::Failed(why) if why == REFUSED),
                "started over"
            );
        });
        std::fs::remove_dir_all(&data).unwrap();
    }
}
