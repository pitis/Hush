//! Opening Hush at login (the Mac app's `LoginItem`), with this executable where it is now.
//! SMAppService registers only an app bundle and this app runs unbundled (`cargo run`), so macOS
//! gets a LaunchAgent in ~/Library/LaunchAgents (System Settings ▸ General ▸ Login Items &
//! Extensions lists it under Allow in the Background); Windows a value in the user's Run key; Linux
//! an XDG autostart entry. It is on when Hush's entry is there and opens this executable.

#[cfg(not(windows))]
use std::path::{Path, PathBuf};

#[cfg(not(windows))]
use super::notifications::APP_ID;

/// This executable's path, while it is there (an entry for one moved or rebuilt opens nothing).
fn exe() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    if !exe.exists() {
        return Err("this copy of Hush was moved or rebuilt since it opened: open it again first".into());
    }
    exe.into_os_string()
        .into_string()
        .map_err(|_| "the app's path isn't valid text".into())
}

/// shortcut: an entry switched off in System Settings' Allow in the Background still reads as on;
/// ask `SMAppService.statusForLegacyURL` if that matters.
#[cfg(not(windows))]
pub(super) fn enabled() -> bool {
    let (Some(path), Ok(exe)) = (entry(), exe()) else {
        return false;
    };
    opens(&path, &exe)
}

#[cfg(not(windows))]
pub(super) fn set(on: bool) -> Result<(), String> {
    let path = entry().ok_or("this system has no folder for login items")?;
    write(&path, on.then(exe).transpose()?.as_deref())
}

/// Where the entry goes.
#[cfg(not(windows))]
fn entry() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        Some(
            dirs::home_dir()?
                .join("Library/LaunchAgents")
                .join(format!("{APP_ID}.plist")),
        )
    } else {
        Some(dirs::config_dir()?.join("autostart").join(format!("{APP_ID}.desktop")))
    }
}

/// Writes the entry at `path` that opens `exe`, or removes it (None).
#[cfg(not(windows))]
fn write(path: &Path, exe: Option<&str>) -> Result<(), String> {
    let Some(exe) = exe else {
        return match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        };
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, contents(exe)).map_err(|e| e.to_string())
}

/// The entry at `path` is the one that opens `exe` (not one for another copy, or edited).
#[cfg(not(windows))]
fn opens(path: &Path, exe: &str) -> bool {
    std::fs::read_to_string(path).is_ok_and(|kept| kept == contents(exe))
}

#[cfg(not(windows))]
fn contents(exe: &str) -> String {
    if cfg!(target_os = "macos") {
        launch_agent(exe)
    } else {
        autostart(exe)
    }
}

/// A LaunchAgent run once, at login.
#[cfg(not(windows))]
fn launch_agent(exe: &str) -> String {
    let exe = exe.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{APP_ID}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>ProcessType</key>
	<string>Interactive</string>
</dict>
</plist>
"#
    )
}

/// An XDG autostart entry, its `Exec` quoted as the Desktop Entry spec asks: in double quotes,
/// `"`, `` ` ``, `$` and `\` escaped, then each `\` again as a string value's; `%` doubled.
#[cfg(not(windows))]
fn autostart(exe: &str) -> String {
    let mut quoted = String::new();
    for c in exe.chars() {
        match c {
            '"' | '`' | '$' | '\\' => quoted.push('\\'),
            '%' => quoted.push('%'),
            _ => {}
        }
        quoted.push(c);
    }
    let exec = quoted.replace('\\', "\\\\");
    format!("[Desktop Entry]\nType=Application\nName=Hush\nExec=\"{exec}\"\n")
}

#[cfg(windows)]
const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Where Settings ▸ Apps ▸ Startup and Task Manager switch a Run value off (an odd first byte).
#[cfg(windows)]
const APPROVED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
/// The Run key's value.
#[cfg(windows)]
const NAME: &str = "Hush Desktop";

#[cfg(windows)]
pub(super) fn enabled() -> bool {
    use windows_registry::CURRENT_USER;
    let Ok(exe) = exe() else { return false };
    let kept = CURRENT_USER.open(RUN).and_then(|key| key.get_string(NAME));
    let off = CURRENT_USER.open(APPROVED).and_then(|key| key.get_value(NAME));
    kept.is_ok_and(|kept| kept == format!("\"{exe}\"")) && !off.is_ok_and(|v| v.first().is_some_and(|b| b & 1 == 1))
}

#[cfg(windows)]
pub(super) fn set(on: bool) -> Result<(), String> {
    use windows_registry::CURRENT_USER;
    let run = CURRENT_USER.create(RUN).map_err(|e| e.to_string())?;
    if !on {
        return gone(run.remove_value(NAME));
    }
    run.set_string(NAME, format!("\"{}\"", exe()?))
        .map_err(|e| e.to_string())?;
    // switched off in the system's settings before: on again
    match CURRENT_USER.options().read().write().open(APPROVED) {
        Ok(approved) => gone(approved.remove_value(NAME)),
        Err(e) => gone(Err(e)),
    }
}

/// Removed, or not there to remove (`ERROR_FILE_NOT_FOUND`).
#[cfg(windows)]
fn gone(removed: windows_registry::Result<()>) -> Result<(), String> {
    match removed {
        Err(e) if e.code().0 != 0x8007_0002_u32 as i32 => Err(e.to_string()),
        _ => Ok(()),
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::{autostart, launch_agent, opens, write};

    #[test]
    fn the_launch_agent_opens_the_app_at_login() {
        let plist = launch_agent("/Users/a/Hush & Co/<hush>");
        assert!(
            plist.contains("<string>/Users/a/Hush &amp; Co/&lt;hush&gt;</string>"),
            "{plist}"
        );
        assert!(plist.contains("<key>Label</key>\n\t<string>dev.pitis.HushDesktop</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n\t<true/>"));
    }

    #[test]
    fn the_autostart_entry_quotes_the_path_as_the_spec_asks() {
        let entry = autostart("/home/a b/hush");
        assert!(entry.starts_with("[Desktop Entry]\n"), "{entry}");
        assert!(entry.contains("\nExec=\"/home/a b/hush\"\n"), "{entry}");
        assert!(entry.contains("\nType=Application\n"));
        // `"`, `` ` ``, `$` and `\` escaped in the quotes, then every `\` again as a string's; `%%`
        let odd = autostart(r#"/x/"q"/`c`/$v/b\s/100%"#);
        assert!(odd.contains(r#"Exec="/x/\\"q\\"/\\`c\\`/\\$v/b\\\\s/100%%""#), "{odd}");
    }

    #[test]
    fn the_entry_is_written_and_removed() {
        let dir = std::env::temp_dir().join(format!("hush-desktop-login-{}", std::process::id()));
        let path = dir.join("agents").join("hush.plist");
        assert!(!opens(&path, "/apps/hush"));
        write(&path, Some("/apps/hush")).unwrap();
        assert!(opens(&path, "/apps/hush"));
        // another copy's entry, or one edited: not this one's
        assert!(!opens(&path, "/elsewhere/hush"));
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            written,
            if cfg!(target_os = "macos") {
                launch_agent("/apps/hush")
            } else {
                autostart("/apps/hush")
            }
        );
        write(&path, None).unwrap();
        assert!(!path.exists());
        // already gone: nothing to do
        write(&path, None).unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }
}
