//! Warnings and errors from the core and presage go to the unified log (Console.app, subsystem
//! `dev.pitis.semaphore`). Never log message content.

use std::sync::Once;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::EnvFilter;

pub fn init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // sqlx statements stay off even if someone raises the level: they can carry key pragmas.
        // RingRTC's warnings come through the `log` bridge (its info level narrates every call).
        let filter = EnvFilter::new("warn,sig_core=info,sqlx=off,ringrtc=warn");
        let subscriber = tracing_subscriber::registry()
            .with(filter)
            .with(tracing_oslog::OsLogger::new("dev.pitis.semaphore", "core"));
        let _ = tracing::subscriber::set_global_default(subscriber);
        let _ = tracing_log::LogTracer::init();
    });
}
