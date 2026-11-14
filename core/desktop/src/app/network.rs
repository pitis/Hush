//! Reconnects as soon as the network comes back, or the computer wakes from sleep, rather than
//! after the core's backoff (the Mac app's `NetworkWatch`). The network: NWPathMonitor on macOS,
//! NetworkManager's state over D-Bus on Linux, the connectivity hint on Windows (10 2004 on).
//! Without one (Linux without NetworkManager, an older Windows) waking still reconnects at once,
//! and a network coming back waits for the core's backoff, as before.

use futures::channel::mpsc::{unbounded, UnboundedSender};
use futures::StreamExt;
use gpui_kit::*;

use super::{Hush, Screen};

/// What a watcher tells.
#[derive(Clone, Copy, Debug)]
enum Report {
    /// The network can be used, or not.
    Online(bool),
    /// The computer woke from sleep.
    Woke,
}

/// What a report leaves the network at (unknown until the first), and whether to reconnect now.
fn next(was: Option<bool>, report: Report) -> (Option<bool>, bool) {
    match report {
        // its first report says nothing came back
        Report::Online(now) => (Some(now), was == Some(false) && now),
        // the old connection can be dead though the network stayed; without one the attempt would
        // fail, and its return reconnects anyway
        Report::Woke => (was, was != Some(false)),
    }
}

/// Watches the network, and the system waking (GPUI's: NSWorkspace's, Windows' power broadcast,
/// logind's), for as long as the app runs.
pub fn watch(hush: WeakEntity<Hush>, cx: &mut App) {
    let (reports, mut reported) = unbounded();
    platform(reports.clone());
    cx.on_system_wake(move |_| {
        let _ = reports.unbounded_send(Report::Woke);
    })
    .detach();
    cx.spawn(async move |cx| {
        let mut was = None;
        while let Some(report) = reported.next().await {
            let reconnect;
            (was, reconnect) = next(was, report);
            if reconnect && hush.update(cx, |app, cx| app.network_returned(cx)).is_err() {
                break;
            }
        }
    })
    .detach();
}

impl Hush {
    /// Reconnects now (`AppStore.networkReturned`).
    fn network_returned(&mut self, cx: &mut Context<Self>) {
        if matches!(self.screen, Screen::Chats) {
            self.reconnect(cx);
        }
    }
}

/// NWPathMonitor, on a queue of its own; kept for the app's life.
#[cfg(target_os = "macos")]
fn platform(reports: UnboundedSender<Report>) {
    use std::ffi::{c_char, c_int, c_void};

    #[link(name = "Network", kind = "framework")]
    unsafe extern "C" {
        fn nw_path_monitor_create() -> *mut c_void;
        fn nw_path_monitor_set_update_handler(monitor: *mut c_void, handler: &block2::Block<dyn Fn(*mut c_void)>);
        fn nw_path_monitor_set_queue(monitor: *mut c_void, queue: *mut c_void);
        fn nw_path_monitor_start(monitor: *mut c_void);
        fn nw_path_get_status(path: *mut c_void) -> c_int;
    }
    unsafe extern "C" {
        fn dispatch_queue_create(label: *const c_char, attr: *mut c_void) -> *mut c_void;
    }
    /// `nw_path_status_satisfied`
    const SATISFIED: c_int = 1;

    let handler = block2::RcBlock::new(move |path: *mut c_void| {
        // SAFETY: the path the monitor reports, valid for the handler's call
        let online = unsafe { nw_path_get_status(path) } == SATISFIED;
        let _ = reports.unbounded_send(Report::Online(online));
    });
    // SAFETY: the monitor takes its own copy of the handler, and is started once with a queue
    unsafe {
        let monitor = nw_path_monitor_create();
        nw_path_monitor_set_update_handler(monitor, &handler);
        let queue = dispatch_queue_create(c"dev.pitis.hush.network".as_ptr(), std::ptr::null_mut());
        nw_path_monitor_set_queue(monitor, queue);
        nw_path_monitor_start(monitor);
    }
}

/// NetworkManager's state, on a thread of its own.
#[cfg(not(any(target_os = "macos", windows)))]
fn platform(reports: UnboundedSender<Report>) {
    /// From `NM_STATE_CONNECTED_SITE`: a network with a way out, as NWPathMonitor's satisfied path,
    /// whether or not NetworkManager's own check reached the internet (a proxy can block it).
    const ROUTED: u32 = 60;

    // Ok(true): the app is quitting
    let watch = move || -> zbus::Result<bool> {
        let bus = zbus::blocking::Connection::system()?;
        let nm = zbus::blocking::Proxy::new(
            &bus,
            "org.freedesktop.NetworkManager",
            "/org/freedesktop/NetworkManager",
            "org.freedesktop.NetworkManager",
        )?;
        // before the state is read: no change is missed in between
        let changes = nm.receive_signal("StateChanged")?;
        let state: u32 = nm.get_property("State")?;
        if reports.unbounded_send(Report::Online(state >= ROUTED)).is_err() {
            return Ok(true);
        }
        for change in changes {
            let state: u32 = match change.body().deserialize() {
                Ok(state) => state,
                Err(e) => {
                    tracing::warn!(error = %e, "a network state didn't read");
                    continue;
                }
            };
            if reports.unbounded_send(Report::Online(state >= ROUTED)).is_err() {
                return Ok(true);
            }
        }
        Ok(false)
    };
    let started = std::thread::Builder::new()
        .name("network-watch".into())
        .spawn(move || match watch() {
            Ok(true) => {}
            Ok(false) => tracing::warn!("the network watch stopped: the core reconnects on its own"),
            Err(e) => tracing::warn!(error = %e, "no network watch: the core reconnects on its own"),
        });
    if let Err(e) = started {
        tracing::warn!(error = %e, "no network watch: the core reconnects on its own");
    }
}

/// The system's connectivity hint, told on a thread of its pool; registered for the app's life.
#[cfg(windows)]
fn platform(reports: UnboundedSender<Report>) {
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::NetworkManagement::IpHelper::PNETWORK_CONNECTIVITY_HINT_CHANGE_CALLBACK;
    use windows_sys::Win32::Networking::WinSock::{
        NetworkConnectivityLevelHintHidden, NetworkConnectivityLevelHintNone, NetworkConnectivityLevelHintUnknown,
        NL_NETWORK_CONNECTIVITY_HINT,
    };
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};

    /// `NotifyNetworkConnectivityHintChange`, from Windows 10 2004 on: looked up rather than
    /// imported, so an older Windows still starts the app (GPUI runs from 1703).
    type Notify =
        unsafe extern "system" fn(PNETWORK_CONNECTIVITY_HINT_CHANGE_CALLBACK, *const c_void, bool, *mut HANDLE) -> u32;
    type Found = unsafe extern "system" fn() -> isize;

    unsafe extern "system" fn changed(context: *const c_void, hint: NL_NETWORK_CONNECTIVITY_HINT) {
        // SAFETY: the sender registered with the callback, never freed while it is
        let reports = unsafe { &*context.cast::<UnboundedSender<Report>>() };
        let level = hint.ConnectivityLevel;
        if level == NetworkConnectivityLevelHintUnknown || level == NetworkConnectivityLevelHintHidden {
            return;
        }
        // any network, as NWPathMonitor's satisfied path: Windows' own internet check can fail
        // behind a proxy
        let online = level != NetworkConnectivityLevelHintNone;
        let _ = reports.unbounded_send(Report::Online(online));
    }

    let dll: Vec<u16> = "iphlpapi.dll\0".encode_utf16().collect();
    // SAFETY: a system DLL by a NUL-terminated name, kept loaded for the app's life
    let module = unsafe { LoadLibraryExW(dll.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
    let found = match module.is_null() {
        true => None,
        // SAFETY: a NUL-terminated name, in a module loaded
        false => unsafe { GetProcAddress(module, c"NotifyNetworkConnectivityHintChange".as_ptr().cast()) },
    };
    let Some(found) = found else {
        return tracing::warn!("no network watch before Windows 10 2004: the core reconnects on its own");
    };
    // SAFETY: the function's documented signature
    let notify = unsafe { std::mem::transmute::<Found, Notify>(found) };
    let context = Box::into_raw(Box::new(reports));
    let mut handle = std::ptr::null_mut();
    // SAFETY: `context` stays valid for as long as the registration (the app's life)
    let failed = unsafe { notify(Some(changed), context.cast(), true, &mut handle) };
    if failed != 0 {
        // SAFETY: not registered: nothing else holds it
        drop(unsafe { Box::from_raw(context) });
        tracing::warn!(error = failed, "no network watch: the core reconnects on its own");
    }
}

#[cfg(test)]
mod tests {
    use super::{next, Report};
    use std::time::Duration;

    #[test]
    fn only_the_network_coming_back_reconnects() {
        // the first report: nothing was lost
        assert_eq!(next(None, Report::Online(true)), (Some(true), false));
        assert_eq!(next(None, Report::Online(false)), (Some(false), false));
        // lost, then back
        assert_eq!(next(Some(true), Report::Online(false)), (Some(false), false));
        assert_eq!(next(Some(false), Report::Online(true)), (Some(true), true));
        // told again, unchanged (a second interface, a cost change)
        assert_eq!(next(Some(true), Report::Online(true)), (Some(true), false));
        assert_eq!(next(Some(false), Report::Online(false)), (Some(false), false));
    }

    #[test]
    fn waking_reconnects_unless_offline() {
        // the old connection may be dead though the network stayed
        assert_eq!(next(Some(true), Report::Woke), (Some(true), true));
        // without a network the attempt fails, and its return reconnects anyway
        assert_eq!(next(Some(false), Report::Woke), (Some(false), false));
        // no watcher to tell: try
        assert_eq!(next(None, Report::Woke), (None, true));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_monitor_reports_the_network_at_once() {
        let (reports, mut reported) = futures::channel::mpsc::unbounded();
        super::platform(reports);
        for _ in 0..100 {
            if let Ok(report) = reported.try_recv() {
                return assert!(matches!(report, Report::Online(_)), "{report:?}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("no report in 5 s");
    }
}

/// The app's part, in a real (headless) window over a core that isn't linked.
#[cfg(all(test, feature = "snapshot"))]
mod window_tests {
    use async_compat::Compat;
    use gpui_kit::{gpui, AppContext as _, TestAppContext, WindowOptions};

    use super::super::attach::window_tests::with_core;
    use super::super::{Hush, Screen};
    use crate::Demo;

    #[gpui::test]
    fn the_network_coming_back_reconnects_only_over_the_chats(cx: &mut TestAppContext) {
        let (_, hush) = cx.update(|cx| {
            gpui_kit::init(cx);
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Hush::demo(Demo::Chat, window, cx))
            })
            .expect("the window opens")
        });
        let data = std::env::temp_dir().join(format!("hush-desktop-network-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        with_core(&hush, &data, cx);
        // stopped: each reconnect asked of it fails, and says so
        let client = hush.read_with(cx, |app, _| app.client.clone()).expect("a core");
        futures::executor::block_on(Compat::new(client.stop()));
        let reconnected = |screen: Screen, cx: &mut TestAppContext| {
            hush.update(cx, |app, cx| {
                app.screen = screen;
                app.error = None;
                app.network_returned(cx);
                app.error.is_some()
            })
        };
        assert!(reconnected(Screen::Chats, cx));
        // starting an unlinked core would end linking, its code with it
        let linking = Screen::Link {
            url: Some("sgnl://linkdevice?uuid=demo".into()),
        };
        assert!(!reconnected(linking, cx));
        assert!(!reconnected(Screen::Unlinked, cx));
        assert!(!reconnected(Screen::Opening, cx));
        std::fs::remove_dir_all(&data).unwrap();
    }
}
