//! The core's log (its info and up, presage's warnings and up) goes to the unified log on Apple
//! platforms (Console.app, subsystem `dev.pitis.hush`), to stderr elsewhere. Never log message
//! content.

use std::sync::Once;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::EnvFilter;

pub fn init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // sqlx statements stay off even if someone raises the level: they can carry key pragmas.
        // RingRTC's warnings come through the `log` bridge (its info level narrates every call).
        // Warnings only: they carry call ids and states, which is what debugging a failed call
        // needs; nothing finer, which could carry network addresses, reaches the log.
        let filter = EnvFilter::new("warn,sig_core=info,sqlx=off,ringrtc=warn");
        let subscriber = tracing_subscriber::registry().with(filter);
        #[cfg(target_vendor = "apple")]
        let subscriber = subscriber.with(tracing_oslog::OsLogger::new("dev.pitis.hush", "core"));
        #[cfg(not(target_vendor = "apple"))]
        let subscriber = subscriber.with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(false),
        );
        let _ = tracing::subscriber::set_global_default(subscriber);
        let _ = tracing_log::LogTracer::init();
    });
}
