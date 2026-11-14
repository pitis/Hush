//! Reconnects as soon as the network comes back, or the computer wakes from sleep, rather than
//! after the core's backoff (the Mac app's `NetworkWatch`). The network: NWPathMonitor on macOS,
//! NetworkManager's state over D-Bus on Linux, the connectivity hint on Windows (10 2004 on).
//! Without one (Linux without NetworkManager) only the core's own backoff reconnects, as before.

use std::time::{Duration, SystemTime};

use futures::channel::mpsc::{unbounded, UnboundedSender};
use futures::StreamExt;
use gpui_kit::*;

use super::{Hush, Screen};

/// How often the clock is looked at, to tell a sleep.
const TICK: Duration = Duration::from_secs(5);

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

/// A tick come this late: the computer slept in between (the clock went on, the timer didn't).
fn slept(gap: Duration) -> bool {
    gap > TICK * 4
}

/// Watches the network and the clock for as long as the app runs.
pub fn watch(hush: WeakEntity<Hush>, cx: &mut App) {
    let (reports, mut reported) = unbounded();
    platform(reports.clone());
    let executor = cx.background_executor().clone();
    cx.background_spawn(async move {
        loop {
            let before = SystemTime::now();
            executor.timer(TICK).await;
            let gap = SystemTime::now().duration_since(before).unwrap_or_default();
            if slept(gap) && reports.unbounded_send(Report::Woke).is_err() {
                break;
            }
        }
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
    /// `NM_STATE_CONNECTED_GLOBAL`: a way out to the internet.
    const GLOBAL: u32 = 70;

    let watch = move || -> zbus::Result<()> {
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
        let _ = reports.unbounded_send(Report::Online(state == GLOBAL));
        for change in changes {
            let state: u32 = change.body().deserialize()?;
            if reports.unbounded_send(Report::Online(state == GLOBAL)).is_err() {
                break;
            }
        }
        Ok(())
    };
    let started = std::thread::Builder::new().name("network-watch".into()).spawn(move || {
        if let Err(e) = watch() {
            tracing::warn!(error = %e, "no network watch: the core reconnects on its own");
        }
    });
    if let Err(e) = started {
        tracing::warn!(error = %e, "no network watch: the core reconnects on its own");
    }
}

/// The system's connectivity hint, told on a thread of its pool; registered for the app's life.
#[cfg(windows)]
fn platform(reports: UnboundedSender<Report>) {
    use std::ffi::c_void;
    use windows_sys::Win32::NetworkManagement::IpHelper::NotifyNetworkConnectivityHintChange;
    use windows_sys::Win32::Networking::WinSock::{
        NetworkConnectivityLevelHintHidden, NetworkConnectivityLevelHintInternetAccess,
        NetworkConnectivityLevelHintUnknown, NL_NETWORK_CONNECTIVITY_HINT,
    };

    unsafe extern "system" fn changed(context: *const c_void, hint: NL_NETWORK_CONNECTIVITY_HINT) {
        // SAFETY: the sender registered with the callback, never freed while it is
        let reports = unsafe { &*context.cast::<UnboundedSender<Report>>() };
        let level = hint.ConnectivityLevel;
        if level == NetworkConnectivityLevelHintUnknown || level == NetworkConnectivityLevelHintHidden {
            return;
        }
        // not a captive portal's constrained access: the server can't be reached through it
        let online = level == NetworkConnectivityLevelHintInternetAccess;
        let _ = reports.unbounded_send(Report::Online(online));
    }

    let context = Box::into_raw(Box::new(reports));
    let mut handle = std::ptr::null_mut();
    // SAFETY: `context` stays valid for as long as the registration (the app's life)
    let failed = unsafe { NotifyNetworkConnectivityHintChange(Some(changed), context.cast(), true, &mut handle) };
    if failed != 0 {
        // SAFETY: not registered: nothing else holds it
        drop(unsafe { Box::from_raw(context) });
        tracing::warn!(error = failed, "no network watch: the core reconnects on its own");
    }
}

#[cfg(test)]
mod tests {
    use super::{next, slept, Report, TICK};
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

    #[test]
    fn a_tick_come_late_is_a_sleep() {
        assert!(!slept(TICK));
        // a busy computer, a clock set back
        assert!(!slept(TICK + Duration::from_secs(3)));
        assert!(!slept(Duration::ZERO));
        assert!(slept(Duration::from_secs(60)));
        assert!(slept(Duration::from_secs(8 * 3600)));
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
