//! The core's log (its info and up, presage's warnings and up) goes to the unified log on Apple
//! platforms (Console.app, subsystem `dev.pitis.hush`), to stderr elsewhere. Never log message
//! content, nor anyone's id: the core names chats by opaque id (`ProjectionDb::opaque_id`), and
//! every line is scrubbed of what presage's and libsignal's messages and errors carry.

use std::sync::{LazyLock, Once};

use tracing::field::{display, Field, Value, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::EnvFilter;

pub fn init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // sqlx statements stay off even if someone raises the level: they can carry key pragmas.
        // RingRTC's warnings come through the `log` bridge (its info level narrates every call).
        // Warnings only: they carry call ids and states, which is what debugging a failed call
        // needs; nothing finer, which could carry network addresses, reaches the log.
        // libsignal-service's decryption warnings dump whole envelopes (who wrote when): errors only.
        let filter = EnvFilter::new("warn,sig_core=info,sqlx=off,ringrtc=warn,libsignal_service::cipher=error");
        let subscriber = tracing_subscriber::registry().with(filter);
        #[cfg(target_vendor = "apple")]
        let subscriber = subscriber.with(Scrub(tracing_oslog::OsLogger::new("dev.pitis.hush", "core")));
        #[cfg(not(target_vendor = "apple"))]
        let subscriber = subscriber.with(Scrub(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(false),
        ));
        let _ = tracing::subscriber::set_global_default(subscriber);
        let _ = tracing_log::LogTracer::init();
    });
}

/// `text` with service ids (UUIDs), keys and other long hex, phone numbers and byte dumps
/// blanked out.
fn scrub(text: &str) -> String {
    static SECRETS: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(concat!(
            r"(?i)[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
            r"|[0-9a-f]{32,}",
            r"|\+[0-9]{7,15}",
            r"|\[(?:[0-9]{1,3}, ){15,}[0-9]{1,3}\]",
        ))
        .expect("a valid pattern")
    });
    SECRETS.replace_all(text, "<redacted>").into_owned()
}

/// Hands each event on to its layer (the log) with every field scrubbed.
struct Scrub<L>(L);

/// An event's fields as text.
#[derive(Default)]
struct Fields(Vec<(&'static str, String)>);

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.push((field.name(), value.to_owned()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.push((field.name(), format!("{value:?}")));
    }
}

impl<S: Subscriber + for<'a> LookupSpan<'a>, L: Layer<S>> Layer<S> for Scrub<L> {
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let mut seen = Fields::default();
        event.record(&mut seen);
        let fields = event.metadata().fields();
        let texts: Vec<_> = fields
            .iter()
            .map(|f| {
                seen.0
                    .iter()
                    .find(|(name, _)| *name == f.name())
                    .map(|(_, v)| display(scrub(v)))
            })
            .collect();
        let values: Vec<Option<&dyn Value>> = texts.iter().map(|t| t.as_ref().map(|t| t as &dyn Value)).collect();
        let values = fields.value_set_all(&values);
        let scrubbed = match event.is_contextual() {
            true => Event::new(event.metadata(), &values),
            false => Event::new_child_of(event.parent().cloned(), event.metadata(), &values),
        };
        self.0.on_event(&scrubbed, ctx);
    }

    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        self.0.on_new_span(attrs, id, ctx);
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        self.0.on_record(id, values, ctx);
    }

    fn on_enter(&self, id: &Id, ctx: Context<'_, S>) {
        self.0.on_enter(id, ctx);
    }

    fn on_exit(&self, id: &Id, ctx: Context<'_, S>) {
        self.0.on_exit(id, ctx);
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        self.0.on_close(id, ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Keeps each event's fields as text.
    struct Capture(Arc<Mutex<Vec<String>>>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
        fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
            struct Text<'a>(&'a mut String);
            impl tracing::field::Visit for Text<'_> {
                fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                    self.0.push_str(&format!("{}={value:?} ", field.name()));
                }
            }
            let mut line = String::new();
            event.record(&mut Text(&mut line));
            self.0.lock().unwrap().push(line);
        }
    }

    #[test]
    fn no_service_id_phone_number_or_key_reaches_the_log() {
        let lines = Arc::new(Mutex::new(vec![]));
        let subscriber = tracing_subscriber::registry().with(Scrub(Capture(lines.clone())));
        let aci = "4a6e1f3c-2b7d-4c5e-9f80-1a2b3c4d5e6f";
        let key = "ab".repeat(32);
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(sender = %aci, error = %format!("session with PNI:{} not found", aci.to_uppercase()), "a message couldn't be decrypted");
            tracing::warn!(
                "Thread(group={key}) has no message from +40712345678 at {}",
                1_700_000_000_000u64
            );
            tracing::warn!(envelope = ?vec![7u8; 32], chat = "0123456789abcdef", "dumped");
        });
        let lines = lines.lock().unwrap();
        let all = lines.join("\n");
        for secret in [aci, &aci.to_uppercase(), &key, "40712345678", "7, 7, 7"] {
            assert!(!all.contains(secret), "{secret} in {all}");
        }
        // what happened, its error kind, timestamps and opaque ids stay
        assert!(lines[0].contains("a message couldn't be decrypted") && lines[0].contains("session with PNI:"));
        assert!(lines[1].contains("1700000000000"));
        assert!(lines[2].contains("chat=0123456789abcdef"));
    }
}
