//! Opening Hush at login (the Mac app's `LoginItem`), with this executable where it is now.
//! SMAppService registers only an app bundle and this app runs unbundled (`cargo run`), so macOS
//! gets a LaunchAgent in ~/Library/LaunchAgents (System Settings ▸ General ▸ Login Items lists it
//! under Allow in the Background); Windows a value in the user's Run key; Linux an XDG autostart
//! entry.

#[cfg(not(windows))]
use std::path::{Path, PathBuf};

#[cfg(not(windows))]
use super::notifications::APP_ID;

/// This executable's path.
fn exe() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    exe.into_os_string()
        .into_string()
        .map_err(|_| "the app's path isn't valid text".into())
}

#[cfg(not(windows))]
pub(super) fn enabled() -> bool {
    entry().is_some_and(|path| path.exists())
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
    let contents = if cfg!(target_os = "macos") {
        launch_agent(exe)
    } else {
        autostart(exe)
    };
    std::fs::write(path, contents).map_err(|e| e.to_string())
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
/// The Run key's value, by the name Windows shows in Settings ▸ Apps ▸ Startup.
#[cfg(windows)]
const NAME: &str = "Hush Desktop";

#[cfg(windows)]
pub(super) fn enabled() -> bool {
    let key = windows_registry::CURRENT_USER.open(RUN);
    key.and_then(|key| key.get_string(NAME)).is_ok()
}

#[cfg(windows)]
pub(super) fn set(on: bool) -> Result<(), String> {
    let key = windows_registry::CURRENT_USER.create(RUN).map_err(|e| e.to_string())?;
    let done = match on {
        true => key.set_string(NAME, format!("\"{}\"", exe()?)),
        // removing a value that isn't there fails
        false if !enabled() => return Ok(()),
        false => key.remove_value(NAME),
    };
    done.map_err(|e| e.to_string())
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::{autostart, launch_agent, write};

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
        write(&path, Some("/apps/hush")).unwrap();
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
