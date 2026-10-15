//! Outgoing messages: recipient parsing and the sender task on the core thread.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use presage::libsignal_service::content::DataMessage;
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::{Aci, Pni, ServiceId};
use presage::manager::Registered;
use presage::proto::data_message;
use presage::Manager;
use presage_store_sqlite::SqliteStore;
use tokio::sync::{mpsc, watch};

use crate::events::{CoreEvent, EventListener};
use crate::projection::{MessageKey, MessageStatus, Op, ProjectionDb, Quote};

#[derive(Debug, Clone, PartialEq)]
pub enum Recipient {
    Direct(ServiceId),
    Group([u8; 32]),
}

/// The recipient a conversation id addresses: `c:<aci>`, `c:PNI:<pni>` or `g:<hex master key>`.
pub fn recipient(conversation_id: &str) -> Option<Recipient> {
    if let Some(rest) = conversation_id.strip_prefix("c:") {
        return match rest.strip_prefix("PNI:") {
            Some(pni) => Uuid::parse_str(pni)
                .ok()
                .map(|u| Recipient::Direct(Pni::from(u).into())),
            None => Uuid::parse_str(rest)
                .ok()
                .map(|u| Recipient::Direct(Aci::from(u).into())),
        };
    }
    let key = hex::decode(conversation_id.strip_prefix("g:")?).ok()?;
    Some(Recipient::Group(key.try_into().ok()?))
}

/// One queued send of a message already stored as `Pending`.
pub struct SendJob {
    pub key: MessageKey,
    pub conversation_id: String,
    pub body: String,
    pub quote: Option<Quote>,
}

fn data_message(job: &SendJob) -> DataMessage {
    DataMessage {
        body: Some(job.body.clone()),
        timestamp: Some(job.key.sent_at as u64),
        quote: job.quote.as_ref().map(|q| data_message::Quote {
            id: Some(q.sent_at as u64),
            author_aci: Some(q.author.clone()),
            text: q.text.clone(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// A send that takes longer than this is reported Failed (it may still go out; a receipt fixes it).
const SEND_TIMEOUT: Duration = Duration::from_secs(60);

/// The job queue, shared so a restarted sender keeps the jobs queued before a crash.
pub type Jobs = Rc<RefCell<mpsc::UnboundedReceiver<SendJob>>>;

/// Sends queued messages in order, once the server queue has been drained (presage: sessions,
/// profile keys and sender keys must be up to date before encrypting). Sends share the session
/// store with the receive loop on this thread; presage is used this way by design.
pub async fn sender(
    mut m: Manager<SqliteStore, Registered>,
    db: ProjectionDb,
    l: Arc<dyn EventListener>,
    jobs: Jobs,
    mut caught_up: watch::Receiver<bool>,
) {
    if caught_up.wait_for(|ready| *ready).await.is_err() {
        return;
    }
    loop {
        // the RefCell borrow is never held across the send itself
        let next = std::future::poll_fn(|cx| jobs.borrow_mut().poll_recv(cx)).await;
        let Some(job) = next else { return };
        let result = tokio::time::timeout(SEND_TIMEOUT, send_one(&mut m, &job))
            .await
            .unwrap_or_else(|_| Err("timed out".to_string()));
        let stored = match &result {
            Ok(()) => db
                .apply(&Op::SetStatus {
                    targets: vec![job.key.clone()],
                    status: MessageStatus::Sent,
                })
                .await
                .map(|_| ()),
            Err(_) => db.set_failed(&job.key).await,
        };
        if let Err(e) = result {
            // presage also fails a send whose message was delivered but whose sync transcript or
            // local copy failed; a delivery receipt then lifts the row out of Failed.
            l.on_event(CoreEvent::Error {
                message: format!("a message to {} may not have been sent: {e}", job.conversation_id),
            });
        }
        if let Err(e) = stored {
            l.on_event(CoreEvent::Error {
                message: format!("recording a send failed: {e}"),
            });
        }
        l.on_event(CoreEvent::MessageChanged {
            conversation_id: job.conversation_id.clone(),
            key: job.key,
        });
    }
}

async fn send_one(m: &mut Manager<SqliteStore, Registered>, job: &SendJob) -> Result<(), String> {
    let at = job.key.sent_at as u64;
    match recipient(&job.conversation_id) {
        Some(Recipient::Direct(to)) => Box::pin(m.send_message(to, data_message(job), at))
            .await
            .map_err(|e| e.to_string()),
        Some(Recipient::Group(key)) => Box::pin(m.send_message_to_group(&key, data_message(job), at))
            .await
            .map_err(|e| e.to_string()),
        None => Err("not a conversation id".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const U: &str = "22222222-2222-2222-2222-222222222222";

    #[test]
    fn parses_every_conversation_id_form() {
        let u = Uuid::parse_str(U).unwrap();
        assert_eq!(
            recipient(&format!("c:{U}")),
            Some(Recipient::Direct(Aci::from(u).into()))
        );
        assert_eq!(
            recipient(&format!("c:PNI:{U}")),
            Some(Recipient::Direct(Pni::from(u).into()))
        );
        assert_eq!(
            recipient(&format!("g:{}", "ab".repeat(32))),
            Some(Recipient::Group([0xab; 32]))
        );
        for bad in [
            "",
            "c:",
            "c:nope",
            "c:PNI:nope",
            "g:abc",
            &format!("g:{}", "ab".repeat(31)),
            "x:1",
        ] {
            assert_eq!(recipient(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn data_message_carries_body_timestamp_and_quote() {
        let job = SendJob {
            key: MessageKey {
                author: "me".into(),
                sent_at: 42,
            },
            conversation_id: format!("c:{U}"),
            body: "hi".into(),
            quote: Some(Quote {
                author: U.into(),
                sent_at: 7,
                text: Some("orig".into()),
            }),
        };
        let dm = data_message(&job);
        assert_eq!((dm.body.as_deref(), dm.timestamp), (Some("hi"), Some(42)));
        let q = dm.quote.unwrap();
        assert_eq!(
            (q.id, q.author_aci.as_deref(), q.text.as_deref()),
            (Some(7), Some(U), Some("orig"))
        );
    }
}
