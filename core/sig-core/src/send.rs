//! Outgoing messages: recipient parsing, what each kind of send looks like on the wire, and the
//! sender task on the core thread.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use presage::libsignal_service::content::{
    sync_message, Content, ContentBody, DataMessage, EditMessage, GroupContextV2, Metadata, ReceiptMessage,
    SyncMessage, TypingMessage,
};
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::{Aci, DeviceId, Pni, ServiceId};
use presage::libsignal_service::sender::AttachmentSpec;
use presage::manager::Registered;
use presage::proto::{data_message, receipt_message, typing_message, AttachmentPointer};
use presage::store::{ContentsStore, Thread};
use presage::Manager;
use presage_store_sqlite::SqliteStore;
use tokio::sync::{mpsc, watch};

use crate::events::{from_applied, CoreEvent, EventListener};
use crate::projection::{
    project, BodyRange, Message, MessageKey, MessageStatus, Op, ProjectionDb, Quote, RequestResponse, TextStyle,
};
use crate::typing::TYPING_IDLE;

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

/// presage's thread for a conversation id: where it keeps the chat's messages and timer.
pub fn thread(conversation_id: &str) -> Option<Thread> {
    Some(match recipient(conversation_id)? {
        Recipient::Direct(sid) => Thread::Contact(sid),
        Recipient::Group(key) => Thread::Group(key),
    })
}

/// A message: queued as `Job::Text` once stored as `Pending`, and the content of an edit.
pub struct SendJob {
    pub key: MessageKey,
    pub conversation_id: String,
    pub body: Option<String>,
    /// Styles and mentions in `body`.
    pub ranges: Vec<BodyRange>,
    pub quote: Option<Quote>,
    /// Ids of its stored files, uploaded before it is sent.
    pub attachments: Vec<i64>,
}

/// Everything the sender sends, in order.
pub enum Job {
    Text(SendJob),
    /// A reaction, edit or delete (built by `reaction`, `edit` or `delete`), sent at `at`. Applied
    /// here after sending, exactly as our phone applies the sync transcript of it.
    Mutation {
        conversation_id: String,
        body: Box<ContentBody>,
        at: u64,
    },
    /// A call signaling envelope, sent at `at`; the outcome goes back through `done` (RingRTC
    /// retries or fails the call on it).
    Call {
        conversation_id: String,
        body: Box<ContentBody>,
        at: u64,
        done: tokio::sync::oneshot::Sender<bool>,
    },
    /// Send the unsent read receipts (`ProjectionDb::unsent_receipts`) and the read state to our
    /// phone. Queued by mark-read and when the phone's settings arrive.
    Receipts,
    /// Tell our other devices the Storage Service changed (we just wrote to it).
    StorageChanged,
    /// Tell our other devices how we answered a 1:1 chat's message request (nothing for groups).
    RequestResponse {
        conversation_id: String,
        response: RequestResponse,
    },
    /// Send our other devices the whole block list, as it is when the job runs.
    BlockedList,
    /// Give a contact our profile key (name, avatar): we accepted their message request.
    ProfileKey {
        conversation_id: String,
    },
    /// Started or Stopped; dropped if it waited `TYPING_IDLE` (since `queued`) before going out.
    Typing {
        conversation_id: String,
        started: bool,
        queued: Instant,
    },
}

/// Every message in a group carries the group's master key, or members get it as a 1:1 message.
fn group_context(conversation_id: &str) -> Option<GroupContextV2> {
    match recipient(conversation_id)? {
        Recipient::Group(key) => Some(GroupContextV2 {
            master_key: Some(key.to_vec()),
            revision: Some(0),
            ..Default::default()
        }),
        Recipient::Direct(_) => None,
    }
}

fn data_message(job: &SendJob, attachments: Vec<AttachmentPointer>) -> DataMessage {
    DataMessage {
        body: job.body.clone(),
        body_ranges: body_ranges(&job.ranges),
        attachments,
        timestamp: Some(job.key.sent_at as u64),
        quote: job.quote.as_ref().map(|q| data_message::Quote {
            id: Some(q.sent_at as u64),
            author_aci: Some(q.author.clone()),
            text: q.text.clone(),
            body_ranges: body_ranges(&q.ranges),
            ..Default::default()
        }),
        group_v2: group_context(&job.conversation_id),
        ..Default::default()
    }
}

/// Our ranges as Signal's body ranges.
fn body_ranges(ranges: &[BodyRange]) -> Vec<presage::proto::BodyRange> {
    use presage::proto::body_range::{AssociatedValue, Style};
    ranges
        .iter()
        .map(|r| presage::proto::BodyRange {
            start: Some(r.start),
            length: Some(r.length),
            associated_value: Some(match &r.style {
                TextStyle::Mention { aci } => AssociatedValue::MentionAci(aci.clone()),
                TextStyle::Bold => AssociatedValue::Style(Style::Bold as i32),
                TextStyle::Italic => AssociatedValue::Style(Style::Italic as i32),
                TextStyle::Spoiler => AssociatedValue::Style(Style::Spoiler as i32),
                TextStyle::Strikethrough => AssociatedValue::Style(Style::Strikethrough as i32),
                TextStyle::Monospace => AssociatedValue::Style(Style::Monospace as i32),
            }),
        })
        .collect()
}

/// Our reaction to `target` (`remove` takes back our reaction `emoji`).
pub fn reaction(conversation_id: &str, target: &MessageKey, emoji: String, remove: bool, at: u64) -> DataMessage {
    DataMessage {
        timestamp: Some(at),
        reaction: Some(data_message::Reaction {
            emoji: Some(emoji),
            remove: Some(remove),
            target_author_aci: Some(target.author.clone()),
            target_sent_timestamp: Some(target.sent_at as u64),
            ..Default::default()
        }),
        group_v2: group_context(conversation_id),
        ..Default::default()
    }
}

/// Deletes our own message sent at `target_sent_at` for everyone.
pub fn delete(conversation_id: &str, target_sent_at: i64, at: u64) -> DataMessage {
    DataMessage {
        timestamp: Some(at),
        delete: Some(data_message::Delete {
            target_sent_timestamp: Some(target_sent_at as u64),
        }),
        group_v2: group_context(conversation_id),
        ..Default::default()
    }
}

/// Replaces the text of our own `original`; the new revision keeps its quote.
pub fn edit(original: &Message, new_body: String, ranges: Vec<BodyRange>, at: u64) -> EditMessage {
    let revision = SendJob {
        key: MessageKey {
            author: original.author.clone(),
            sent_at: at as i64,
        },
        conversation_id: original.conversation_id.clone(),
        body: Some(new_body),
        ranges,
        quote: original.quote.clone(),
        attachments: vec![],
    };
    EditMessage {
        target_sent_timestamp: Some(original.sent_at as u64),
        data_message: Some(data_message(&revision, vec![])),
    }
}

/// Typing in a conversation; groups name the group by its identifier, not its master key.
pub fn typing(conversation_id: &str, started: bool, at: u64) -> Option<TypingMessage> {
    use typing_message::Action;
    let group_id = match recipient(conversation_id)? {
        Recipient::Group(_) => crate::projection::ids::group_identifier(conversation_id),
        Recipient::Direct(_) => None,
    };
    let action = if started { Action::Started } else { Action::Stopped };
    Some(TypingMessage {
        timestamp: Some(at),
        action: Some(action as i32),
        group_id,
    })
}

/// One Read receipt per sender, naming that sender's messages.
pub fn read_receipts(targets: &[MessageKey]) -> Vec<(String, ReceiptMessage)> {
    let mut by_author: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
    for t in targets {
        by_author.entry(&t.author).or_default().push(t.sent_at as u64);
    }
    by_author
        .into_iter()
        .map(|(author, timestamp)| {
            (
                author.to_string(),
                ReceiptMessage {
                    r#type: Some(receipt_message::Type::Read as i32),
                    timestamp,
                },
            )
        })
        .collect()
}

/// The receipts to send for `targets` under the read receipts setting: none when it is off, and
/// `None` (keep them queued) while the phone hasn't told us yet.
pub fn receipts_allowed(setting: Option<bool>, targets: &[MessageKey]) -> Option<Vec<(String, ReceiptMessage)>> {
    match setting? {
        true => Some(read_receipts(targets)),
        false => Some(vec![]),
    }
}

/// Asks our phone for the account's settings (read receipts, typing indicators).
pub fn configuration_request() -> SyncMessage {
    SyncMessage {
        content: Some(sync_message::Content::Request(sync_message::Request {
            r#type: Some(sync_message::request::Type::Configuration as i32),
        })),
        ..Default::default()
    }
}

/// Asks our other devices to fetch the Storage Service manifest: we wrote a new one.
pub fn storage_changed_sync() -> SyncMessage {
    SyncMessage {
        content: Some(sync_message::Content::FetchLatest(sync_message::FetchLatest {
            r#type: Some(sync_message::fetch_latest::Type::StorageManifest as i32),
        })),
        ..Default::default()
    }
}

/// Tells our other devices how we answered a contact's message request. None for a group (as
/// Desktop: they have no ACI to name; the block list carries group blocks) or a PNI chat.
pub fn request_response_sync(conversation_id: &str, response: RequestResponse) -> Option<SyncMessage> {
    use sync_message::message_request_response::Type;
    let Recipient::Direct(ServiceId::Aci(aci)) = recipient(conversation_id)? else {
        return None;
    };
    let uuid = Uuid::from(aci);
    let t = match response {
        RequestResponse::Accept => Type::Accept,
        RequestResponse::Delete => Type::Delete,
        RequestResponse::Block => Type::Block,
        RequestResponse::BlockAndDelete => Type::BlockAndDelete,
    };
    Some(SyncMessage {
        content: Some(sync_message::Content::MessageRequestResponse(
            sync_message::MessageRequestResponse {
                thread_aci: Some(uuid.to_string()),
                thread_aci_binary: Some(uuid.as_bytes().to_vec()),
                group_id: None,
                r#type: Some(t as i32),
            },
        )),
        ..Default::default()
    })
}

/// Our whole block list for our other devices: contacts by ACI (both encodings), groups by
/// identifier. PNI chats can't be named here and are left out.
pub fn blocked_sync(blocked: &[String]) -> SyncMessage {
    let mut b = sync_message::Blocked::default();
    for id in blocked {
        match recipient(id) {
            Some(Recipient::Direct(ServiceId::Aci(aci))) => {
                let uuid = Uuid::from(aci);
                b.acis.push(uuid.to_string());
                b.acis_binary.push(uuid.as_bytes().to_vec());
            }
            Some(Recipient::Group(_)) => b.group_ids.extend(crate::projection::ids::group_identifier(id)),
            _ => {}
        }
    }
    SyncMessage {
        content: Some(sync_message::Content::Blocked(b)),
        ..Default::default()
    }
}

/// A message that only carries our profile key (presage adds the key to every message it
/// sends).
pub fn profile_key_update(at: u64) -> DataMessage {
    DataMessage {
        flags: Some(data_message::Flags::ProfileKeyUpdate as u32),
        timestamp: Some(at),
        ..Default::default()
    }
}

/// Tells our other devices that we read `targets`.
pub fn read_sync(targets: &[MessageKey]) -> SyncMessage {
    SyncMessage {
        read: targets
            .iter()
            .map(|t| sync_message::Read {
                sender_aci: Some(t.author.clone()),
                timestamp: Some(t.sent_at as u64),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

/// The projection ops of `body`, which we sent to `conversation_id`: our own sends are projected
/// as the sync transcript our phone receives for them, so both apply them the same way.
pub fn own_ops(me: &str, conversation_id: &str, body: ContentBody) -> Vec<Op> {
    let (Ok(me_uuid), Ok(device)) = (Uuid::parse_str(me), DeviceId::new(1)) else {
        return vec![];
    };
    let destination = match recipient(conversation_id) {
        Some(Recipient::Direct(to)) => Some(to.service_id_string()),
        _ => None,
    };
    let sent = |message, edit_message| {
        ContentBody::SynchronizeMessage(SyncMessage {
            content: Some(sync_message::Content::Sent(sync_message::Sent {
                destination_service_id: destination,
                message,
                edit_message,
                ..Default::default()
            })),
            ..Default::default()
        })
    };
    let body = match body {
        ContentBody::DataMessage(dm) => sent(Some(dm), None),
        ContentBody::EditMessage(em) => sent(None, Some(em)),
        other => other,
    };
    let me_sid = ServiceId::from(Aci::from(me_uuid));
    let now = chrono::Utc::now();
    let content = Content {
        metadata: Metadata {
            sender: me_sid,
            destination: me_sid,
            sender_device: device,
            pni_verified: None,
            client_timestamp: now,
            server_timestamp: now,
            needs_receipt: false,
            unidentified_sender: false,
            was_plaintext: false,
            server_guid: None,
        },
        body,
    };
    project(&content, me)
}

/// Signal lets you edit or delete a message for everyone for this long after sending it.
pub const CHANGE_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

/// Why `m` can't be edited or deleted for everyone by `me` at `now_ms`, if it can't.
pub fn check_own_change(m: &Message, me: &str, now_ms: i64) -> Result<(), String> {
    if m.author != me {
        return Err("only your own messages can be changed".into());
    }
    if m.deleted {
        return Err("the message was deleted".into());
    }
    if m.status < MessageStatus::Sent {
        return Err("the message hasn't been sent".into());
    }
    if now_ms - m.sent_at > CHANGE_WINDOW_MS {
        return Err("messages can only be changed for 24 hours".into());
    }
    Ok(())
}

/// A send that takes longer than this is reported Failed (it may still go out; a receipt fixes it).
const SEND_TIMEOUT: Duration = Duration::from_secs(60);

/// The job queue, shared so a restarted sender keeps the jobs queued before a crash.
pub type Jobs = Rc<RefCell<mpsc::UnboundedReceiver<Job>>>;

/// Sends queued jobs in order, once the server queue has been drained (presage: sessions,
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
    // Settings are asked for on every start, so a change we missed is picked up. Receipts left
    // over from an earlier run wait for the answer (storing it queues a flush) or for the next
    // mark-read, so they don't go out under a setting the phone may have changed meanwhile.
    let me = m.registration_data().service_ids.aci();
    if let Err(e) = timed(Box::pin(m.send_message(me, configuration_request(), now_ms()))).await {
        tracing::warn!(error = %e, "asking the phone for settings failed");
    }
    loop {
        // the RefCell borrow is never held across the send itself
        let next = std::future::poll_fn(|cx| jobs.borrow_mut().poll_recv(cx)).await;
        let Some(job) = next else { return };
        match job {
            Job::Text(job) => send_text(&mut m, &db, l.as_ref(), job).await,
            Job::Mutation {
                conversation_id,
                body,
                at,
            } => send_mutation(&mut m, &db, l.as_ref(), &conversation_id, *body, at).await,
            Job::Call {
                conversation_id,
                body,
                at,
                done,
            } => {
                let sent = send_to(&mut m, &conversation_id, *body, at).await;
                if let Err(e) = &sent {
                    // ids only; the call engine ends the call, this says why
                    tracing::warn!(conversation = %conversation_id, error = %e, "a call message wasn't sent");
                    l.on_event(CoreEvent::Error {
                        message: format!("a call message to {conversation_id} wasn't sent: {e}"),
                    });
                }
                let _ = done.send(sent.is_ok());
            }
            Job::Receipts => send_receipts(&mut m, &db, l.as_ref()).await,
            Job::StorageChanged => {
                // the write is done either way; the phone also fetches on its own schedule
                if let Err(e) = timed(Box::pin(m.send_message(me, storage_changed_sync(), now_ms()))).await {
                    tracing::warn!(error = %e, "telling the phone the storage changed failed");
                }
            }
            Job::RequestResponse {
                conversation_id,
                response,
            } => {
                let Some(sm) = request_response_sync(&conversation_id, response) else {
                    continue;
                };
                if let Err(e) = timed(Box::pin(m.send_message(me, sm, now_ms()))).await {
                    let what = format!("telling the phone about the request from {conversation_id}");
                    report(l.as_ref(), &what, &e);
                }
            }
            Job::BlockedList => match db.blocked_chats().await {
                Ok(blocked) => {
                    if let Err(e) = timed(Box::pin(m.send_message(me, blocked_sync(&blocked), now_ms()))).await {
                        report(l.as_ref(), "sending the phone the block list", &e);
                    }
                }
                Err(e) => report(l.as_ref(), "reading the block list", &e),
            },
            Job::ProfileKey { conversation_id } => {
                let at = now_ms();
                // not retried: every message we send carries the key too
                if let Err(e) = send_to(&mut m, &conversation_id, profile_key_update(at).into(), at).await {
                    let what = format!("sharing your profile with {conversation_id} (it goes with your next message)");
                    report(l.as_ref(), &what, &e);
                }
            }
            Job::Typing {
                conversation_id,
                started,
                queued,
            } => {
                // typing that waited this long (offline, catching up) is no longer true
                if queued.elapsed() >= TYPING_IDLE {
                    continue;
                }
                // only with typing indicators known to be on (they are an account setting)
                if !matches!(db.settings().await, Ok(s) if s.typing_indicators == Some(true)) {
                    continue;
                }
                let at = now_ms();
                let Some(tm) = typing(&conversation_id, started, at) else {
                    continue;
                };
                if let Err(e) = send_to(&mut m, &conversation_id, tm.into(), at).await {
                    tracing::warn!(conversation = %conversation_id, error = %e, "typing not sent");
                }
            }
        }
    }
}

/// A send that failed and isn't retried: logged and shown (ids and errors only).
fn report(l: &dyn EventListener, what: &str, e: &dyn std::fmt::Display) {
    tracing::warn!(error = %e, "{what} failed");
    l.on_event(CoreEvent::Error {
        message: format!("{what} failed: {e}"),
    });
}

async fn send_text(m: &mut Manager<SqliteStore, Registered>, db: &ProjectionDb, l: &dyn EventListener, job: SendJob) {
    // presage sends it with the chat's timer as it keeps it (by version, which we don't see):
    // our copy disappears with the recipient's
    if let Some(thread) = thread(&job.conversation_id) {
        // unreadable: keep the timer the message was stored with (the chat's), not none
        let timer = match m.store().expire_timer(&thread).await {
            Ok(t) => Ok(t.map_or(0, |(t, _)| t)),
            Err(e) => Err(format!("reading the chat's timer: {e}")),
        };
        let recorded = match timer {
            Ok(timer) => db.set_message_timer(&job.key, timer).await.map_err(|e| e.to_string()),
            Err(e) => Err(e),
        };
        if let Err(e) = recorded {
            l.on_event(CoreEvent::Error {
                message: format!("recording a message's timer failed: {e}"),
            });
        }
    }
    let result = match upload(m, db, &job.attachments).await {
        Ok(pointers) => {
            send_to(
                m,
                &job.conversation_id,
                data_message(&job, pointers).into(),
                job.key.sent_at as u64,
            )
            .await
        }
        Err(e) => Err(format!("uploading failed: {e}")),
    };
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

/// Sends a reaction, edit or delete, then applies it here.
async fn send_mutation(
    m: &mut Manager<SqliteStore, Registered>,
    db: &ProjectionDb,
    l: &dyn EventListener,
    conversation_id: &str,
    body: ContentBody,
    at: u64,
) {
    let sent = send_to(m, conversation_id, body.clone(), at).await;
    let me = m.registration_data().service_ids.aci.to_string();
    record_mutation(db, l, &me, conversation_id, body, sent).await;
}

/// Applies our sent reaction, edit or delete here, even if sending reported an error: presage
/// fails a group send when one member fails, after the others (and our phone) got it, and a send
/// can fail after it went out (timeout, saving presage's copy). Showing it is closer to what
/// everyone else sees than hiding it.
async fn record_mutation(
    db: &ProjectionDb,
    l: &dyn EventListener,
    me: &str,
    conversation_id: &str,
    body: ContentBody,
    sent: Result<(), String>,
) {
    if let Err(e) = sent {
        l.on_event(CoreEvent::Error {
            message: format!("a reaction, edit or delete in {conversation_id} may not have reached everyone: {e}"),
        });
    }
    for op in own_ops(me, conversation_id, body) {
        match db.apply(&op).await {
            Ok(applied) => applied.into_iter().for_each(|a| l.on_event(from_applied(a))),
            Err(e) => l.on_event(CoreEvent::Error {
                message: format!("recording a change in {conversation_id} failed: {e}"),
            }),
        }
    }
}

/// Sends the unsent read receipts, one per sender, as the read receipts setting allows, then the
/// read state to our phone. A receipt stays queued until it went out (a repeat is harmless), and
/// while the setting is unknown.
async fn send_receipts(m: &mut Manager<SqliteStore, Registered>, db: &ProjectionDb, l: &dyn EventListener) {
    let at = now_ms();
    // our other devices learn every read at once, a message request's too; the receipts to the
    // senders may wait (requests, blocked chats, the setting not known yet)
    match db.unsynced_reads().await {
        Ok(reads) if !reads.is_empty() => {
            // Sent to our own account; libsignal-service delivers a sync message to self twice (harmless).
            let me = m.registration_data().service_ids.aci();
            if let Err(e) = timed(Box::pin(m.send_message(me, read_sync(&reads), at))).await {
                l.on_event(CoreEvent::Error {
                    message: format!("couldn't tell your phone what you read; it will be retried: {e}"),
                });
                return;
            }
            if let Err(e) = db.reads_synced(&reads).await {
                l.on_event(CoreEvent::Error {
                    message: format!("recording what your phone was told failed: {e}"),
                });
                return;
            }
        }
        Ok(_) => {}
        Err(e) => {
            l.on_event(CoreEvent::Error {
                message: format!("reading unsent read receipts failed: {e}"),
            });
            return;
        }
    }
    let (targets, settings) = match (db.unsent_receipts().await, db.settings().await) {
        (Ok(t), _) if t.is_empty() => return,
        (Ok(t), Ok(s)) => (t, s),
        (Err(e), _) | (_, Err(e)) => {
            l.on_event(CoreEvent::Error {
                message: format!("reading unsent read receipts failed: {e}"),
            });
            return;
        }
    };
    let allowed = receipts_allowed(settings.read_receipts, &targets);
    let mut done = vec![];
    let mut failed = 0;
    for (author, receipt) in allowed.clone().unwrap_or_default() {
        // the setting may have been turned off while earlier receipts were going out
        if !matches!(db.settings().await, Ok(s) if s.read_receipts == Some(true)) {
            break;
        }
        let sent = match Uuid::parse_str(&author) {
            Ok(u) => timed(Box::pin(m.send_message(Aci::from(u), receipt, at))).await,
            Err(e) => Err(e.to_string()),
        };
        match sent {
            Ok(()) => done.extend(targets.iter().filter(|t| t.author == author).cloned()),
            Err(e) => {
                tracing::warn!(to = %author, error = %e, "read receipt not sent");
                failed += 1;
            }
        }
    }
    if failed > 0 {
        l.on_event(CoreEvent::Error {
            message: format!("read receipts to {failed} people weren't sent; they will be retried"),
        });
    }
    if allowed.is_some_and(|r| r.is_empty()) {
        done = targets; // read receipts are off: nothing to send to anyone
    }
    if let Err(e) = db.receipts_sent(&done).await {
        l.on_event(CoreEvent::Error {
            message: format!("recording sent read receipts failed: {e}"),
        });
    }
}

/// An upload that takes longer is given up (the message is Failed; retry uploads again).
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// Uploads a message's stored files, in order, one at a time (only one is in memory at once).
async fn upload(
    m: &Manager<SqliteStore, Registered>,
    db: &ProjectionDb,
    ids: &[i64],
) -> Result<Vec<AttachmentPointer>, String> {
    let mut pointers = vec![];
    for &id in ids {
        let a = db
            .attachment(id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("a file of the message is gone")?;
        let path = a.path.ok_or("a file of the message is missing")?;
        let bytes = tokio::task::spawn_blocking(move || std::fs::read(path))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        let spec = AttachmentSpec {
            content_type: a.content_type,
            length: bytes.len(),
            file_name: a.file_name,
            preview: None,
            voice_note: Some(a.is_voice_note),
            borderless: Some(false),
            width: a.width,
            height: a.height,
            caption: None,
            blur_hash: None,
        };
        let pointer = tokio::time::timeout(UPLOAD_TIMEOUT, Box::pin(m.upload_attachment(spec, bytes)))
            .await
            .map_err(|_| "timed out".to_string())?
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        pointers.push(pointer);
    }
    Ok(pointers)
}

fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis() as u64
}

async fn timed<E: std::fmt::Display>(send: impl std::future::Future<Output = Result<(), E>>) -> Result<(), String> {
    match tokio::time::timeout(SEND_TIMEOUT, send).await {
        Ok(result) => result.map_err(|e| e.to_string()),
        Err(_) => Err("timed out".to_string()),
    }
}

async fn send_to(
    m: &mut Manager<SqliteStore, Registered>,
    conversation_id: &str,
    body: ContentBody,
    at: u64,
) -> Result<(), String> {
    match recipient(conversation_id) {
        Some(Recipient::Direct(to)) => timed(Box::pin(m.send_message(to, body, at))).await,
        Some(Recipient::Group(key)) => timed(Box::pin(m.send_message_to_group(&key, body, at))).await,
        None => Err("not a conversation id".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const U: &str = "22222222-2222-2222-2222-222222222222";
    const ME: &str = "11111111-1111-1111-1111-111111111111";

    #[test]
    fn the_storage_changed_sync_asks_the_other_devices_for_the_manifest() {
        let sm = storage_changed_sync();
        assert!(matches!(
            sm.content,
            Some(sync_message::Content::FetchLatest(f))
                if f.r#type == Some(sync_message::fetch_latest::Type::StorageManifest as i32)
        ));
        assert!(sm.read.is_empty());
    }

    #[test]
    fn a_request_answer_tells_the_other_devices_which_contact_and_what() {
        use sync_message::message_request_response::Type;
        let u = Uuid::parse_str(U).unwrap();
        for (response, t) in [
            (RequestResponse::Accept, Type::Accept),
            (RequestResponse::Block, Type::Block),
            (RequestResponse::Delete, Type::Delete),
            (RequestResponse::BlockAndDelete, Type::BlockAndDelete),
        ] {
            let Some(sync_message::Content::MessageRequestResponse(r)) =
                request_response_sync(&format!("c:{U}"), response).and_then(|s| s.content)
            else {
                panic!("{response:?}")
            };
            assert_eq!(r.r#type, Some(t as i32));
            assert_eq!(r.thread_aci.as_deref(), Some(U));
            assert_eq!(r.thread_aci_binary.as_deref(), Some(&u.as_bytes()[..]));
            assert_eq!(r.group_id, None);
        }
        let group = format!("g:{}", "ab".repeat(32));
        assert!(
            request_response_sync(&group, RequestResponse::Accept).is_none(),
            "Desktop sends none for groups (they have no ACI); the block list covers them"
        );
        assert!(request_response_sync(&format!("c:PNI:{U}"), RequestResponse::Accept).is_none());
    }

    #[test]
    fn the_block_list_names_contacts_both_ways_and_groups_by_identifier() {
        let u = Uuid::parse_str(U).unwrap();
        let group = format!("g:{}", "ab".repeat(32));
        let Some(sync_message::Content::Blocked(b)) =
            blocked_sync(&[format!("c:{U}"), group.clone(), format!("c:PNI:{U}")]).content
        else {
            panic!()
        };
        assert_eq!(b.acis, vec![U.to_string()]);
        assert_eq!(b.acis_binary, vec![u.as_bytes().to_vec()]);
        assert_eq!(
            b.group_ids,
            vec![crate::projection::ids::group_identifier(&group).unwrap()]
        );
        assert!(b.numbers.is_empty());
    }

    #[test]
    fn the_profile_key_message_is_flagged_and_bodiless() {
        let dm = profile_key_update(42);
        assert_eq!(dm.flags, Some(data_message::Flags::ProfileKeyUpdate as u32));
        assert_eq!((dm.timestamp, dm.body), (Some(42), None));
        assert_eq!(dm.profile_key, None, "presage fills in our key as it sends");
    }

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
    fn group_text_names_its_group_so_members_dont_get_it_as_a_direct_message() {
        let job = SendJob {
            key: MessageKey {
                author: "me".into(),
                sent_at: 42,
            },
            conversation_id: format!("g:{}", "ab".repeat(32)),
            body: Some("hi".into()),
            ranges: vec![],
            attachments: vec![],
            quote: None,
        };
        let g = data_message(&job, vec![]).group_v2.expect("group context");
        assert_eq!(g.master_key, Some(vec![0xab; 32]));
        let direct = SendJob {
            conversation_id: format!("c:{U}"),
            ..job
        };
        assert_eq!(data_message(&direct, vec![]).group_v2, None);
    }

    #[test]
    fn files_go_out_in_order_with_their_caption() {
        let job = SendJob {
            key: MessageKey {
                author: "me".into(),
                sent_at: 42,
            },
            conversation_id: format!("c:{U}"),
            body: None,
            ranges: vec![],
            quote: None,
            attachments: vec![1, 2],
        };
        let pointer = |name: &str| AttachmentPointer {
            file_name: Some(name.into()),
            ..Default::default()
        };
        let dm = data_message(&job, vec![pointer("a"), pointer("b")]);
        assert_eq!(dm.body, None);
        assert_eq!(dm.attachments, vec![pointer("a"), pointer("b")]);
        let captioned = SendJob {
            body: Some("look".into()),
            ..job
        };
        assert_eq!(
            data_message(&captioned, vec![pointer("a")]).body.as_deref(),
            Some("look")
        );
    }

    #[test]
    fn data_message_carries_body_timestamp_and_quote() {
        let job = SendJob {
            key: MessageKey {
                author: "me".into(),
                sent_at: 42,
            },
            conversation_id: format!("c:{U}"),
            body: Some("hi".into()),
            ranges: vec![BodyRange {
                start: 0,
                length: 2,
                style: TextStyle::Italic,
            }],
            attachments: vec![],
            quote: Some(Quote {
                author: U.into(),
                sent_at: 7,
                ranges: vec![
                    BodyRange {
                        start: 0,
                        length: 1,
                        style: TextStyle::Mention { aci: U.into() },
                    },
                    BodyRange {
                        start: 2,
                        length: 4,
                        style: TextStyle::Bold,
                    },
                ],
                text: Some("\u{fffc} orig".into()),
            }),
        };
        let dm = data_message(&job, vec![]);
        assert_eq!((dm.body.as_deref(), dm.timestamp), (Some("hi"), Some(42)));
        use presage::proto::body_range::{AssociatedValue, Style};
        assert_eq!(
            dm.body_ranges,
            vec![presage::proto::BodyRange {
                start: Some(0),
                length: Some(2),
                associated_value: Some(AssociatedValue::Style(Style::Italic as i32)),
            }]
        );
        let q = dm.quote.unwrap();
        assert_eq!(
            (q.id, q.author_aci.as_deref(), q.text.as_deref()),
            (Some(7), Some(U), Some("\u{fffc} orig"))
        );
        // the quoted text's mention and style go along, so it reads the same there
        assert_eq!(
            q.body_ranges,
            vec![
                presage::proto::BodyRange {
                    start: Some(0),
                    length: Some(1),
                    associated_value: Some(AssociatedValue::MentionAci(U.into())),
                },
                presage::proto::BodyRange {
                    start: Some(2),
                    length: Some(4),
                    associated_value: Some(AssociatedValue::Style(Style::Bold as i32)),
                },
            ]
        );
    }

    fn key(author: &str, sent_at: i64) -> MessageKey {
        MessageKey {
            author: author.into(),
            sent_at,
        }
    }

    fn group() -> String {
        format!("g:{}", "07".repeat(32))
    }

    fn mine(conversation_id: &str, sent_at: i64) -> Message {
        Message {
            conversation_id: conversation_id.into(),
            author: ME.into(),
            sent_at,
            received_at: sent_at,
            ranges: vec![],
            body: Some("orig".into()),
            is_outgoing: true,
            status: MessageStatus::Sent,
            edited: false,
            deleted: false,
            quote: Some(Quote {
                author: U.into(),
                sent_at: 5,
                ranges: vec![],
                text: Some("q".into()),
            }),
            kind: crate::projection::MessageKind::Normal,
            expires_in: 0,
            expires_at: None,
        }
    }

    // Our own changes yield the same ops as the same change made by someone else (see the
    // projector's tests), with us as the author.

    #[test]
    fn own_reaction_applies_like_an_incoming_one() {
        for conv in [format!("c:{U}"), group()] {
            let dm = reaction(&conv, &key(U, 100), "👍".into(), false, 600);
            assert_eq!(dm.group_v2.is_some(), conv.starts_with("g:"));
            assert_eq!(
                own_ops(ME, &conv, dm.into()),
                vec![Op::React {
                    target: key(U, 100),
                    reactor: ME.into(),
                    emoji: Some("👍".into())
                }],
                "{conv}"
            );
        }
        let undo = reaction(&format!("c:{U}"), &key(U, 100), "👍".into(), true, 601);
        assert_eq!(
            own_ops(ME, &format!("c:{U}"), undo.into()),
            vec![Op::React {
                target: key(U, 100),
                reactor: ME.into(),
                emoji: None
            }]
        );
    }

    #[test]
    fn own_delete_applies_like_an_incoming_one() {
        for conv in [format!("c:{U}"), group()] {
            let dm = delete(&conv, 100, 700);
            assert_eq!(dm.group_v2.is_some(), conv.starts_with("g:"));
            assert_eq!(own_ops(ME, &conv, dm.into()), vec![Op::Delete { target: key(ME, 100) }]);
        }
    }

    #[test]
    fn own_edit_applies_like_an_incoming_one_and_keeps_the_quote() {
        for conv in [format!("c:{U}"), group()] {
            let bold = BodyRange {
                start: 0,
                length: 5,
                style: TextStyle::Bold,
            };
            let em = edit(&mine(&conv, 100), "fixed".into(), vec![bold.clone()], 800);
            let dm = em.data_message.as_ref().unwrap();
            assert_eq!(em.target_sent_timestamp, Some(100));
            assert_eq!(
                (dm.timestamp, dm.group_v2.is_some()),
                (Some(800), conv.starts_with("g:"))
            );
            assert_eq!(dm.quote.as_ref().and_then(|q| q.id), Some(5));
            assert_eq!(
                own_ops(ME, &conv, em.into()),
                vec![Op::Edit {
                    target: key(ME, 100),
                    new_body: Some("fixed".into()),
                    new_ranges: vec![bold.clone()],
                    edited_at: 800
                }]
            );
        }
    }

    #[test]
    fn own_read_applies_like_the_phone_reading() {
        let targets = vec![key(U, 100), key(ME, 1), key(U, 101)];
        let ops = own_ops(ME, &format!("c:{U}"), read_sync(&targets).into());
        assert!(
            matches!(&ops[..], [Op::Read { targets: t, .. }] if *t == targets),
            "{ops:?}"
        );
    }

    struct Sink(std::sync::Mutex<Vec<CoreEvent>>);
    impl EventListener for Sink {
        fn on_event(&self, e: CoreEvent) {
            self.0.lock().unwrap().push(e);
        }
    }

    #[tokio::test]
    async fn a_change_that_may_not_have_reached_everyone_still_shows_here() {
        let db = ProjectionDb::open("sqlite::memory:", None).await.unwrap();
        let conv = format!("c:{U}");
        for t in [100, 101] {
            db.insert_message(&mine(&conv, t)).await.unwrap();
        }
        let sink = Sink(Default::default());
        record_mutation(&db, &sink, ME, &conv, delete(&conv, 100, 700).into(), Ok(())).await;
        assert!(db.message(&key(ME, 100)).await.unwrap().unwrap().deleted);
        assert!(!sink
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, CoreEvent::Error { .. })));
        let failed = Err("one member failed".to_string());
        record_mutation(&db, &sink, ME, &conv, delete(&conv, 101, 701).into(), failed).await;
        assert!(db.message(&key(ME, 101)).await.unwrap().unwrap().deleted);
        let events = sink.0.lock().unwrap();
        assert!(events
            .iter()
            .any(|e| matches!(e, CoreEvent::Error { message } if message.contains("may not have reached everyone"))));
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, CoreEvent::MessageChanged { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn receipts_follow_the_read_receipts_setting() {
        let targets = [key(U, 100)];
        assert_eq!(receipts_allowed(None, &targets), None);
        assert_eq!(receipts_allowed(Some(false), &targets), Some(vec![]));
        assert_eq!(receipts_allowed(Some(true), &targets), Some(read_receipts(&targets)));
        let request = match configuration_request().content {
            Some(sync_message::Content::Request(r)) => r.r#type,
            other => panic!("{other:?}"),
        };
        assert_eq!(request, Some(sync_message::request::Type::Configuration as i32));
    }

    #[test]
    fn read_receipts_go_to_each_sender_once() {
        let b = "33333333-3333-3333-3333-333333333333";
        let receipts = read_receipts(&[key(U, 100), key(b, 50), key(U, 101)]);
        let read = Some(receipt_message::Type::Read as i32);
        assert_eq!(
            receipts,
            vec![
                (
                    U.to_string(),
                    ReceiptMessage {
                        r#type: read,
                        timestamp: vec![100, 101]
                    }
                ),
                (
                    b.to_string(),
                    ReceiptMessage {
                        r#type: read,
                        timestamp: vec![50]
                    }
                ),
            ]
        );
    }

    #[test]
    fn typing_names_groups_by_identifier() {
        let t = typing(&format!("c:{U}"), true, 5).unwrap();
        assert_eq!(
            (t.timestamp, t.action, t.group_id),
            (Some(5), Some(typing_message::Action::Started as i32), None)
        );
        let g = typing(&group(), false, 6).unwrap();
        assert_eq!(g.action, Some(typing_message::Action::Stopped as i32));
        let id = g.group_id.unwrap();
        assert_eq!(id.len(), 32);
        assert_ne!(id, vec![7u8; 32], "the identifier, not the master key");
        assert_eq!(typing("c:nope", true, 1), None);
    }

    #[test]
    fn only_own_sent_recent_undeleted_messages_can_change() {
        let now = 10 * CHANGE_WINDOW_MS;
        let ok = mine("c:x", now - CHANGE_WINDOW_MS);
        assert_eq!(check_own_change(&ok, ME, now), Ok(()));
        let rejected = [
            Message {
                author: U.into(),
                ..ok.clone()
            },
            Message {
                sent_at: now - CHANGE_WINDOW_MS - 1,
                ..ok.clone()
            },
            Message {
                deleted: true,
                ..ok.clone()
            },
            Message {
                status: MessageStatus::Pending,
                ..ok.clone()
            },
            Message {
                status: MessageStatus::Failed,
                ..ok.clone()
            },
        ];
        for m in rejected {
            assert!(check_own_change(&m, ME, now).is_err(), "{m:?}");
        }
        for status in [MessageStatus::Delivered, MessageStatus::Read] {
            assert_eq!(check_own_change(&Message { status, ..ok.clone() }, ME, now), Ok(()));
        }
    }
}
