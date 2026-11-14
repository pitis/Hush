//! Test binaries only. sqlx closes a dropped connection on its own worker thread, after the drop
//! returns, so the last tests' databases can still be closing as the test binary exits. SQLCipher's
//! exit handler then frees what an encrypted database's close still uses: SIGSEGV after
//! `test result: ok`.

use std::time::{Duration, Instant};

extern "C" {
    fn sqlite3_initialize() -> std::ffi::c_int;
    fn sqlcipher_extra_init(arg: *const std::ffi::c_char) -> std::ffi::c_int;
    fn atexit(f: extern "C" fn()) -> std::ffi::c_int;
}

/// Called before an encrypted database opens. SQLCipher registers its exit handler in its init;
/// this one, registered after it, runs before it and waits for sqlx's workers to end.
pub(crate) fn wait_for_closing_databases() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        sqlite3_initialize();
        // SQLite runs SQLCipher's init after it counts as initialized: another test's first open can
        // still be in it. Called again, it waits for that one under SQLite's main mutex, or does it.
        sqlcipher_extra_init(std::ptr::null());
        atexit(wait);
    });
}

extern "C" fn wait() {
    // a pool never dropped keeps its workers: not forever
    let deadline = Instant::now() + Duration::from_secs(5);
    while workers() > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// sqlx's worker threads still running: one per connection, ending once it closed.
#[cfg(target_vendor = "apple")]
fn workers() -> usize {
    // <sys/proc_info.h>; not in libc
    const PROC_PIDLISTTHREADS: std::ffi::c_int = 6;
    let pid = std::process::id() as std::ffi::c_int;
    let mut ids = [0u64; 4096];
    let size = unsafe {
        libc::proc_pidinfo(
            pid,
            PROC_PIDLISTTHREADS,
            0,
            ids.as_mut_ptr().cast(),
            size_of_val(&ids) as _,
        )
    };
    ids[..size.max(0) as usize / size_of::<u64>()]
        .iter()
        .filter(|&&id| {
            let mut info: libc::proc_threadinfo = unsafe { std::mem::zeroed() };
            let read = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDTHREADINFO,
                    id,
                    (&raw mut info).cast(),
                    size_of_val(&info) as _,
                )
            };
            read > 0
                && unsafe { std::ffi::CStr::from_ptr(info.pth_name.as_ptr()) }
                    .to_bytes()
                    .starts_with(b"sqlx-sqlite-worker")
        })
        .count()
}

/// sqlx's worker threads still running (names cut to 15 bytes there).
#[cfg(target_os = "linux")]
fn workers() -> usize {
    std::fs::read_dir("/proc/self/task")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|t| std::fs::read(t.path().join("comm")).is_ok_and(|name| name.starts_with(b"sqlx-sqlite-wor")))
        .count()
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
fn workers() -> usize {
    0
}
