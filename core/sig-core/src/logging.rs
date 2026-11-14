//! The core's log (its info and up, presage's warnings and up) goes to the unified log on Apple
//! platforms (Console.app, subsystem `dev.pitis.hush`), to stderr elsewhere. Never log message
//! content, nor anyone's id: the core names chats by opaque id (`ProjectionDb::opaque_id`), and
//! every line is scrubbed of what presage's and libsignal's messages and errors carry.

use std::sync::{LazyLock, Once};

use tracing::field::{display, Field, FieldSet, Value, ValueSet, Visit};
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
        // libsignal-service's warnings about messages it drops dump whole envelopes: `Scrub`
        // blanks who wrote them.
        let filter = EnvFilter::new("warn,sig_core=info,sqlx=off,ringrtc=warn");
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
/// blanked out. Known limit: base64 and phone numbers without their `+` pass; no log line is
/// known to print either.
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

/// Hands each event and span on to its layer (the log) with every field scrubbed.
struct Scrub<L>(L);

/// Fields as text.
#[derive(Default)]
struct Fields(Vec<(&'static str, String)>);

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.push((field.name(), value.to_owned()));
    }

    /// As hex, so a key or a binary service id is blanked like one in text.
    fn record_bytes(&mut self, field: &Field, value: &[u8]) {
        self.0.push((field.name(), hex::encode(value)));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.push((field.name(), format!("{value:?}")));
    }
}

/// Calls `then` with the values `record` gives `fields`, scrubbed.
fn scrubbed<T>(fields: &FieldSet, record: impl FnOnce(&mut dyn Visit), then: impl FnOnce(&ValueSet<'_>) -> T) -> T {
    let mut seen = Fields::default();
    record(&mut seen);
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
    then(&fields.value_set_all(&values))
}

impl<S: Subscriber + for<'a> LookupSpan<'a>, L: Layer<S>> Layer<S> for Scrub<L> {
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let meta = event.metadata();
        scrubbed(
            meta.fields(),
            |v| event.record(v),
            |values| {
                let scrubbed = match event.is_contextual() {
                    true => Event::new(meta, values),
                    false => Event::new_child_of(event.parent().cloned(), meta, values),
                };
                self.0.on_event(&scrubbed, ctx);
            },
        );
    }

    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let meta = attrs.metadata();
        scrubbed(
            meta.fields(),
            |v| attrs.record(v),
            |values| {
                let scrubbed = match (attrs.is_contextual(), attrs.parent()) {
                    (true, _) => Attributes::new(meta, values),
                    (false, Some(parent)) => Attributes::child_of(parent.clone(), meta, values),
                    (false, None) => Attributes::new_root(meta, values),
                };
                self.0.on_new_span(&scrubbed, id, ctx);
            },
        );
    }

    fn on_record(&self, id: &Id, record: &Record<'_>, ctx: Context<'_, S>) {
        let Some(meta) = ctx.metadata(id) else { return };
        scrubbed(
            meta.fields(),
            |v| record.record(v),
            |values| self.0.on_record(id, &Record::new(values), ctx),
        );
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
    use presage::libsignal_service::prelude::Uuid;
    use std::sync::{Arc, Mutex};

    /// Keeps each event's and span's fields as text.
    struct Capture(Arc<Mutex<Vec<String>>>);

    struct Text<'a>(&'a mut String);

    impl Visit for Text<'_> {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0.push_str(&format!("{}={value:?} ", field.name()));
        }
    }

    impl<S: Subscriber> Layer<S> for Capture {
        fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
            let mut line = String::new();
            event.record(&mut Text(&mut line));
            self.0.lock().unwrap().push(line);
        }

        fn on_new_span(&self, attrs: &Attributes<'_>, _: &Id, _: Context<'_, S>) {
            let mut line = "span ".to_string();
            attrs.record(&mut Text(&mut line));
            self.0.lock().unwrap().push(line);
        }
    }

    #[test]
    fn no_service_id_phone_number_or_key_reaches_the_log() {
        let lines = Arc::new(Mutex::new(vec![]));
        let subscriber = tracing_subscriber::registry().with(Scrub(Capture(lines.clone())));
        let aci = "4a6e1f3c-2b7d-4c5e-9f80-1a2b3c4d5e6f";
        let uuid = Uuid::parse_str(aci).unwrap();
        let key = "ab".repeat(32);
        // as libsignal-service dumps one when it drops a message
        let envelope = presage::libsignal_service::proto::Envelope {
            source_service_id: Some(aci.into()),
            source_service_id_binary: Some(uuid.as_bytes().to_vec()),
            server_guid: Some("9f0c3a1e-5b6d-4e7f-8a9b-0c1d2e3f4a5b".into()),
            content: Some(vec![9; 40]),
            server_timestamp: Some(1_700_000_000_123),
            ..Default::default()
        };
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(sender = %aci, error = %format!("session with PNI:{} not found", aci.to_uppercase()), "a message couldn't be decrypted");
            tracing::warn!(
                "Thread(group={key}) has no message from +40712345678 at {}",
                1_700_000_000_000u64
            );
            tracing::warn!(envelope = ?vec![7u8; 32], chat = "0123456789abcdef", "dumped");
            tracing::warn!(?envelope, "Received a decryption error message.");
            tracing::warn!(id = uuid.as_bytes().as_slice(), pack = %crate::stickers::pack_tag(&[0xab; 16]), "bytes");
            tracing::warn_span!("a span", sender = %aci).in_scope(|| {});
        });
        let lines = lines.lock().unwrap();
        let all = lines.join("\n");
        let simple = uuid.simple().to_string();
        let bytes = format!("{:?}", uuid.as_bytes());
        for secret in [
            aci,
            &aci.to_uppercase(),
            &key,
            "40712345678",
            "7, 7, 7",
            "9, 9, 9",
            &simple,
            &bytes[1..20],
        ] {
            assert!(!all.contains(secret), "{secret} in {all}");
        }
        // what happened, its error kind, timestamps and opaque ids stay
        assert!(lines[0].contains("a message couldn't be decrypted") && lines[0].contains("session with PNI:"));
        assert!(lines[1].contains("1700000000000"));
        assert!(lines[2].contains("chat=0123456789abcdef"));
        assert!(lines[3].contains("Received a decryption error message.") && lines[3].contains("1700000000123"));
        assert!(
            lines[4].contains("pack=abababab"),
            "a pack's short tag stays: {}",
            lines[4]
        );
        assert!(lines[5].starts_with("span sender=<redacted>"), "{}", lines[5]);
    }
}
