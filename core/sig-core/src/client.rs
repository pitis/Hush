use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use tokio::sync::mpsc;

use crate::actor::{self, Command};
use crate::error::CoreError;
use crate::events::{from_applied, CoreEvent, EventListener};
use crate::projection::db::MAX_ATTACHMENT;
use crate::projection::{
    Attachment, ChatEvent, Conversation, ConversationKind, LinkPreview, Message, MessageKey, MessageKind,
    MessageStatus, OutgoingFile, ProjectionDb, Quote, Reaction, RequestResponse, SearchHit,
};
use crate::send::{self, Job, SendJob};

/// Signal's limit for attachments in one message.
const MAX_ATTACHMENTS: usize = 32;

/// The app's handle on the core. Cheap to call from any thread; Signal work happens on the core
/// thread and reports back through the `EventListener`.
/// Keep the `EventListener` free of strong references back to the `Client`: the core thread lives
/// as long as the `Client` does.
#[derive(uniffi::Object)]
pub struct Client {
    commands: Mutex<Option<mpsc::UnboundedSender<Command>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    db: ProjectionDb,
    linked: Arc<AtomicBool>,
    own_aci: Arc<Mutex<Option<String>>>,
    listener: Arc<dyn EventListener>,
    data_dir: PathBuf,
    /// Last `sent_at` handed out: own messages need unique timestamps (they are their identity).
    last_sent_at: Mutex<i64>,
    /// How many storage syncs were asked for (tests can't see the core's commands).
    #[cfg(test)]
    storage_sync_requests: std::sync::atomic::AtomicUsize,
    /// The block and request jobs queued, by name (tests can't see the core's commands).
    #[cfg(test)]
    sent_jobs: Mutex<Vec<String>>,
}

impl Drop for Client {
    /// Stops the core thread and waits for it, so the data directory is free when this returns.
    fn drop(&mut self) {
        drop(self.commands.lock().ok().and_then(|mut c| c.take()));
        if let Some(t) = self.thread.lock().ok().and_then(|mut t| t.take()) {
            let _ = t.join();
        }
    }
}

impl Client {
    fn next_sent_at(&self) -> i64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let mut last = self.last_sent_at.lock().unwrap_or_else(|e| e.into_inner());
        *last = now.max(*last + 1);
        *last
    }

    /// A stored message, or why there is none.
    async fn stored(&self, key: &MessageKey) -> Result<Message, CoreError> {
        self.db.message(key).await?.ok_or(CoreError::InvalidInput {
            message: "unknown message".into(),
        })
    }

    /// A conversation id that is well-formed and in the projection.
    async fn known_conversation(&self, id: &str) -> Result<(), CoreError> {
        check_conversation(id)?;
        if !self.db.has_conversation(id).await? {
            return Err(CoreError::InvalidInput {
                message: format!("no such chat: {id}"),
            });
        }
        Ok(())
    }

    /// Queues a reaction, edit or delete of `conversation_id`'s message sent at `at`.
    fn send_mutation(
        &self,
        conversation_id: String,
        body: impl Into<presage::libsignal_service::content::ContentBody>,
        at: u64,
    ) -> Result<(), CoreError> {
        self.send(Command::Send(Job::Mutation {
            conversation_id,
            body: Box::new(body.into()),
            at,
        }))
    }

    async fn is_archived(&self, conversation_id: &str) -> Result<bool, CoreError> {
        Ok(self.db.chat_flags(conversation_id).await?.is_some_and(|f| f.archived))
    }

    /// Sending brought an archived chat back (the projection does that): the phone should know.
    async fn unarchived_by_sending(&self, conversation_id: &str, was_archived: bool) -> Result<(), CoreError> {
        if was_archived {
            self.db.set_archived(conversation_id, false).await?; // marks it for the sync
            self.request_storage_sync();
        }
        Ok(())
    }

    /// A chat setting changed here: the Storage Service sync writes it (debounced). If the core
    /// is closing, the dirty marks stay and the next start's sync writes them.
    fn request_storage_sync(&self) {
        #[cfg(test)]
        self.storage_sync_requests.fetch_add(1, Ordering::SeqCst);
        let _ = self.send(Command::StorageSync);
    }

    /// Queues a block or request job; if the core is closing it is dropped (the storage record
    /// still carries the change at the next start).
    fn queue(&self, job: Job) {
        #[cfg(test)]
        self.sent_jobs.lock().unwrap().push(match &job {
            Job::RequestResponse { response, .. } => format!("RequestResponse({response:?})"),
            Job::BlockedList => "BlockedList".into(),
            Job::ProfileKey { .. } => "ProfileKey".into(),
            Job::Receipts => "Receipts".into(),
            _ => "other".into(),
        });
        let _ = self.send(Command::Send(job));
    }

    /// A chat that can be blocked or deleted: known, and not Note to Self.
    async fn other_chat(&self, id: &str) -> Result<(), CoreError> {
        let me = self.own_aci().ok_or(CoreError::NotLinked)?;
        self.known_conversation(id).await?;
        if id == format!("c:{me}") {
            return Err(CoreError::InvalidInput {
                message: "Note to Self can't be blocked or deleted".into(),
            });
        }
        Ok(())
    }

    fn send(&self, cmd: Command) -> Result<(), CoreError> {
        let commands = self.commands.lock().map_err(|_| CoreError::Closed)?;
        commands
            .as_ref()
            .ok_or(CoreError::Closed)?
            .send(cmd)
            .map_err(|_| CoreError::Closed)
    }
}

#[uniffi::export]
impl Client {
    /// Opens (creating if needed) the data in `data_dir` with the 64-hex-char `db_key_hex`.
    #[uniffi::constructor]
    pub fn open(
        data_dir: String,
        db_key_hex: String,
        listener: Arc<dyn EventListener>,
    ) -> Result<Arc<Self>, CoreError> {
        crate::logging::init();
        let linked = Arc::new(AtomicBool::new(false));
        let own_aci = Arc::new(Mutex::new(None));
        let data_dir = PathBuf::from(data_dir);
        let h = actor::spawn(
            data_dir.clone(),
            db_key_hex,
            listener.clone(),
            linked.clone(),
            own_aci.clone(),
        )?;
        Ok(Arc::new(Self {
            commands: Mutex::new(Some(h.commands)),
            thread: Mutex::new(Some(h.thread)),
            db: h.projection,
            linked,
            own_aci,
            listener,
            data_dir,
            last_sent_at: Mutex::new(0),
            #[cfg(test)]
            storage_sync_requests: Default::default(),
            #[cfg(test)]
            sent_jobs: Default::default(),
        }))
    }

    /// Our own ACI once linked; own messages carry it as their author.
    pub fn own_aci(&self) -> Option<String> {
        self.own_aci.lock().ok().and_then(|a| a.clone())
    }

    pub fn is_linked(&self) -> bool {
        self.linked.load(Ordering::SeqCst)
    }

    /// Starts linking as a secondary device: emits `LinkUrl`, then `Linked`, or `Error` and
    /// `Connection(Unlinked)` (the code expires after 5 minutes). Call `start` after `Linked`.
    pub fn link(&self, device_name: String) -> Result<(), CoreError> {
        self.send(Command::Link { device_name })
    }

    /// Starts receiving (idempotent). Emits `Connection`, `MessageChanged`, `Ready`, …
    pub fn start(&self) -> Result<(), CoreError> {
        self.send(Command::Start)
    }

    /// Reconnects now: when the network comes back, or the user taps "Retry".
    pub fn reconnect(&self) -> Result<(), CoreError> {
        self.send(Command::Reconnect)
    }

    /// Starts a new secure session with the contact of a 1:1 chat (after messages from them
    /// couldn't be decrypted).
    pub fn reset_session(&self, conversation_id: String) -> Result<(), CoreError> {
        // presage resets sessions with an ACI only
        let aci = |id: &str| id.strip_prefix("c:").is_some_and(|rest| !rest.starts_with("PNI:"));
        if !matches!(send::recipient(&conversation_id), Some(send::Recipient::Direct(_))) || !aci(&conversation_id) {
            return Err(CoreError::InvalidInput {
                message: format!("not a 1:1 chat with a known contact: {conversation_id}"),
            });
        }
        self.send(Command::ResetSession { conversation_id })
    }

    /// Starts a voice call in a 1:1 chat (`CallChanged` events follow; the call window opens
    /// on `Outgoing`).
    pub fn start_call(&self, conversation_id: String) -> Result<(), CoreError> {
        if !self.is_linked() {
            return Err(CoreError::NotLinked);
        }
        let aci = conversation_id
            .strip_prefix("c:")
            .filter(|rest| !rest.starts_with("PNI:") && !rest.is_empty());
        let Some(peer_aci) = aci else {
            return Err(CoreError::InvalidInput {
                message: format!("not a 1:1 chat with a known contact: {conversation_id}"),
            });
        };
        self.send(Command::Call(crate::calls::actor::CallCommand::Start {
            peer_aci: peer_aci.to_owned(),
        }))
    }

    /// Accepts the ringing incoming call.
    pub fn accept_call(&self) -> Result<(), CoreError> {
        self.send(Command::Call(crate::calls::actor::CallCommand::Accept))
    }

    /// Hangs up the active call (or declines the ringing one).
    pub fn hangup_call(&self) -> Result<(), CoreError> {
        self.send(Command::Call(crate::calls::actor::CallCommand::Hangup))
    }

    /// Mutes or unmutes our microphone in the active call.
    pub fn set_call_muted(&self, muted: bool) -> Result<(), CoreError> {
        self.send(Command::Call(crate::calls::actor::CallCommand::SetMuted(muted)))
    }

    /// Switches the call microphone (an id from `call_audio_devices`).
    pub fn set_call_audio_input(&self, id: String) -> Result<(), CoreError> {
        self.send(Command::Call(crate::calls::actor::CallCommand::SetAudioInput(id)))
    }

    /// Switches the call speaker (an id from `call_audio_devices`).
    pub fn set_call_audio_output(&self, id: String) -> Result<(), CoreError> {
        self.send(Command::Call(crate::calls::actor::CallCommand::SetAudioOutput(id)))
    }

    /// "Continue without history": stops the history import (or clears a failed one), deletes
    /// the archive and its state, and starts receiving.
    pub fn cancel_history(&self) -> Result<(), CoreError> {
        self.send(Command::CancelHistory)
    }

    /// After a failed history transfer: picks the import up from where it stopped.
    pub fn retry_history(&self) -> Result<(), CoreError> {
        self.send(Command::Start)
    }

    /// Downloads an attachment that didn't download by itself (or failed). Reported through
    /// `AttachmentChanged`.
    pub fn download_attachment(&self, id: i64) -> Result<(), CoreError> {
        if !self.is_linked() {
            return Err(CoreError::NotLinked);
        }
        self.send(Command::Download(id))
    }

    /// The user is typing in a conversation (`started`), or stopped (sent, cleared the field, left
    /// the chat). Call it on every change of the field; typing stops by itself after `TYPING_IDLE`
    /// (10 s) without one. Nothing is sent in Note to Self, or unless the account's typing indicators
    /// setting is on.
    pub fn set_typing(&self, conversation_id: String, started: bool) -> Result<(), CoreError> {
        if !self.is_linked() {
            return Err(CoreError::NotLinked);
        }
        if send::recipient(&conversation_id).is_none() {
            return Err(CoreError::InvalidInput {
                message: format!("not a conversation: {conversation_id}"),
            });
        }
        if self.own_aci().is_some_and(|me| conversation_id == format!("c:{me}")) {
            return Ok(());
        }
        self.send(Command::Typing {
            conversation_id,
            started,
        })
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl Client {
    /// Shows `body` at once as a pending message and queues it for sending (it moves to Sent or
    /// Failed, reported through `MessageChanged`). Returns the new message's key.
    pub async fn send_text(
        &self,
        conversation_id: String,
        body: String,
        quote: Option<Quote>,
    ) -> Result<MessageKey, CoreError> {
        if body.trim().is_empty() {
            return Err(CoreError::InvalidInput {
                message: "empty message".into(),
            });
        }
        if crate::send::recipient(&conversation_id).is_none() {
            return Err(CoreError::InvalidInput {
                message: format!("not a conversation: {conversation_id}"),
            });
        }
        let author = self.own_aci().ok_or(CoreError::NotLinked)?;
        // the chat's timer; ours start disappearing when sent
        let expires_in = self.db.expire_timer(&conversation_id).await?;
        let mut message = Message {
            conversation_id: conversation_id.clone(),
            author,
            sent_at: self.next_sent_at(),
            received_at: 0,
            body: Some(body.clone()),
            is_outgoing: true,
            status: MessageStatus::Pending,
            edited: false,
            deleted: false,
            quote: quote.clone(),
            kind: MessageKind::Normal,
            expires_in,
            expires_at: None,
        };
        let was_archived = self.is_archived(&conversation_id).await?;
        // (author, sent_at) is the identity: on a collision (a phone-sent message in the same ms,
        // a clock that went back) take the next timestamp rather than dropping the message.
        for _ in 0..1000 {
            message.received_at = message.sent_at;
            message.expires_at = (expires_in > 0).then(|| message.sent_at + i64::from(expires_in) * 1000);
            if self.db.insert_message(&message).await? {
                break;
            }
            message.sent_at = self.next_sent_at();
        }
        self.unarchived_by_sending(&conversation_id, was_archived).await?;
        let key = message.key();
        self.listener.on_event(CoreEvent::MessageChanged {
            conversation_id: conversation_id.clone(),
            key: key.clone(),
        });
        let queued = self.send(Command::Send(Job::Text(SendJob {
            key: key.clone(),
            conversation_id,
            body: Some(body),
            quote,
            attachments: vec![],
        })));
        if let Err(e) = queued {
            self.db.set_failed(&key).await?;
            return Err(e);
        }
        Ok(key)
    }

    /// Re-queues a Failed message. Returns false if it isn't Failed.
    pub async fn retry(&self, key: MessageKey) -> Result<bool, CoreError> {
        if !self.db.set_pending(&key).await? {
            return Ok(false);
        }
        let m = self.db.message(&key).await?.ok_or(CoreError::InvalidInput {
            message: "unknown message".into(),
        })?;
        self.listener.on_event(CoreEvent::MessageChanged {
            conversation_id: m.conversation_id.clone(),
            key: key.clone(),
        });
        let attachments = self.db.attachments(&key).await?.iter().map(|a| a.id).collect();
        self.send(Command::Send(Job::Text(SendJob {
            key,
            conversation_id: m.conversation_id,
            body: m.body,
            quote: m.quote,
            attachments,
        })))?;
        Ok(true)
    }

    /// Sends files (with an optional caption) like `send_text`: shown at once as a pending
    /// message, with a copy of each file kept by Semaphore; uploaded, then sent.
    pub async fn send_attachments(
        &self,
        conversation_id: String,
        files: Vec<OutgoingFile>,
        caption: Option<String>,
        quote: Option<Quote>,
    ) -> Result<MessageKey, CoreError> {
        let invalid = |message: String| CoreError::InvalidInput { message };
        if files.is_empty() || files.len() > MAX_ATTACHMENTS {
            return Err(invalid(format!(
                "between 1 and {MAX_ATTACHMENTS} files can be sent at once"
            )));
        }
        for f in &files {
            let name = std::path::Path::new(&f.path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let size = std::fs::metadata(&f.path)
                .map_err(|e| invalid(format!("{name} can't be read: {e}")))?
                .len();
            if size > MAX_ATTACHMENT {
                return Err(invalid(format!("{name} is larger than 100 MB")));
            }
        }
        if crate::send::recipient(&conversation_id).is_none() {
            return Err(invalid(format!("not a conversation: {conversation_id}")));
        }
        let author = self.own_aci().ok_or(CoreError::NotLinked)?;
        // the chat's timer; ours start disappearing when sent
        let expires_in = self.db.expire_timer(&conversation_id).await?;
        let body = caption.map(|c| c.trim().to_string()).filter(|c| !c.is_empty());
        let mut message = Message {
            conversation_id: conversation_id.clone(),
            author,
            sent_at: self.next_sent_at(),
            received_at: 0,
            body: body.clone(),
            is_outgoing: true,
            status: MessageStatus::Pending,
            edited: false,
            deleted: false,
            quote: quote.clone(),
            kind: MessageKind::Normal,
            expires_in,
            expires_at: None,
        };
        let was_archived = self.is_archived(&conversation_id).await?;
        // as in send_text: a timestamp collision takes the next one
        for _ in 0..1000 {
            message.received_at = message.sent_at;
            message.expires_at = (expires_in > 0).then(|| message.sent_at + i64::from(expires_in) * 1000);
            if self.db.insert_outgoing(&message, &files).await? {
                break;
            }
            message.sent_at = self.next_sent_at();
        }
        self.unarchived_by_sending(&conversation_id, was_archived).await?;
        let key = message.key();
        self.listener.on_event(CoreEvent::MessageChanged {
            conversation_id: conversation_id.clone(),
            key: key.clone(),
        });
        let attachments = self.db.attachments(&key).await?.iter().map(|a| a.id).collect();
        let queued = self.send(Command::Send(Job::Text(SendJob {
            key: key.clone(),
            conversation_id,
            body,
            quote,
            attachments,
        })));
        if let Err(e) = queued {
            self.db.set_failed(&key).await?;
            return Err(e);
        }
        Ok(key)
    }

    /// Unlinks this Mac and removes all its Signal data (databases and files): after telling the
    /// server (`tell_server`; a `Network` error if it can't be reached, and nothing is removed),
    /// or only here (the phone already removed it). The core stops; open a new `Client` to link
    /// again. The app removes the database key.
    pub async fn unlink(&self, tell_server: bool) -> Result<(), CoreError> {
        let (done, stopped) = tokio::sync::oneshot::channel();
        self.send(Command::Unlink { tell_server, done })?;
        stopped
            .await
            .map_err(|_| CoreError::Closed)?
            .map_err(|message| CoreError::Network { message })?;
        // the core thread is ending: wait for it, so nothing writes the files again
        drop(self.commands.lock().ok().and_then(|mut c| c.take()));
        if let Some(thread) = self.thread.lock().ok().and_then(|mut t| t.take()) {
            if !matches!(tokio::task::spawn_blocking(move || thread.join()).await, Ok(Ok(()))) {
                tracing::error!("the core thread ended abnormally while unlinking");
            }
        }
        // this handle's own connection to the projection too, before its file goes
        self.db.close().await;
        let storage = |e: std::io::Error| CoreError::Storage { message: e.to_string() };
        // the projection first: a half-done wipe must not leave old messages next to a new link
        for db in ["projection.db", "presage.db"] {
            for suffix in ["", "-wal", "-shm"] {
                match std::fs::remove_file(self.data_dir.join(format!("{db}{suffix}"))) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(storage(e)),
                    _ => {}
                }
            }
        }
        match std::fs::remove_dir_all(self.data_dir.join("attachments")) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(storage(e)),
            _ => Ok(()),
        }
    }

    /// The microphones and speakers a call can use. Starts the audio engine if needed.
    pub async fn call_audio_devices(&self) -> Result<crate::calls::AudioDevices, CoreError> {
        if !self.is_linked() {
            return Err(CoreError::NotLinked);
        }
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.send(Command::Call(crate::calls::actor::CallCommand::ListAudioDevices {
            reply,
        }))?;
        rx.await.map_err(|_| CoreError::Closed)
    }

    /// A history import is waiting, downloading or importing: set right after linking when the
    /// phone offered history, cleared when it finishes or is abandoned. While it is, the app
    /// shows the progress screen (`HistoryProgress` events follow).
    pub async fn history_pending(&self) -> Result<bool, CoreError> {
        Ok(self.db.history_state().await?.is_some())
    }

    /// Relink to transfer history: stops the core and removes presage's store (this Mac becomes
    /// a new linked device) but keeps every shown conversation and file. Open a new `Client`
    /// and `link` afterwards; the import then merges the phone's history in.
    pub async fn relink(&self) -> Result<(), CoreError> {
        let (done, stopped) = tokio::sync::oneshot::channel();
        self.send(Command::Relink { done })?;
        stopped.await.map_err(|_| CoreError::Closed)?;
        // the core thread is ending: wait for it, so nothing writes presage.db again
        drop(self.commands.lock().ok().and_then(|mut c| c.take()));
        if let Some(thread) = self.thread.lock().ok().and_then(|mut t| t.take()) {
            if !matches!(tokio::task::spawn_blocking(move || thread.join()).await, Ok(Ok(()))) {
                tracing::error!("the core thread ended abnormally while relinking");
            }
        }
        // a stale import (its key belongs to the old link) must not resume against the new one
        self.db.clear_history_state().await?;
        self.db.close().await;
        let storage = |e: std::io::Error| CoreError::Storage { message: e.to_string() };
        for suffix in ["", "-wal", "-shm"] {
            match std::fs::remove_file(self.data_dir.join(format!("presage.db{suffix}"))) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(storage(e)),
                _ => {}
            }
        }
        match std::fs::remove_file(self.data_dir.join("history.tmp")) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(storage(e)),
            _ => Ok(()),
        }
    }

    /// A chat's events (e.g. undecryptable messages) from `since` (ms) on.
    pub async fn chat_events(&self, conversation_id: String, since: i64) -> Result<Vec<ChatEvent>, CoreError> {
        Ok(self.db.chat_events(&conversation_id, since).await?)
    }

    /// A message's files, in order.
    pub async fn attachments(&self, key: MessageKey) -> Result<Vec<Attachment>, CoreError> {
        Ok(self.db.attachments(&key).await?)
    }

    pub async fn attachment(&self, id: i64) -> Result<Option<Attachment>, CoreError> {
        Ok(self.db.attachment(id).await?)
    }

    pub async fn link_preview(&self, key: MessageKey) -> Result<Option<LinkPreview>, CoreError> {
        Ok(self.db.link_preview(&key).await?)
    }

    /// Reacts to a message with `emoji`, or takes back our reaction `emoji` (`remove`). It shows
    /// here once sent (`MessageChanged`); a failed send is reported as `Error`.
    pub async fn react(&self, target: MessageKey, emoji: String, remove: bool) -> Result<(), CoreError> {
        self.own_aci().ok_or(CoreError::NotLinked)?;
        if emoji.is_empty() {
            return Err(CoreError::InvalidInput {
                message: "no emoji".into(),
            });
        }
        let m = self.stored(&target).await?;
        if m.deleted {
            return Err(CoreError::InvalidInput {
                message: "the message was deleted".into(),
            });
        }
        let at = self.next_sent_at() as u64;
        let body = send::reaction(&m.conversation_id, &target, emoji, remove, at);
        self.send_mutation(m.conversation_id, body, at)
    }

    /// Replaces the text of our own message (sent, under 24 hours old). It shows here once sent.
    pub async fn edit(&self, key: MessageKey, new_body: String) -> Result<(), CoreError> {
        let me = self.own_aci().ok_or(CoreError::NotLinked)?;
        if new_body.trim().is_empty() {
            return Err(CoreError::InvalidInput {
                message: "empty message".into(),
            });
        }
        let m = self.stored(&key).await?;
        let at = self.next_sent_at();
        send::check_own_change(&m, &me, at).map_err(|message| CoreError::InvalidInput { message })?;
        let body = send::edit(&m, new_body, at as u64);
        self.send_mutation(m.conversation_id, body, at as u64)
    }

    /// Deletes our own message (sent, under 24 hours old) for everyone. It shows here once sent.
    pub async fn delete_for_everyone(&self, key: MessageKey) -> Result<(), CoreError> {
        let me = self.own_aci().ok_or(CoreError::NotLinked)?;
        let m = self.stored(&key).await?;
        let at = self.next_sent_at();
        send::check_own_change(&m, &me, at).map_err(|message| CoreError::InvalidInput { message })?;
        let body = send::delete(&m.conversation_id, m.sent_at, at as u64);
        self.send_mutation(m.conversation_id, body, at as u64)
    }

    /// Marks the conversation's unread messages read here at once, then sends read receipts to
    /// their senders (only if the account's read receipts setting is on) and tells our phone. The receipts are stored first and go out as soon as
    /// sending runs (retried until sent). Returns how many were unread.
    pub async fn mark_read(&self, conversation_id: String) -> Result<u32, CoreError> {
        let me = self.own_aci().ok_or(CoreError::NotLinked)?;
        if self.db.clear_marked_unread(&conversation_id).await? {
            self.listener.on_event(CoreEvent::ConversationsChanged);
            self.request_storage_sync();
        }
        let targets = self.db.unread(&conversation_id).await?;
        if targets.is_empty() {
            return Ok(0);
        }
        self.db.queue_receipts(&targets).await?;
        for op in send::own_ops(&me, &conversation_id, send::read_sync(&targets).into()) {
            for applied in self.db.apply(&op).await? {
                self.listener.on_event(from_applied(applied));
            }
        }
        // if the core is closing, the stored receipts go out on the next start
        let _ = self.send(Command::Send(Job::Receipts));
        Ok(targets.len() as u32)
    }

    /// Archives a chat (out of the list, behind "Archived") or brings it back.
    pub async fn set_archived(&self, conversation_id: String, archived: bool) -> Result<(), CoreError> {
        self.known_conversation(&conversation_id).await?;
        self.db.set_archived(&conversation_id, archived).await?;
        self.listener.on_event(CoreEvent::ConversationsChanged);
        self.request_storage_sync();
        Ok(())
    }

    /// Pins a chat at the top of the list (after the pinned ones, at most four, like the phone),
    /// or unpins it.
    pub async fn set_pinned(&self, conversation_id: String, pinned: bool) -> Result<(), CoreError> {
        self.known_conversation(&conversation_id).await?;
        if !self.db.set_pinned(&conversation_id, pinned).await? {
            return Err(CoreError::InvalidInput {
                message: format!("up to {} chats can be pinned", ProjectionDb::MAX_PINNED),
            });
        }
        self.listener.on_event(CoreEvent::ConversationsChanged);
        self.request_storage_sync();
        Ok(())
    }

    /// Mutes a chat's notifications until `until_ms` (ms since the epoch; `i64::MAX` for good),
    /// or unmutes it (0).
    pub async fn set_mute_until(&self, conversation_id: String, until_ms: i64) -> Result<(), CoreError> {
        self.known_conversation(&conversation_id).await?;
        if until_ms < 0 {
            return Err(CoreError::InvalidInput {
                message: "a mute ends at a time, or never".into(),
            });
        }
        self.db.set_mute_until(&conversation_id, until_ms).await?;
        self.listener.on_event(CoreEvent::ConversationsChanged);
        self.request_storage_sync();
        Ok(())
    }

    /// Shows a chat as unread although its messages are read (until it is opened), or not.
    pub async fn set_marked_unread(&self, conversation_id: String, marked: bool) -> Result<(), CoreError> {
        self.known_conversation(&conversation_id).await?;
        self.db.set_marked_unread(&conversation_id, marked).await?;
        self.listener.on_event(CoreEvent::ConversationsChanged);
        self.request_storage_sync();
        Ok(())
    }

    /// Blocks a person or group: nothing they send shows from now on (a group is not left).
    /// Our other devices and the storage record follow.
    pub async fn block(&self, conversation_id: String) -> Result<(), CoreError> {
        self.other_chat(&conversation_id).await?;
        self.db.block(&conversation_id).await?;
        self.listener.on_event(CoreEvent::ConversationsChanged);
        self.request_storage_sync();
        self.queue(Job::RequestResponse {
            conversation_id,
            response: RequestResponse::Block,
        });
        self.queue(Job::BlockedList);
        Ok(())
    }

    /// Accepts a message request, or unblocks (the same on Signal): a contact gets our profile
    /// key, read receipts held back go out, and our other devices and the storage record follow.
    pub async fn accept_request(&self, conversation_id: String) -> Result<(), CoreError> {
        self.other_chat(&conversation_id).await?;
        let was_blocked = self.db.accept(&conversation_id).await?;
        self.listener.on_event(CoreEvent::ConversationsChanged);
        self.request_storage_sync();
        self.queue(Job::RequestResponse {
            conversation_id: conversation_id.clone(),
            response: RequestResponse::Accept,
        });
        if was_blocked {
            self.queue(Job::BlockedList);
        }
        if ConversationKind::from_id(&conversation_id) == Some(ConversationKind::Direct) {
            self.queue(Job::ProfileKey { conversation_id });
        }
        self.queue(Job::Receipts);
        Ok(())
    }

    /// Deletes a chat here (messages, files, and presage's copies); a contact's is deleted on our
    /// other devices too. It comes back with the next message, as a request.
    pub async fn delete_chat(&self, conversation_id: String) -> Result<(), CoreError> {
        self.other_chat(&conversation_id).await?;
        self.db.delete_chat(&conversation_id).await?;
        self.listener.on_event(CoreEvent::ConversationsChanged);
        self.request_storage_sync();
        self.queue(Job::RequestResponse {
            conversation_id,
            response: RequestResponse::Delete,
        });
        Ok(())
    }

    /// Whether chat settings changed here are written to the Storage Service (the phone sees
    /// them). On unless turned off; the phone's changes are read either way.
    pub async fn storage_writes(&self) -> Result<bool, CoreError> {
        Ok(self.db.storage_writes().await?)
    }

    /// Turns writing chat settings to the Storage Service on or off. Turning it on writes what
    /// changed meanwhile.
    pub async fn set_storage_writes(&self, on: bool) -> Result<(), CoreError> {
        self.db.set_storage_writes(on).await?;
        if on {
            self.request_storage_sync();
        }
        Ok(())
    }

    /// Whether the chat's notifications are off right now. Asked by the app for each
    /// notification, so a mute set a moment ago counts even before the app's list caught up.
    pub async fn is_muted(&self, conversation_id: String) -> Result<bool, CoreError> {
        Ok(self.db.is_muted(&conversation_id, now_ms()).await?)
    }

    /// The stored name of any conversation or contact (e.g. a group member's `c:<aci>`).
    pub async fn title(&self, conversation_id: String) -> Result<Option<String>, CoreError> {
        Ok(self.db.title(&conversation_id).await?)
    }

    pub async fn conversations(&self) -> Result<Vec<Conversation>, CoreError> {
        Ok(self.db.conversations().await?)
    }

    pub async fn messages(
        &self,
        conversation_id: String,
        before: Option<MessageKey>,
        limit: u32,
    ) -> Result<Vec<Message>, CoreError> {
        Ok(self.db.messages(&conversation_id, before, limit).await?)
    }

    /// Messages matching `query` (see `ProjectionDb::search`), in one conversation or all.
    pub async fn search(
        &self,
        query: String,
        conversation_id: Option<String>,
        limit: u32,
    ) -> Result<Vec<SearchHit>, CoreError> {
        Ok(self.db.search(&query, conversation_id.as_deref(), limit).await?)
    }

    /// How many messages of a conversation are newer than `key`.
    pub async fn count_newer(&self, conversation_id: String, key: MessageKey) -> Result<u32, CoreError> {
        Ok(self.db.count_newer(&conversation_id, &key).await?)
    }

    pub async fn message(&self, key: MessageKey) -> Result<Option<Message>, CoreError> {
        Ok(self.db.message(&key).await?)
    }

    pub async fn reactions(&self, key: MessageKey) -> Result<Vec<Reaction>, CoreError> {
        Ok(self.db.reactions(&key).await?)
    }
}

/// Milliseconds since the epoch (the unit of chat mutes).
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Rejects anything that isn't a conversation id (`c:<aci>` or `g:<master key>`).
fn check_conversation(id: &str) -> Result<(), CoreError> {
    if send::recipient(id).is_none() {
        return Err(CoreError::InvalidInput {
            message: format!("not a conversation: {id}"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{ConnectionState, CoreEvent};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    struct Sink(Mutex<Vec<CoreEvent>>);
    impl EventListener for Sink {
        fn on_event(&self, e: CoreEvent) {
            self.0.lock().unwrap().push(e);
        }
    }

    struct TempDir(std::path::PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_dir(tag: &str) -> TempDir {
        let d = std::env::temp_dir().join(format!("semaphore-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        TempDir(d)
    }

    fn wait_for(sink: &Sink, pred: impl Fn(&CoreEvent) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if sink.0.lock().unwrap().iter().any(&pred) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn fresh_dir_is_unlinked_and_queries_work_from_another_runtime() {
        let dir = temp_dir("client-fresh");
        let sink = Arc::new(Sink(Mutex::new(vec![])));
        let client = Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink.clone()).unwrap();
        assert!(!client.is_linked());
        // the app calls these from UniFFI's runtime, not the core thread's
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            assert!(client.conversations().await.unwrap().is_empty());
            assert!(client.messages("c:x".into(), None, 10).await.unwrap().is_empty());
            assert!(client.search("hi".into(), None, 10).await.unwrap().is_empty());
        });
        assert!(dir.0.join("presage.db").exists() && dir.0.join("projection.db").exists());
    }

    #[test]
    fn start_while_unlinked_reports_unlinked() {
        let dir = temp_dir("client-start");
        let sink = Arc::new(Sink(Mutex::new(vec![])));
        let client = Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink.clone()).unwrap();
        client.start().unwrap();
        assert!(wait_for(&sink, |e| *e
            == CoreEvent::Connection {
                state: ConnectionState::Unlinked
            }));
    }

    /// Relinking makes this Mac a new device but keeps what it shows: presage's store goes, the
    /// projection stays, and the client is closed (the app opens a fresh one to link).
    #[test]
    fn relink_keeps_the_projection_and_drops_presage() {
        let dir = temp_dir("client-relink");
        let sink = Arc::new(Sink(Mutex::new(vec![])));
        let client = Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink.clone()).unwrap();
        std::fs::write(dir.0.join("history.tmp"), b"stale").unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            client.relink().await.unwrap();
            assert!(matches!(client.relink().await, Err(CoreError::Closed)), "closed");
        });
        assert!(!dir.0.join("presage.db").exists(), "presage's store is gone");
        assert!(!dir.0.join("history.tmp").exists(), "stale archive gone");
        assert!(dir.0.join("projection.db").exists(), "conversations stay");
        // the data directory is free again: a fresh client can link anew
        drop(client);
        assert!(Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink).is_ok());
    }

    #[test]
    fn history_pending_only_with_a_state_row() {
        let dir = temp_dir("client-pending");
        let sink = Arc::new(Sink(Mutex::new(vec![])));
        let client = Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink).unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            assert!(!client.history_pending().await.unwrap());
            client
                .db
                .set_history_state(&crate::projection::HistoryImportState {
                    stage: crate::projection::HistoryDbStage::Waiting,
                    key: vec![1; 32],
                    aci: "22222222-2222-2222-2222-222222222222".into(),
                    cdn: None,
                    cdn_key: None,
                    bytes_done: 0,
                    total_bytes: None,
                })
                .await
                .unwrap();
            assert!(client.history_pending().await.unwrap());
            client.cancel_history().unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while client.history_pending().await.unwrap() && std::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert!(!client.history_pending().await.unwrap(), "cancel clears it");
        });
    }

    /// Calls need a link and a real 1:1 chat; the engine (and the audio stack) never starts
    /// for an invalid ask.
    #[test]
    fn calls_need_a_link_and_a_direct_chat() {
        let dir = temp_dir("client-calls");
        let sink = Arc::new(Sink(Mutex::new(vec![])));
        let client = Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink).unwrap();
        assert!(matches!(client.start_call("c:alice".into()), Err(CoreError::NotLinked)));
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            assert!(matches!(client.call_audio_devices().await, Err(CoreError::NotLinked)));
        });
    }

    #[test]
    fn start_call_rejects_groups_and_pni_chats() {
        let dir = temp_dir("client-call-ids");
        let sink = Arc::new(Sink(Mutex::new(vec![])));
        let client = Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink).unwrap();
        // linked-only guard comes first, so force the id checks by faking the link bit
        client.linked.store(true, std::sync::atomic::Ordering::SeqCst);
        for bad in ["g:0011", "c:PNI:22222222-2222-2222-2222-222222222222", "c:", "alice"] {
            assert!(
                matches!(client.start_call(bad.into()), Err(CoreError::InvalidInput { .. })),
                "{bad}"
            );
        }
    }

    #[test]
    fn wrong_key_fails_to_open() {
        let dir = temp_dir("client-key");
        let sink = Arc::new(Sink(Mutex::new(vec![])));
        // dropping the client waits for the core thread, so the files are free right away
        drop(Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink.clone()).unwrap());
        assert!(Client::open(dir.0.to_string_lossy().into(), "cd".repeat(32), sink.clone()).is_err());
        assert!(Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink).is_ok());
    }

    #[test]
    fn a_second_client_on_the_same_data_is_refused() {
        let dir = temp_dir("client-lock");
        let sink = Arc::new(Sink(Mutex::new(vec![])));
        let first = Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink.clone()).unwrap();
        let second = Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink.clone());
        assert!(matches!(second, Err(CoreError::AlreadyOpen)));
        drop(first);
        assert!(Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink).is_ok());
    }

    #[test]
    fn unreadable_registration_fails_open_instead_of_looking_unlinked() {
        let dir = temp_dir("client-corrupt");
        let sink = Arc::new(Sink(Mutex::new(vec![])));
        drop(Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink.clone()).unwrap());
        // corrupt the stored registration the way a bad presage upgrade or disk error could
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let opts = crate::actor::sqlite_options(&dir.0.join("presage.db"), &"ab".repeat(32)).unwrap();
            let mut conn = sqlx::ConnectOptions::connect(&opts).await.unwrap();
            sqlx::query("INSERT INTO kv(key, value) VALUES ('registration', x'00')")
                .execute(&mut conn)
                .await
                .unwrap();
        });
        let err = Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink)
            .err()
            .expect("must fail");
        assert!(matches!(err, CoreError::Storage { .. }), "{err}");
    }

    #[test]
    fn sending_needs_a_link_and_valid_input() {
        let dir = temp_dir("client-send");
        let sink = Arc::new(Sink(Mutex::new(vec![])));
        let client = Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let to = "c:22222222-2222-2222-2222-222222222222".to_string();
            assert!(matches!(
                client.send_text(to.clone(), "  ".into(), None).await,
                Err(CoreError::InvalidInput { .. })
            ));
            assert!(matches!(
                client.send_text("c:nope".into(), "hi".into(), None).await,
                Err(CoreError::InvalidInput { .. })
            ));
            assert!(matches!(
                client.send_text(to, "hi".into(), None).await,
                Err(CoreError::NotLinked)
            ));
        });
        assert_eq!(client.own_aci(), None);
    }

    #[test]
    fn own_message_timestamps_are_unique_and_increasing() {
        let dir = temp_dir("client-ts");
        let sink = Arc::new(Sink(Mutex::new(vec![])));
        let client = Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink).unwrap();
        let stamps: Vec<i64> = (0..100).map(|_| client.next_sent_at()).collect();
        assert!(stamps.windows(2).all(|w| w[1] > w[0]));
    }

    const ME: &str = "11111111-1111-1111-1111-111111111111";
    const ALICE: &str = "22222222-2222-2222-2222-222222222222";

    /// A client that believes it is linked as `ME` (nothing is sent: receiving never starts).
    fn as_me(tag: &str) -> (TempDir, Arc<Sink>, Arc<Client>, tokio::runtime::Runtime) {
        let dir = temp_dir(tag);
        let sink = Arc::new(Sink(Mutex::new(vec![])));
        let client = Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink.clone()).unwrap();
        *client.own_aci.lock().unwrap() = Some(ME.into());
        client.linked.store(true, Ordering::SeqCst);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        (dir, sink, client, rt)
    }

    fn stored(author: &str, sent_at: i64) -> Message {
        Message {
            conversation_id: format!("c:{ALICE}"),
            author: author.into(),
            sent_at,
            received_at: sent_at,
            body: Some("x".into()),
            is_outgoing: author == ME,
            status: if author == ME {
                MessageStatus::Sent
            } else {
                MessageStatus::Delivered
            },
            edited: false,
            deleted: false,
            quote: None,
            kind: crate::projection::MessageKind::Normal,
            expires_in: 0,
            expires_at: None,
        }
    }

    fn invalid<T: std::fmt::Debug>(r: Result<T, CoreError>) -> bool {
        matches!(r, Err(CoreError::InvalidInput { .. }))
    }

    #[test]
    fn changes_need_a_link() {
        let dir = temp_dir("client-changes-unlinked");
        let sink = Arc::new(Sink(Mutex::new(vec![])));
        let client = Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let k = MessageKey {
            author: ALICE.into(),
            sent_at: 1,
        };
        let conv = format!("c:{ALICE}");
        rt.block_on(async {
            let not_linked = |r: Result<(), CoreError>| matches!(r, Err(CoreError::NotLinked));
            assert!(not_linked(client.react(k.clone(), "👍".into(), false).await));
            assert!(not_linked(client.edit(k.clone(), "new".into()).await));
            assert!(not_linked(client.delete_for_everyone(k).await));
            assert!(matches!(
                client.mark_read(conv.clone()).await,
                Err(CoreError::NotLinked)
            ));
        });
        assert!(matches!(client.set_typing(conv, true), Err(CoreError::NotLinked)));
    }

    #[test]
    fn edit_and_delete_only_own_recent_messages() {
        let (_dir, _sink, client, rt) = as_me("client-edit-rules");
        let now = client.next_sent_at();
        rt.block_on(async {
            for m in [
                stored(ME, now - 1000),
                stored(ALICE, now - 900),
                stored(ME, now - 25 * 3600 * 1000),
            ] {
                client.db.insert_message(&m).await.unwrap();
            }
            let key = |author: &str, sent_at| MessageKey {
                author: author.into(),
                sent_at,
            };
            // someone else's, too old, unknown, or no text
            assert!(invalid(client.edit(key(ALICE, now - 900), "new".into()).await));
            assert!(invalid(client.delete_for_everyone(key(ALICE, now - 900)).await));
            let old = key(ME, now - 25 * 3600 * 1000);
            assert!(invalid(client.edit(old.clone(), "new".into()).await));
            assert!(invalid(client.delete_for_everyone(old).await));
            assert!(invalid(client.edit(key(ME, now - 500), "new".into()).await));
            assert!(invalid(client.edit(key(ME, now - 1000), " ".into()).await));
            // own and recent: queued (shown once sent)
            client.edit(key(ME, now - 1000), "new".into()).await.unwrap();
            client.delete_for_everyone(key(ME, now - 1000)).await.unwrap();
            assert_eq!(
                client
                    .message(key(ME, now - 1000))
                    .await
                    .unwrap()
                    .unwrap()
                    .body
                    .as_deref(),
                Some("x")
            );
        });
    }

    #[test]
    fn reactions_need_an_emoji_and_a_message_that_is_still_there() {
        let (_dir, _sink, client, rt) = as_me("client-react");
        rt.block_on(async {
            let m = stored(ALICE, 10);
            client.db.insert_message(&m).await.unwrap();
            assert!(invalid(client.react(m.key(), "".into(), false).await));
            assert!(invalid(
                client
                    .react(
                        MessageKey {
                            author: ALICE.into(),
                            sent_at: 11
                        },
                        "👍".into(),
                        false
                    )
                    .await
            ));
            client.react(m.key(), "👍".into(), false).await.unwrap();
            client
                .db
                .apply(&crate::projection::Op::Delete { target: m.key() })
                .await
                .unwrap();
            assert!(invalid(client.react(m.key(), "👍".into(), false).await));
        });
    }

    #[test]
    fn mark_read_clears_unread_at_once() {
        let (_dir, sink, client, rt) = as_me("client-mark-read");
        let conv = format!("c:{ALICE}");
        rt.block_on(async {
            for m in [stored(ALICE, 1), stored(ME, 2), stored(ALICE, 3)] {
                client.db.insert_message(&m).await.unwrap();
            }
            assert_eq!(client.conversations().await.unwrap()[0].unread_count, 2);
            assert_eq!(client.mark_read(conv.clone()).await.unwrap(), 2);
            // stored until sent, so a quit or a failed send can't lose them
            assert_eq!(
                client.db.unsynced_reads().await.unwrap(),
                vec![
                    MessageKey {
                        author: ALICE.into(),
                        sent_at: 1
                    },
                    MessageKey {
                        author: ALICE.into(),
                        sent_at: 3
                    }
                ]
            );
            assert_eq!(client.conversations().await.unwrap()[0].unread_count, 0);
            for t in [1, 3] {
                let key = MessageKey {
                    author: ALICE.into(),
                    sent_at: t,
                };
                assert_eq!(client.message(key).await.unwrap().unwrap().status, MessageStatus::Read);
            }
            assert_eq!(client.mark_read(conv.clone()).await.unwrap(), 0);
        });
        let changed = sink
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, CoreEvent::MessageChanged { conversation_id, .. } if *conversation_id == conv))
            .count();
        assert_eq!(changed, 2);
    }

    fn announced(sink: &Sink, e: fn(&CoreEvent) -> bool) -> usize {
        sink.0.lock().unwrap().iter().filter(|x| e(x)).count()
    }

    #[test]
    fn chat_flags_announce_the_list_and_refuse_bad_ids() {
        let (_dir, sink, client, rt) = as_me("client-flags");
        let conv = format!("c:{ALICE}");
        rt.block_on(async {
            client.db.insert_message(&stored(ALICE, 1)).await.unwrap();
            client.set_archived(conv.clone(), true).await.unwrap();
            client.set_pinned(conv.clone(), true).await.unwrap();
            client.set_mute_until(conv.clone(), i64::MAX).await.unwrap();
            client.set_marked_unread(conv.clone(), true).await.unwrap();
            let c = &client.conversations().await.unwrap()[0];
            assert_eq!(
                (c.archived, c.pinned_order, c.mute_until, c.marked_unread),
                (false, Some(1), i64::MAX, true)
            );
            assert!(invalid(client.set_archived("nope".into(), true).await));
            assert!(invalid(client.set_mute_until(conv.clone(), -1).await));
        });
        assert_eq!(announced(&sink, |e| matches!(e, CoreEvent::ConversationsChanged)), 4);
    }

    fn jobs(client: &Client) -> Vec<String> {
        std::mem::take(&mut *client.sent_jobs.lock().unwrap())
    }

    #[test]
    fn blocking_and_answering_requests_tell_the_phone_and_the_contact() {
        let (_dir, sink, client, rt) = as_me("client-block");
        let conv = format!("c:{ALICE}");
        let requests = || client.storage_sync_requests.load(Ordering::SeqCst);
        rt.block_on(async {
            client.db.insert_message(&stored(ALICE, 1)).await.unwrap();
            client.block(conv.clone()).await.unwrap();
            assert!(client.conversations().await.unwrap()[0].blocked);
            assert_eq!(jobs(&client), ["RequestResponse(Block)", "BlockedList"]);
            client.accept_request(conv.clone()).await.unwrap();
            let c = &client.conversations().await.unwrap()[0];
            assert!(!c.blocked && c.whitelisted);
            assert_eq!(
                jobs(&client),
                ["RequestResponse(Accept)", "BlockedList", "ProfileKey", "Receipts"],
                "unblocking is accepting; the block list changed too"
            );
            client.accept_request(conv.clone()).await.unwrap();
            assert_eq!(
                jobs(&client),
                ["RequestResponse(Accept)", "ProfileKey", "Receipts"],
                "the block list didn't change"
            );
            client.delete_chat(conv.clone()).await.unwrap();
            assert!(client.conversations().await.unwrap().is_empty());
            assert_eq!(jobs(&client), ["RequestResponse(Delete)"]);
            assert_eq!(requests(), 4, "each one goes to the storage record too");
            // Note to Self and unknown chats can't be blocked or deleted
            client
                .db
                .insert_message(&Message {
                    conversation_id: format!("c:{ME}"),
                    ..stored(ME, 2)
                })
                .await
                .unwrap();
            assert!(invalid(client.block(format!("c:{ME}")).await));
            assert!(invalid(client.delete_chat(format!("c:{ME}")).await));
            assert!(invalid(client.block("c:nope".into()).await));
            assert!(jobs(&client).is_empty());
        });
        assert_eq!(announced(&sink, |e| matches!(e, CoreEvent::ConversationsChanged)), 4);
    }

    #[test]
    fn a_groups_request_answers_go_without_a_profile_key() {
        let (_dir, _sink, client, rt) = as_me("client-block-group");
        let group = format!("g:{}", "ab".repeat(32));
        rt.block_on(async {
            client
                .db
                .insert_message(&Message {
                    conversation_id: group.clone(),
                    ..stored(ALICE, 1)
                })
                .await
                .unwrap();
            client.accept_request(group.clone()).await.unwrap();
            assert_eq!(jobs(&client), ["RequestResponse(Accept)", "Receipts"]);
            client.block(group.clone()).await.unwrap();
            assert_eq!(jobs(&client), ["RequestResponse(Block)", "BlockedList"]);
        });
    }

    #[test]
    fn a_fifth_pin_is_refused() {
        let (_dir, _sink, client, rt) = as_me("client-pins");
        let chat = |n: u8| format!("c:00000000-0000-4000-8000-00000000000{n}");
        rt.block_on(async {
            for n in 1..=5 {
                let m = Message {
                    conversation_id: chat(n),
                    ..stored(ALICE, i64::from(n))
                };
                client.db.insert_message(&m).await.unwrap();
            }
            for n in 1..=4 {
                client.set_pinned(chat(n), true).await.unwrap();
            }
            assert!(invalid(client.set_pinned(chat(5), true).await));
        });
    }

    #[test]
    fn mark_read_clears_marked_unread_even_without_unread_messages() {
        let (_dir, sink, client, rt) = as_me("client-marked-unread");
        let conv = format!("c:{ALICE}");
        rt.block_on(async {
            let seen = Message {
                status: MessageStatus::Read,
                ..stored(ALICE, 1)
            };
            client.db.insert_message(&seen).await.unwrap();
            client.set_marked_unread(conv.clone(), true).await.unwrap();
            assert_eq!(client.mark_read(conv.clone()).await.unwrap(), 0);
            assert!(!client.conversations().await.unwrap()[0].marked_unread);
            // already clear: nothing to announce
            assert_eq!(client.mark_read(conv.clone()).await.unwrap(), 0);
        });
        assert_eq!(announced(&sink, |e| matches!(e, CoreEvent::ConversationsChanged)), 2);
    }

    #[test]
    fn mark_read_clears_the_mark_and_the_messages_together() {
        let (_dir, _sink, client, rt) = as_me("client-marked-and-unread");
        let conv = format!("c:{ALICE}");
        rt.block_on(async {
            client.db.insert_message(&stored(ALICE, 1)).await.unwrap();
            client.set_marked_unread(conv.clone(), true).await.unwrap();
            assert_eq!(client.mark_read(conv.clone()).await.unwrap(), 1);
            let c = &client.conversations().await.unwrap()[0];
            assert_eq!((c.unread_count, c.marked_unread), (0, false));
        });
    }

    #[test]
    fn a_chat_that_is_not_there_cannot_be_flagged() {
        let (_dir, sink, client, rt) = as_me("client-unknown-chat");
        let nobody = "c:00000000-0000-4000-8000-000000000009".to_string();
        rt.block_on(async {
            client.db.insert_message(&stored(ALICE, 1)).await.unwrap();
            let refused = client.set_pinned(nobody.clone(), true).await;
            assert!(invalid(refused), "an unknown chat is not the pin limit");
            assert!(invalid(client.set_archived(nobody.clone(), true).await));
            assert!(invalid(client.set_mute_until(nobody.clone(), 1).await));
            assert!(invalid(client.set_marked_unread(nobody.clone(), true).await));
        });
        assert_eq!(announced(&sink, |e| matches!(e, CoreEvent::ConversationsChanged)), 0);
    }

    #[test]
    fn the_app_can_ask_whether_a_chat_is_muted_now() {
        let (_dir, _sink, client, rt) = as_me("client-is-muted");
        let conv = format!("c:{ALICE}");
        rt.block_on(async {
            client.db.insert_message(&stored(ALICE, 1)).await.unwrap();
            assert!(!client.is_muted(conv.clone()).await.unwrap());
            client.set_mute_until(conv.clone(), i64::MAX).await.unwrap();
            assert!(client.is_muted(conv.clone()).await.unwrap());
            client.set_mute_until(conv.clone(), 1).await.unwrap(); // long over
            assert!(!client.is_muted(conv.clone()).await.unwrap());
        });
    }

    #[test]
    fn flag_changes_ask_for_a_storage_sync() {
        let (_dir, _sink, client, rt) = as_me("client-storage-sync");
        let conv = format!("c:{ALICE}");
        let requests = || client.storage_sync_requests.load(Ordering::SeqCst);
        rt.block_on(async {
            client.db.insert_message(&stored(ALICE, 1)).await.unwrap();
            client.set_archived(conv.clone(), true).await.unwrap();
            client.set_pinned(conv.clone(), true).await.unwrap();
            client.set_mute_until(conv.clone(), 1).await.unwrap();
            client.set_marked_unread(conv.clone(), true).await.unwrap();
            assert_eq!(requests(), 4);
            // opening the chat clears the mark: the phone should know that too
            client.mark_read(conv.clone()).await.unwrap();
            assert_eq!(requests(), 5);
            client.mark_read(conv.clone()).await.unwrap(); // nothing to clear this time
            assert_eq!(requests(), 5);
            // a refused change asks for nothing
            assert!(invalid(client.set_pinned("c:nope".into(), true).await));
            assert_eq!(requests(), 5);
        });
    }

    #[test]
    fn sending_in_an_archived_chat_unarchives_it_on_the_phone_too() {
        let (_dir, _sink, client, rt) = as_me("client-send-unarchives");
        let conv = format!("c:{ALICE}");
        let requests = || client.storage_sync_requests.load(Ordering::SeqCst);
        rt.block_on(async {
            client.db.insert_message(&stored(ALICE, 1)).await.unwrap();
            client.send_text(conv.clone(), "hi".into(), None).await.unwrap();
            assert_eq!(requests(), 0, "not archived: nothing for the phone");
            client.set_archived(conv.clone(), true).await.unwrap();
            client
                .db
                .clear_dirty(&client.db.dirty_chats().await.unwrap())
                .await
                .unwrap();
            client.send_text(conv.clone(), "back".into(), None).await.unwrap();
            assert_eq!(requests(), 2);
            let dirty = client.db.dirty_chats().await.unwrap();
            assert_eq!(dirty.len(), 1);
            assert!(!dirty[0].1.archived);
        });
    }

    #[test]
    fn the_storage_writes_switch_round_trips_and_asks_for_a_sync_when_turned_on() {
        let (_dir, _sink, client, rt) = as_me("client-storage-writes");
        let requests = || client.storage_sync_requests.load(Ordering::SeqCst);
        rt.block_on(async {
            assert!(client.storage_writes().await.unwrap());
            client.set_storage_writes(false).await.unwrap();
            assert!(!client.storage_writes().await.unwrap());
            assert_eq!(requests(), 0);
            client.set_storage_writes(true).await.unwrap();
            assert!(client.storage_writes().await.unwrap());
            assert_eq!(requests(), 1);
        });
    }

    #[test]
    fn typing_needs_a_conversation() {
        let (_dir, _sink, client, _rt) = as_me("client-typing");
        assert!(invalid(client.set_typing("c:nope".into(), true)));
        client.set_typing(format!("c:{ALICE}"), true).unwrap();
        client.set_typing(format!("c:{ALICE}"), false).unwrap();
    }

    use crate::projection::AttachmentState;

    fn file_in(dir: &std::path::Path, name: &str, bytes: &[u8]) -> String {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p.to_string_lossy().into_owned()
    }

    fn outgoing(path: String, content_type: &str) -> OutgoingFile {
        OutgoingFile {
            path,
            content_type: content_type.into(),
            width: Some(4),
            height: Some(3),
            is_voice_note: false,
        }
    }

    #[test]
    fn sent_files_show_at_once_with_their_caption_and_a_copy_of_each_file() {
        let (dir, sink, client, rt) = as_me("client-send-files");
        let src = dir.0.join("src");
        let photo = file_in(&src, "beach.heic", b"heic");
        let pdf = file_in(&src, "plan.pdf", b"pdf");
        let conv = format!("c:{ALICE}");
        rt.block_on(async {
            let key = client
                .send_attachments(
                    conv.clone(),
                    vec![outgoing(photo.clone(), "image/heic"), outgoing(pdf, "application/pdf")],
                    Some(" sunset ".into()),
                    None,
                )
                .await
                .unwrap();
            let m = client.message(key.clone()).await.unwrap().unwrap();
            assert_eq!(
                (m.body.as_deref(), m.status, m.is_outgoing),
                (Some("sunset"), MessageStatus::Pending, true)
            );
            let files = client.attachments(key.clone()).await.unwrap();
            assert_eq!(files.len(), 2);
            assert_eq!(
                (
                    files[0].content_type.as_str(),
                    files[0].file_name.as_deref(),
                    files[0].size
                ),
                ("image/heic", Some("beach.heic"), 4)
            );
            assert_eq!((files[0].width, files[0].state), (Some(4), AttachmentState::Ready));
            // a copy in Semaphore's own folder: the original can go away
            let copy = files[0].path.clone().unwrap();
            assert_ne!(copy, photo);
            std::fs::remove_file(&photo).unwrap();
            assert_eq!(std::fs::read(&copy).unwrap(), b"heic");
            // no caption is fine too
            let key = client
                .send_attachments(
                    conv.clone(),
                    vec![outgoing(copy, "image/heic")],
                    Some("  ".into()),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(client.message(key).await.unwrap().unwrap().body, None);
        });
        assert!(wait_for(
            &sink,
            |e| matches!(e, CoreEvent::MessageChanged { conversation_id, .. } if *conversation_id == conv)
        ));
    }

    #[test]
    fn sending_files_needs_files_that_exist_and_arent_too_big() {
        let (dir, _sink, client, rt) = as_me("client-send-files-bad");
        let conv = format!("c:{ALICE}");
        let big = dir.0.join("big.bin");
        std::fs::create_dir_all(&dir.0).unwrap();
        // sparse: no 100 MB actually written
        std::fs::File::create(&big)
            .unwrap()
            .set_len(MAX_ATTACHMENT + 1)
            .unwrap();
        rt.block_on(async {
            let send = |files: Vec<OutgoingFile>| client.send_attachments(conv.clone(), files, None, None);
            assert!(invalid(send(vec![]).await));
            assert!(invalid(
                send(vec![outgoing("/nope/missing.jpg".into(), "image/jpeg")]).await
            ));
            assert!(invalid(
                send(vec![outgoing(big.to_string_lossy().into(), "application/zip")]).await
            ));
            assert!(client.conversations().await.unwrap().is_empty(), "nothing was stored");
        });
    }

    #[test]
    fn downloads_need_a_link() {
        let dir = temp_dir("client-download-unlinked");
        let sink = Arc::new(Sink(Mutex::new(vec![])));
        let client = Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink).unwrap();
        assert!(matches!(client.download_attachment(1), Err(CoreError::NotLinked)));
    }

    #[test]
    fn unlinking_wipes_this_macs_data_and_closes_the_core() {
        let dir = temp_dir("client-unlink");
        let sink = Arc::new(Sink(Mutex::new(vec![])));
        let client = Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink).unwrap();
        std::fs::create_dir_all(dir.0.join("attachments")).unwrap();
        std::fs::write(dir.0.join("attachments").join("1"), b"photo").unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async { client.unlink(false).await.unwrap() });
        for gone in ["presage.db", "projection.db", "attachments"] {
            assert!(!dir.0.join(gone).exists(), "{gone} is still there");
        }
        assert!(matches!(client.start(), Err(CoreError::Closed)));
        // the data folder is free again: a new core starts from nothing
        let sink = Arc::new(Sink(Mutex::new(vec![])));
        let fresh = Client::open(dir.0.to_string_lossy().into(), "cd".repeat(32), sink).unwrap();
        assert!(!fresh.is_linked());
    }

    #[test]
    fn sessions_are_reset_only_with_known_contacts() {
        let (_dir, _sink, client, _rt) = as_me("client-reset");
        assert!(invalid(client.reset_session(format!("c:PNI:{ALICE}"))));
        assert!(invalid(client.reset_session(format!("g:{}", "ab".repeat(32)))));
        client.reset_session(format!("c:{ALICE}")).unwrap();
    }

    #[test]
    fn after_unlinking_this_handle_reads_nothing_more() {
        let dir = temp_dir("client-unlink-closed");
        let sink = Arc::new(Sink(Mutex::new(vec![])));
        let client = Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            client.unlink(false).await.unwrap();
            assert!(client.chat_events(format!("c:{ALICE}"), 0).await.is_err());
        });
    }

    #[test]
    fn our_messages_take_the_chats_timer_and_start_it_at_once() {
        let (_dir, _sink, client, rt) = as_me("client-timer");
        let conv = format!("c:{ALICE}");
        rt.block_on(async {
            client
                .db
                .apply(&crate::projection::Op::SetTimer {
                    conversation_id: conv.clone(),
                    author: ALICE.into(),
                    seconds: 60,
                    at: 1,
                })
                .await
                .unwrap();
            let key = client.send_text(conv, "brb".into(), None).await.unwrap();
            let m = client.message(key.clone()).await.unwrap().unwrap();
            assert_eq!((m.expires_in, m.expires_at), (60, Some(key.sent_at + 60_000)));
        });
    }
}
