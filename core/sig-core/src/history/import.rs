//! Maps archive frames to projection rows and presage seeds. Pure logic, no IO: the driver
//! feeds frames in and writes the batches out.

use std::collections::HashMap;

use libsignal_message_backup::proto::backup;
use presage::libsignal_service::prelude::{ProfileKey, ProtobufMessage as _, Timer, Uuid};
use presage::libsignal_service::protocol::{Aci, Pni, ServiceId};

use crate::projection::{Message, MessageKind, MessageStatus, Quote};

/// How many rows go into one transaction.
pub(crate) const BATCH: usize = 1_000;

/// A transit-CDN file stays downloadable this long after upload (Desktop's rule: twice the
/// server's 45-day queue time).
const TRANSIT_TTL_MS: i64 = 90 * 24 * 60 * 60 * 1000;

#[derive(Default, Debug, PartialEq)]
pub(crate) struct HistoryBatch {
    pub conversations: Vec<ImportConversation>,
    pub messages: Vec<ImportMessage>,
    pub events: Vec<ImportEvent>,
}

impl HistoryBatch {
    pub fn len(&self) -> usize {
        self.conversations.len() + self.messages.len() + self.events.len()
    }
}

#[derive(Default, Debug, PartialEq)]
pub(crate) struct ImportConversation {
    pub id: String,
    pub title: String,
    pub expire_timer: u32,
    pub archived: bool,
    pub pinned_order: Option<u32>,
    /// Ms since the epoch; 0: not muted; `i64::MAX`: always (the backup's INT64_MAX).
    pub mute_until: i64,
    pub marked_unread: bool,
}

#[derive(Debug, PartialEq)]
pub(crate) struct ImportMessage {
    pub message: Message,
    /// (reactor service id, emoji), one per reactor (the item's latest by sortOrder).
    pub reactions: Vec<(String, String)>,
    pub attachments: Vec<ImportAttachment>,
    pub preview: Option<ImportPreview>,
    /// An edited message's later timestamps (each revision's, and the newest's): live ops
    /// target the message by any of them, so they map to `message.sent_at`.
    pub revisions: Vec<i64>,
    /// The newest revision's timestamp (0: never edited), so only a later live edit wins.
    pub edited_at: i64,
}

#[derive(Debug, PartialEq)]
pub(crate) struct ImportAttachment {
    pub position: i64,
    pub content_type: String,
    pub file_name: Option<String>,
    pub size: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub is_voice_note: bool,
    /// An encoded `AttachmentPointer` if still downloadable (transit key, uploaded within 90
    /// days); None shows as no longer available.
    pub pointer: Option<Vec<u8>>,
}

#[derive(Debug, PartialEq)]
pub(crate) struct ImportPreview {
    pub url: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub image: Option<ImportAttachment>,
}

/// A TimerChanged chat event.
#[derive(Debug, PartialEq)]
pub(crate) struct ImportEvent {
    pub conversation_id: String,
    pub at: i64,
    pub author: String,
    /// The new timer (seconds, 0: off).
    pub value: i64,
}

#[derive(Default, Debug, PartialEq, Clone)]
pub(crate) struct Counts {
    pub messages: u64,
    pub conversations: u64,
    pub skipped: u64,
    pub expired: u64,
    pub unavailable_files: u64,
}

/// What presage's store needs so sending keeps working: contacts (with profile keys) and groups.
#[derive(Default)]
pub(crate) struct Seed {
    pub contacts: Vec<presage::model::contacts::Contact>,
    pub groups: Vec<([u8; 32], presage::model::groups::Group)>,
}

/// Who a recipient id stands for.
#[derive(Clone)]
enum Known {
    Me,
    /// The service-id string: a bare ACI uuid, or `PNI:<uuid>`.
    Direct(String),
    Group(String),
}

pub(crate) struct Importer {
    me: String,
    now_ms: i64,
    /// Recipient id → who it is (None: a kind we can't address; its items are skipped).
    recipients: HashMap<u64, Option<Known>>,
    /// Recipient id → display title (contact or group name), for Chat frames.
    titles: HashMap<u64, String>,
    /// Chat id → conversation id.
    chats: HashMap<u64, String>,
    /// ACI → profile key, to seed contacts and join group members.
    profile_keys: HashMap<Uuid, [u8; 32]>,
    /// Contact seeds: (ACI, name, recipient id), joined with chat timers in `finish`.
    contact_parts: Vec<(Uuid, String, u64)>,
    /// Chat timers by recipient id, for the contact seeds.
    timer_by_recipient: HashMap<u64, u32>,
    /// Group snapshots wait for `finish`, so members can join profile keys seen later.
    group_frames: Vec<([u8; 32], backup::Group)>,
    batch: HistoryBatch,
    counts: Counts,
}

impl Importer {
    pub fn new(me_aci: &str, now_ms: i64) -> Self {
        Self {
            me: me_aci.to_owned(),
            now_ms,
            recipients: HashMap::new(),
            titles: HashMap::new(),
            chats: HashMap::new(),
            profile_keys: HashMap::new(),
            contact_parts: Vec::new(),
            timer_by_recipient: HashMap::new(),
            group_frames: Vec::new(),
            batch: HistoryBatch::default(),
            counts: Counts::default(),
        }
    }

    /// Buffers one frame's rows; returns a full batch when `BATCH` items are ready.
    pub fn add(&mut self, frame: backup::Frame) -> Option<HistoryBatch> {
        match frame.item {
            Some(backup::frame::Item::Recipient(r)) => self.recipient(r),
            Some(backup::frame::Item::Chat(c)) => self.chat(c),
            Some(backup::frame::Item::ChatItem(i)) => self.chat_item(i),
            // account data isn't ours to import; the rest (sticker packs, calls, notification
            // profiles, chat folders) is out of scope
            Some(backup::frame::Item::Account(_)) => {}
            Some(_) | None => self.counts.skipped += 1,
        }
        (self.batch.len() >= BATCH).then(|| std::mem::take(&mut self.batch))
    }

    /// The final partial batch, the presage seed, and the counters.
    pub fn finish(mut self) -> (HistoryBatch, Seed, Counts) {
        let mut seed = Seed::default();
        for (uuid, name, recipient_id) in &self.contact_parts {
            seed.contacts.push(presage::model::contacts::Contact {
                uuid: *uuid,
                phone_number: None,
                name: name.clone(),
                verified: Default::default(),
                profile_key: self.profile_keys.get(uuid).map(|k| k.to_vec()).unwrap_or_default(),
                expire_timer: self.timer_by_recipient.get(recipient_id).copied().unwrap_or(0),
                expire_timer_version: 1,
                inbox_position: 0,
                avatar: None,
            });
        }
        for (master_key, group) in &self.group_frames {
            let snapshot = group.snapshot.as_ref();
            let title = snapshot
                .and_then(|s| s.title.as_ref())
                .and_then(blob_title)
                .unwrap_or_default();
            let timer = snapshot
                .and_then(|s| s.disappearingMessagesTimer.as_ref())
                .and_then(blob_timer);
            let description = snapshot.and_then(|s| s.description.as_ref()).and_then(blob_description);
            let members = snapshot
                .map(|s| {
                    s.members
                        .iter()
                        .filter_map(|m| {
                            let aci = Uuid::from_slice(&m.userId).ok()?;
                            Some(presage::model::groups::Member {
                                aci: Aci::from(aci),
                                role: presage::libsignal_service::groups_v2::Role::Default,
                                // a member whose Contact frame carried no key gets a zeroed one;
                                // presage falls back to identified sends
                                profile_key: ProfileKey::create(
                                    self.profile_keys.get(&aci).copied().unwrap_or([0; 32]),
                                ),
                                joined_at_revision: m.joinedAtVersion,
                                label: None,
                                label_emoji: None,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            seed.groups.push((
                *master_key,
                presage::model::groups::Group {
                    title,
                    avatar: String::new(),
                    disappearing_messages_timer: timer.map(|duration| Timer { duration }),
                    access_control: None,
                    revision: snapshot.map(|s| s.version).unwrap_or(0),
                    members,
                    pending_members: vec![],
                    requesting_members: vec![],
                    invite_link_password: snapshot.map(|s| s.inviteLinkPassword.clone()).unwrap_or_default(),
                    description,
                },
            ));
        }
        (std::mem::take(&mut self.batch), seed, self.counts)
    }

    fn recipient(&mut self, r: backup::Recipient) {
        use backup::recipient::Destination;
        let known = match r.destination {
            Some(Destination::Self_(_)) => Some(Known::Me),
            Some(Destination::Contact(c)) => {
                let aci = c.aci.as_deref().and_then(|b| Uuid::from_slice(b).ok());
                let pni = c.pni.as_deref().and_then(|b| Uuid::from_slice(b).ok());
                match (aci, pni) {
                    (Some(aci), _) => {
                        if let Some(key) = c.profileKey.as_deref().and_then(|k| <[u8; 32]>::try_from(k).ok()) {
                            self.profile_keys.insert(aci, key);
                        }
                        let name = [c.profileGivenName.as_deref(), c.profileFamilyName.as_deref()]
                            .into_iter()
                            .flatten()
                            .collect::<Vec<_>>()
                            .join(" ")
                            .trim()
                            .to_owned();
                        self.titles.insert(r.id, name.clone());
                        self.contact_parts.push((aci, name, r.id));
                        Some(Known::Direct(ServiceId::from(Aci::from(aci)).service_id_string()))
                    }
                    (None, Some(pni)) => {
                        // addressable, but presage contacts are ACI-keyed: no seed
                        Some(Known::Direct(ServiceId::from(Pni::from(pni)).service_id_string()))
                    }
                    // a phone number alone can't be messaged from here
                    (None, None) => None,
                }
            }
            Some(Destination::Group(g)) => match <[u8; 32]>::try_from(g.masterKey.as_slice()) {
                Ok(master_key) => {
                    let id = crate::projection::ids::group_id(&master_key);
                    let title = g
                        .snapshot
                        .as_ref()
                        .and_then(|s| s.title.as_ref())
                        .and_then(blob_title)
                        .unwrap_or_default();
                    self.titles.insert(r.id, title);
                    self.group_frames.push((master_key, g));
                    Some(Known::Group(id))
                }
                Err(_) => None,
            },
            // distribution lists, call links, release notes: nothing to show
            Some(_) | None => None,
        };
        if known.is_none() {
            self.counts.skipped += 1;
        }
        self.recipients.insert(r.id, known);
    }

    fn chat(&mut self, c: backup::Chat) {
        let Some(Some(known)) = self.recipients.get(&c.recipientId) else {
            self.counts.skipped += 1;
            return;
        };
        let id = self.conversation_id(known);
        let expire_timer = (c.expirationTimerMs.unwrap_or(0) / 1000) as u32;
        self.timer_by_recipient.insert(c.recipientId, expire_timer);
        self.chats.insert(c.id, id.clone());
        self.batch.conversations.push(ImportConversation {
            id,
            title: self.titles.get(&c.recipientId).cloned().unwrap_or_default(),
            expire_timer,
            archived: c.archived,
            pinned_order: c.pinnedOrder,
            mute_until: c.muteUntilMs.map_or(0, |ms| i64::try_from(ms).unwrap_or(i64::MAX)),
            marked_unread: c.markedUnread,
        });
        self.counts.conversations += 1;
    }

    fn chat_item(&mut self, item: backup::ChatItem) {
        use backup::chat_item::{DirectionalDetails, Item};
        let Some(conversation_id) = self.chats.get(&item.chatId).cloned() else {
            self.counts.skipped += 1;
            return;
        };
        let Some(author) = self.author_of(item.authorId) else {
            self.counts.skipped += 1;
            return;
        };
        let sent_at = item.dateSent as i64;
        // a timer change is a chat event, not a message
        if let Some(Item::UpdateMessage(u)) = &item.item {
            use backup::chat_update_message::Update;
            match &u.update {
                Some(Update::ExpirationTimerChange(e)) => self.batch.events.push(ImportEvent {
                    conversation_id,
                    at: sent_at,
                    author,
                    value: (e.expiresInMs / 1000) as i64,
                }),
                // calls, group changes, profile changes…: no M1 kind to show them as
                _ => self.counts.skipped += 1,
            }
            return;
        }
        let expires_in = (item.expiresInMs.unwrap_or(0) / 1000) as u32;
        let expires_at = match (item.expireStartDate.unwrap_or(0), item.expiresInMs.unwrap_or(0)) {
            (start, len) if start > 0 && len > 0 => Some((start + len) as i64),
            _ => None,
        };
        if expires_at.is_some_and(|at| at <= self.now_ms) {
            self.counts.expired += 1;
            return;
        }
        // An edited item is the newest revision; its identity (and the key a copy already
        // here uses) is the original's timestamp, the oldest revision's.
        let original_sent_at = item.revisions.first().map(|r| r.dateSent as i64).unwrap_or(sent_at);
        let revisions: Vec<i64> = if item.revisions.is_empty() {
            vec![]
        } else {
            item.revisions
                .iter()
                .skip(1)
                .map(|r| r.dateSent as i64)
                .chain([sent_at])
                .collect()
        };
        let (is_outgoing, status, received_at) = match &item.directionalDetails {
            Some(DirectionalDetails::Incoming(d)) => {
                let status = if d.read {
                    MessageStatus::Read
                } else {
                    MessageStatus::Delivered
                };
                let received = if d.dateReceived > 0 {
                    d.dateReceived as i64
                } else {
                    sent_at
                };
                (false, status, received)
            }
            Some(DirectionalDetails::Outgoing(d)) => {
                let received = if d.dateReceived > 0 {
                    d.dateReceived as i64
                } else {
                    sent_at
                };
                (true, send_status(&d.sendStatus), received)
            }
            Some(DirectionalDetails::Directionless(_)) => (author == self.me, MessageStatus::Read, sent_at),
            // the oneof is non-exhaustive: an unknown direction is an unknown item
            Some(_) | None => {
                self.counts.skipped += 1;
                return;
            }
        };
        let mut body = None;
        let mut quote = None;
        let mut kind = MessageKind::Normal;
        let mut deleted = false;
        let mut reactions = vec![];
        let mut attachments = vec![];
        let mut preview = None;
        match &item.item {
            Some(Item::StandardMessage(m)) => {
                body = m.text.as_ref().map(|t| t.body.clone()).filter(|b| !b.is_empty());
                quote = m.quote.as_ref().and_then(|q| self.quote_of(q));
                reactions = self.latest_reactions(&m.reactions);
                attachments = m
                    .attachments
                    .iter()
                    .enumerate()
                    .map(|(i, a)| {
                        let voice = a.flag.enum_value_or_default() == backup::message_attachment::Flag::VOICE_MESSAGE;
                        self.attachment_of(i as i64, a.pointer.as_ref(), voice)
                    })
                    .collect();
                preview = m.linkPreview.first().map(|p| ImportPreview {
                    url: p.url.clone(),
                    title: p.title.clone().filter(|t| !t.is_empty()),
                    description: p.description.clone().filter(|d| !d.is_empty()),
                    image: p.image.as_ref().map(|img| self.attachment_of(-1, Some(img), false)),
                });
            }
            Some(Item::ContactMessage(c)) => {
                kind = MessageKind::Contact {
                    name: c.contact.as_ref().and_then(contact_name),
                };
                reactions = self.latest_reactions(&c.reactions);
            }
            Some(Item::StickerMessage(s)) => {
                kind = MessageKind::Sticker {
                    emoji: s
                        .sticker
                        .as_ref()
                        .and_then(|st| st.emoji.clone())
                        .filter(|e| !e.is_empty()),
                };
                reactions = self.latest_reactions(&s.reactions);
            }
            Some(Item::RemoteDeletedMessage(_)) | Some(Item::AdminDeletedMessage(_)) => deleted = true,
            Some(Item::PaymentNotification(_)) => kind = MessageKind::Payment,
            Some(Item::GiftBadge(_)) => kind = MessageKind::GiftBadge,
            Some(Item::Poll(p)) => {
                kind = MessageKind::Poll {
                    question: Some(p.question.clone()).filter(|q| !q.is_empty()),
                };
                reactions = self.latest_reactions(&p.reactions);
            }
            Some(Item::UpdateMessage(_)) => unreachable!("handled above"),
            // view-once media and story replies have no M1 kind yet
            Some(_) | None => {
                self.counts.skipped += 1;
                return;
            }
        }
        self.batch.messages.push(ImportMessage {
            message: Message {
                conversation_id,
                author,
                sent_at: original_sent_at,
                received_at,
                body,
                is_outgoing,
                status,
                edited: !item.revisions.is_empty(),
                deleted,
                quote,
                kind,
                expires_in,
                expires_at,
            },
            reactions,
            attachments,
            preview,
            edited_at: if item.revisions.is_empty() { 0 } else { sent_at },
            revisions,
        });
        self.counts.messages += 1;
    }

    fn conversation_id(&self, known: &Known) -> String {
        match known {
            Known::Me => format!("c:{}", self.me),
            Known::Direct(sid) => format!("c:{sid}"),
            Known::Group(id) => id.clone(),
        }
    }

    /// The author string of a recipient id: us or a contact (groups don't author messages).
    fn author_of(&self, recipient_id: u64) -> Option<String> {
        match self.recipients.get(&recipient_id) {
            Some(Some(Known::Me)) => Some(self.me.clone()),
            Some(Some(Known::Direct(sid))) => Some(sid.clone()),
            _ => None,
        }
    }

    /// One reaction per reactor: the reactor's latest by sortOrder, in reactor order
    /// (deterministic). A reactor we can't resolve is dropped.
    fn latest_reactions(&self, reactions: &[backup::Reaction]) -> Vec<(String, String)> {
        let mut best: HashMap<String, (u64, String)> = HashMap::new();
        for r in reactions {
            let Some(reactor) = self.author_of(r.authorId) else {
                continue;
            };
            let entry = best.entry(reactor).or_insert((r.sortOrder, r.emoji.clone()));
            if r.sortOrder >= entry.0 {
                *entry = (r.sortOrder, r.emoji.clone());
            }
        }
        let mut out: Vec<(String, String)> = best.into_iter().map(|(reactor, (_, emoji))| (reactor, emoji)).collect();
        out.sort();
        out
    }

    fn quote_of(&self, q: &backup::Quote) -> Option<Quote> {
        let sent_at = q.targetSentTimestamp? as i64;
        let author = self.author_of(q.authorId)?;
        Some(Quote {
            author,
            sent_at,
            text: q.text.as_ref().map(|t| t.body.clone()).filter(|b| !b.is_empty()),
        })
    }

    /// One file row. Downloadable only with a transit key uploaded within the last 90 days;
    /// otherwise it shows as no longer available.
    fn attachment_of(&mut self, position: i64, pointer: Option<&backup::FilePointer>, voice: bool) -> ImportAttachment {
        use backup::file_pointer::locator_info::IntegrityCheck;
        use presage::proto::attachment_pointer::AttachmentIdentifier;
        let loc = pointer.and_then(|p| p.locatorInfo.as_ref());
        let transit_key = loc.and_then(|l| l.transitCdnKey.clone()).filter(|k| !k.is_empty());
        let fresh = loc
            .and_then(|l| l.transitTierUploadTimestamp)
            .is_some_and(|t| t as i64 >= self.now_ms - TRANSIT_TTL_MS);
        let encoded = match (transit_key, fresh, loc) {
            (Some(cdn_key), true, Some(l)) => Some(
                presage::proto::AttachmentPointer {
                    attachment_identifier: Some(AttachmentIdentifier::CdnKey(cdn_key)),
                    cdn_number: l.transitCdnNumber,
                    key: Some(l.key.clone()),
                    digest: match &l.integrityCheck {
                        Some(IntegrityCheck::EncryptedDigest(d)) => Some(d.clone()),
                        _ => None,
                    },
                    size: Some(l.size),
                    content_type: pointer.and_then(|p| p.contentType.clone()),
                    file_name: pointer.and_then(|p| p.fileName.clone()),
                    width: pointer.and_then(|p| p.width),
                    height: pointer.and_then(|p| p.height),
                    upload_timestamp: l.transitTierUploadTimestamp,
                    ..Default::default()
                }
                .encode_to_vec(),
            ),
            _ => None,
        };
        if encoded.is_none() {
            self.counts.unavailable_files += 1;
        }
        ImportAttachment {
            position,
            content_type: pointer
                .and_then(|p| p.contentType.clone())
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| "application/octet-stream".into()),
            file_name: pointer.and_then(|p| p.fileName.clone()).filter(|n| !n.is_empty()),
            size: loc.map(|l| l.size as u64).unwrap_or(0),
            width: pointer.and_then(|p| p.width),
            height: pointer.and_then(|p| p.height),
            is_voice_note: voice,
            pointer: encoded,
        }
    }
}

/// The highest delivery state any recipient reached (imports are never Pending: nothing here
/// will retry them).
fn send_status(statuses: &[backup::SendStatus]) -> MessageStatus {
    use backup::send_status::DeliveryStatus;
    statuses
        .iter()
        .filter_map(|s| {
            Some(match s.deliveryStatus.as_ref()? {
                DeliveryStatus::Read(_) | DeliveryStatus::Viewed(_) => MessageStatus::Read,
                DeliveryStatus::Delivered(_) => MessageStatus::Delivered,
                DeliveryStatus::Sent(_) | DeliveryStatus::Pending(_) | DeliveryStatus::Skipped(_) => {
                    MessageStatus::Sent
                }
                DeliveryStatus::Failed(_) => MessageStatus::Failed,
                _ => return None,
            })
        })
        .max()
        .unwrap_or(MessageStatus::Sent)
}

fn contact_name(c: &backup::ContactAttachment) -> Option<String> {
    let name = c
        .name
        .as_ref()
        .map(|n| format!("{} {}", n.givenName, n.familyName))
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty());
    name.or_else(|| Some(c.organization.clone()).filter(|o| !o.is_empty()))
}

fn blob_title(b: &backup::group::GroupAttributeBlob) -> Option<String> {
    match &b.content {
        Some(backup::group::group_attribute_blob::Content::Title(t)) => {
            Some(t.trim().to_owned()).filter(|t| !t.is_empty())
        }
        _ => None,
    }
}

fn blob_timer(b: &backup::group::GroupAttributeBlob) -> Option<u32> {
    match &b.content {
        Some(backup::group::group_attribute_blob::Content::DisappearingMessagesDuration(d)) => Some(*d),
        _ => None,
    }
}

fn blob_description(b: &backup::group::GroupAttributeBlob) -> Option<String> {
    match &b.content {
        Some(backup::group::group_attribute_blob::Content::DescriptionText(d)) => {
            Some(d.clone()).filter(|d| !d.is_empty())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use protobuf::MessageField;

    use super::*;

    const ME: &str = "11111111-1111-1111-1111-111111111111";
    const ALICE: &str = "22222222-2222-2222-2222-222222222222";
    const BOB: &str = "33333333-3333-3333-3333-333333333333";
    const NOW: i64 = 1_800_000_000_000;
    const DAY_MS: u64 = 24 * 60 * 60 * 1000;

    fn uuid(s: &str) -> Uuid {
        Uuid::parse_str(s).unwrap()
    }

    fn frame(item: backup::frame::Item) -> backup::Frame {
        backup::Frame {
            item: Some(item),
            ..Default::default()
        }
    }

    fn recipient(id: u64, destination: backup::recipient::Destination) -> backup::Frame {
        frame(backup::frame::Item::Recipient(backup::Recipient {
            id,
            destination: Some(destination),
            ..Default::default()
        }))
    }

    fn contact_frame(id: u64, aci: &str, given: &str) -> backup::Frame {
        recipient(
            id,
            backup::recipient::Destination::Contact(backup::Contact {
                aci: Some(uuid(aci).as_bytes().to_vec()),
                profileKey: Some(vec![3u8; 32]),
                profileGivenName: Some(given.into()),
                ..Default::default()
            }),
        )
    }

    fn self_frame(id: u64) -> backup::Frame {
        recipient(id, backup::recipient::Destination::Self_(Default::default()))
    }

    fn chat_frame(id: u64, recipient_id: u64, timer_ms: u64) -> backup::Frame {
        frame(backup::frame::Item::Chat(backup::Chat {
            id,
            recipientId: recipient_id,
            expirationTimerMs: (timer_ms > 0).then_some(timer_ms),
            ..Default::default()
        }))
    }

    fn standard(body: &str) -> backup::chat_item::Item {
        backup::chat_item::Item::StandardMessage(backup::StandardMessage {
            text: MessageField::some(backup::Text {
                body: body.into(),
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    fn chat_item(chat: u64, author: u64, sent_at: u64, item: backup::chat_item::Item) -> backup::ChatItem {
        backup::ChatItem {
            chatId: chat,
            authorId: author,
            dateSent: sent_at,
            directionalDetails: Some(backup::chat_item::DirectionalDetails::Incoming(
                backup::chat_item::IncomingMessageDetails {
                    dateReceived: sent_at + 1,
                    read: true,
                    ..Default::default()
                },
            )),
            item: Some(item),
            ..Default::default()
        }
    }

    fn incoming_text(chat: u64, author: u64, sent_at: u64, body: &str, read: bool) -> backup::Frame {
        let mut item = chat_item(chat, author, sent_at, standard(body));
        item.directionalDetails = Some(backup::chat_item::DirectionalDetails::Incoming(
            backup::chat_item::IncomingMessageDetails {
                dateReceived: sent_at + 1,
                read,
                ..Default::default()
            },
        ));
        frame(backup::frame::Item::ChatItem(item))
    }

    /// An importer with Alice (recipient 1, chat 10) and us (recipient 2) already known.
    fn with_alice() -> Importer {
        let mut imp = Importer::new(ME, NOW);
        assert!(imp.add(contact_frame(1, ALICE, "Alice")).is_none());
        assert!(imp.add(self_frame(2)).is_none());
        assert!(imp.add(chat_frame(10, 1, 0)).is_none());
        imp
    }

    #[test]
    fn a_contact_chat_and_message_become_rows() {
        let mut imp = with_alice();
        assert!(imp.add(incoming_text(10, 1, 5_000, "hi", true)).is_none());
        let (batch, seed, counts) = imp.finish();
        assert_eq!(
            batch.conversations,
            vec![ImportConversation {
                id: format!("c:{ALICE}"),
                title: "Alice".into(),
                expire_timer: 0,
                ..Default::default()
            }]
        );
        let m = &batch.messages[0].message;
        assert_eq!(
            (m.conversation_id.as_str(), m.author.as_str(), m.sent_at, m.received_at),
            (format!("c:{ALICE}").as_str(), ALICE, 5_000, 5_001)
        );
        assert_eq!(
            (m.is_outgoing, m.status, m.body.as_deref(), m.edited, m.deleted),
            (false, MessageStatus::Read, Some("hi"), false, false)
        );
        assert_eq!(seed.contacts.len(), 1);
        assert_eq!(
            (seed.contacts[0].name.as_str(), seed.contacts[0].uuid),
            ("Alice", uuid(ALICE))
        );
        assert_eq!(seed.contacts[0].profile_key, vec![3u8; 32]);
        assert_eq!((counts.messages, counts.conversations, counts.skipped), (1, 1, 0));
    }

    #[test]
    fn odd_backup_flags_are_read_safely() {
        let mut imp = Importer::new(ME, NOW);
        assert!(imp.add(contact_frame(1, ALICE, "Alice")).is_none());
        assert!(imp.add(self_frame(2)).is_none());
        assert!(imp
            .add(frame(backup::frame::Item::Chat(backup::Chat {
                id: 10,
                recipientId: 1,
                pinnedOrder: Some(0),        // a pin order of 0 is a pin
                muteUntilMs: Some(u64::MAX), // past INT64_MAX: as good as "always"
                ..Default::default()
            })))
            .is_none());
        let (batch, _, _) = imp.finish();
        let c = &batch.conversations[0];
        assert_eq!((c.pinned_order, c.mute_until), (Some(0), i64::MAX));
    }

    #[test]
    fn chat_flags_come_from_the_backup() {
        let mut imp = Importer::new(ME, NOW);
        assert!(imp.add(contact_frame(1, ALICE, "Alice")).is_none());
        assert!(imp.add(self_frame(2)).is_none());
        assert!(imp
            .add(frame(backup::frame::Item::Chat(backup::Chat {
                id: 10,
                recipientId: 1,
                archived: true,
                pinnedOrder: Some(2),
                muteUntilMs: Some(i64::MAX as u64), // the backup's "always"
                markedUnread: true,
                ..Default::default()
            })))
            .is_none());
        let (batch, _, _) = imp.finish();
        assert_eq!(
            batch.conversations,
            vec![ImportConversation {
                id: format!("c:{ALICE}"),
                title: "Alice".into(),
                expire_timer: 0,
                archived: true,
                pinned_order: Some(2),
                mute_until: i64::MAX,
                marked_unread: true,
            }]
        );
    }

    #[test]
    fn an_unread_incoming_message_is_delivered_not_read() {
        let mut imp = with_alice();
        imp.add(incoming_text(10, 1, 5_000, "hi", false));
        let (batch, _, _) = imp.finish();
        assert_eq!(batch.messages[0].message.status, MessageStatus::Delivered);
    }

    #[test]
    fn an_e164_only_contact_and_its_chat_are_skipped() {
        let mut imp = Importer::new(ME, NOW);
        imp.add(recipient(
            1,
            backup::recipient::Destination::Contact(backup::Contact {
                e164: Some(4915112345678),
                ..Default::default()
            }),
        ));
        imp.add(chat_frame(10, 1, 0));
        imp.add(incoming_text(10, 1, 5_000, "hi", true));
        let (batch, seed, counts) = imp.finish();
        assert_eq!(batch.len(), 0);
        assert!(seed.contacts.is_empty());
        assert_eq!(counts.skipped, 3, "recipient, chat and item");
    }

    #[test]
    fn an_expired_item_is_skipped() {
        let mut imp = with_alice();
        let mut item = chat_item(10, 1, 5_000, standard("gone"));
        item.expireStartDate = Some((NOW as u64) - 10_000);
        item.expiresInMs = Some(5_000);
        imp.add(frame(backup::frame::Item::ChatItem(item)));
        let (batch, _, counts) = imp.finish();
        assert!(batch.messages.is_empty());
        assert_eq!((counts.expired, counts.messages), (1, 0));
    }

    #[test]
    fn a_still_ticking_timer_is_kept() {
        let mut imp = with_alice();
        let mut item = chat_item(10, 1, 5_000, standard("soon"));
        item.expireStartDate = Some(NOW as u64);
        item.expiresInMs = Some(60_000);
        imp.add(frame(backup::frame::Item::ChatItem(item)));
        let (batch, _, _) = imp.finish();
        let m = &batch.messages[0].message;
        assert_eq!((m.expires_in, m.expires_at), (60, Some(NOW + 60_000)));
    }

    #[test]
    fn outgoing_status_is_the_highest_send_status() {
        use backup::send_status::DeliveryStatus;
        let send = |statuses: Vec<DeliveryStatus>| {
            let mut imp = with_alice();
            let mut item = chat_item(10, 2, 7_000, standard("out"));
            item.directionalDetails = Some(backup::chat_item::DirectionalDetails::Outgoing(
                backup::chat_item::OutgoingMessageDetails {
                    sendStatus: statuses
                        .into_iter()
                        .map(|s| backup::SendStatus {
                            deliveryStatus: Some(s),
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                },
            ));
            imp.add(frame(backup::frame::Item::ChatItem(item)));
            let (batch, _, _) = imp.finish();
            let m = batch.messages[0].message.clone();
            assert!(m.is_outgoing);
            assert_eq!(m.author, ME);
            m.status
        };
        assert_eq!(
            send(vec![
                DeliveryStatus::Delivered(Default::default()),
                DeliveryStatus::Read(Default::default())
            ]),
            MessageStatus::Read
        );
        assert_eq!(
            send(vec![DeliveryStatus::Failed(Default::default())]),
            MessageStatus::Failed
        );
        assert_eq!(send(vec![]), MessageStatus::Sent);
    }

    #[test]
    fn placeholders_map_to_their_kinds() {
        let mut imp = with_alice();
        imp.add(frame(backup::frame::Item::ChatItem(chat_item(
            10,
            1,
            1,
            backup::chat_item::Item::StickerMessage(backup::StickerMessage {
                sticker: MessageField::some(backup::Sticker {
                    emoji: Some("🦊".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        ))));
        imp.add(frame(backup::frame::Item::ChatItem(chat_item(
            10,
            1,
            2,
            backup::chat_item::Item::ContactMessage(backup::ContactMessage {
                contact: MessageField::some(backup::ContactAttachment {
                    name: MessageField::some(backup::contact_attachment::Name {
                        givenName: "Carol".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        ))));
        imp.add(frame(backup::frame::Item::ChatItem(chat_item(
            10,
            1,
            3,
            backup::chat_item::Item::PaymentNotification(Default::default()),
        ))));
        imp.add(frame(backup::frame::Item::ChatItem(chat_item(
            10,
            1,
            4,
            backup::chat_item::Item::GiftBadge(Default::default()),
        ))));
        imp.add(frame(backup::frame::Item::ChatItem(chat_item(
            10,
            1,
            5,
            backup::chat_item::Item::Poll(backup::Poll {
                question: "Lunch?".into(),
                ..Default::default()
            }),
        ))));
        imp.add(frame(backup::frame::Item::ChatItem(chat_item(
            10,
            1,
            6,
            backup::chat_item::Item::ViewOnceMessage(Default::default()),
        ))));
        let (batch, _, counts) = imp.finish();
        let kinds: Vec<MessageKind> = batch.messages.iter().map(|m| m.message.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                MessageKind::Sticker {
                    emoji: Some("🦊".into())
                },
                MessageKind::Contact {
                    name: Some("Carol".into())
                },
                MessageKind::Payment,
                MessageKind::GiftBadge,
                MessageKind::Poll {
                    question: Some("Lunch?".into())
                },
            ]
        );
        assert_eq!(counts.skipped, 1, "view-once has no kind yet");
    }

    #[test]
    fn deleted_messages_are_tombstones() {
        let mut imp = with_alice();
        imp.add(frame(backup::frame::Item::ChatItem(chat_item(
            10,
            1,
            1,
            backup::chat_item::Item::RemoteDeletedMessage(Default::default()),
        ))));
        let (batch, _, _) = imp.finish();
        let m = &batch.messages[0].message;
        assert!(m.deleted);
        assert_eq!(m.body, None);
    }

    #[test]
    fn reactions_keep_the_latest_per_reactor() {
        let mut imp = with_alice();
        let react = |emoji: &str, author: u64, sort: u64| backup::Reaction {
            emoji: emoji.into(),
            authorId: author,
            sortOrder: sort,
            ..Default::default()
        };
        let mut sm = backup::StandardMessage {
            text: MessageField::some(backup::Text {
                body: "hi".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        sm.reactions = vec![react("👍", 1, 1), react("❤️", 1, 2), react("😀", 2, 1)];
        imp.add(frame(backup::frame::Item::ChatItem(chat_item(
            10,
            1,
            1,
            backup::chat_item::Item::StandardMessage(sm),
        ))));
        let (batch, _, _) = imp.finish();
        let mut got = batch.messages[0].reactions.clone();
        got.sort();
        assert_eq!(
            got,
            vec![
                (ME.to_string(), "😀".to_string()),
                (ALICE.to_string(), "❤️".to_string())
            ]
        );
    }

    #[test]
    fn a_reaction_tie_takes_the_later_entry() {
        let mut imp = with_alice();
        let react = |emoji: &str, sort: u64| backup::Reaction {
            emoji: emoji.into(),
            authorId: 1,
            sortOrder: sort,
            ..Default::default()
        };
        let sm = backup::StandardMessage {
            text: MessageField::some(backup::Text {
                body: "hi".into(),
                ..Default::default()
            }),
            reactions: vec![react("👍", 7), react("❤️", 7)],
            ..Default::default()
        };
        imp.add(frame(backup::frame::Item::ChatItem(chat_item(
            10,
            1,
            1,
            backup::chat_item::Item::StandardMessage(sm),
        ))));
        let (batch, _, _) = imp.finish();
        assert_eq!(batch.messages[0].reactions, vec![(ALICE.to_string(), "❤️".to_string())]);
    }

    #[test]
    fn attachments_follow_the_90_day_rule() {
        use presage::proto::attachment_pointer::AttachmentIdentifier;
        let mut imp = with_alice();
        let file = |key: Option<&str>, uploaded: Option<u64>| backup::MessageAttachment {
            pointer: MessageField::some(backup::FilePointer {
                contentType: Some("image/png".into()),
                locatorInfo: MessageField::some(backup::file_pointer::LocatorInfo {
                    key: vec![1, 2, 3],
                    size: 99,
                    transitCdnKey: key.map(Into::into),
                    transitCdnNumber: Some(3),
                    transitTierUploadTimestamp: uploaded,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let sm = backup::StandardMessage {
            attachments: vec![
                file(Some("fresh"), Some((NOW - DAY_MS as i64) as u64)),
                file(Some("stale"), Some((NOW - 91 * DAY_MS as i64) as u64)),
                file(None, Some((NOW - DAY_MS as i64) as u64)),
            ],
            ..Default::default()
        };
        imp.add(frame(backup::frame::Item::ChatItem(chat_item(
            10,
            1,
            1,
            backup::chat_item::Item::StandardMessage(sm),
        ))));
        let (batch, _, counts) = imp.finish();
        let files = &batch.messages[0].attachments;
        assert_eq!(files.len(), 3);
        let pointer = files[0].pointer.as_ref().expect("fresh file downloads");
        let decoded = presage::proto::AttachmentPointer::decode(pointer.as_slice()).unwrap();
        assert_eq!(
            decoded.attachment_identifier,
            Some(AttachmentIdentifier::CdnKey("fresh".into()))
        );
        assert_eq!((decoded.cdn_number, decoded.size), (Some(3), Some(99)));
        assert_eq!(files[1].pointer, None, "older than 90 days");
        assert_eq!(files[2].pointer, None, "no transit key");
        assert_eq!(counts.unavailable_files, 2);
        assert_eq!((files[0].position, files[1].position, files[2].position), (0, 1, 2));
    }

    #[test]
    fn a_timer_update_becomes_a_chat_event() {
        let mut imp = with_alice();
        let mut item = chat_item(
            10,
            1,
            9_000,
            backup::chat_item::Item::UpdateMessage(backup::ChatUpdateMessage {
                update: Some(backup::chat_update_message::Update::ExpirationTimerChange(
                    backup::ExpirationTimerChatUpdate {
                        expiresInMs: 86_400_000,
                        ..Default::default()
                    },
                )),
                ..Default::default()
            }),
        );
        item.directionalDetails = Some(backup::chat_item::DirectionalDetails::Directionless(Default::default()));
        imp.add(frame(backup::frame::Item::ChatItem(item)));
        let (batch, _, counts) = imp.finish();
        assert_eq!(
            batch.events,
            vec![ImportEvent {
                conversation_id: format!("c:{ALICE}"),
                at: 9_000,
                author: ALICE.into(),
                value: 86_400,
            }]
        );
        assert_eq!(counts.messages, 0);
    }

    #[test]
    fn a_group_is_seeded_with_members_and_title() {
        let mut imp = Importer::new(ME, NOW);
        imp.add(contact_frame(1, ALICE, "Alice"));
        let member = |aci: &str| backup::group::Member {
            userId: uuid(aci).as_bytes().to_vec(),
            joinedAtVersion: 4,
            ..Default::default()
        };
        let title_blob = backup::group::GroupAttributeBlob {
            content: Some(backup::group::group_attribute_blob::Content::Title("Friends".into())),
            ..Default::default()
        };
        let timer_blob = backup::group::GroupAttributeBlob {
            content: Some(backup::group::group_attribute_blob::Content::DisappearingMessagesDuration(3600)),
            ..Default::default()
        };
        imp.add(recipient(
            5,
            backup::recipient::Destination::Group(backup::Group {
                masterKey: vec![0xAB; 32],
                snapshot: MessageField::some(backup::group::GroupSnapshot {
                    title: MessageField::some(title_blob),
                    disappearingMessagesTimer: MessageField::some(timer_blob),
                    members: vec![member(ALICE), member(BOB)],
                    version: 7,
                    ..Default::default()
                }),
                ..Default::default()
            }),
        ));
        imp.add(chat_frame(20, 5, 0));
        let (batch, seed, _) = imp.finish();
        let gid = format!("g:{}", "ab".repeat(32));
        assert_eq!(
            batch.conversations,
            vec![ImportConversation {
                id: gid,
                title: "Friends".into(),
                expire_timer: 0,
                ..Default::default()
            }]
        );
        let (master_key, group) = &seed.groups[0];
        assert_eq!(master_key, &[0xAB; 32]);
        assert_eq!(group.title, "Friends");
        assert_eq!(group.disappearing_messages_timer, Some(Timer { duration: 3600 }));
        assert_eq!(group.revision, 7);
        assert_eq!(group.members.len(), 2);
        assert_eq!(group.members[0].profile_key.bytes, [3u8; 32], "Alice's key joined");
        assert_eq!(group.members[1].profile_key.bytes, [0u8; 32], "Bob is unknown: zeroed");
    }

    #[test]
    fn note_to_self_lands_in_our_own_chat() {
        let mut imp = Importer::new(ME, NOW);
        imp.add(self_frame(2));
        imp.add(chat_frame(30, 2, 0));
        let mut item = chat_item(30, 2, 1_000, standard("note"));
        item.directionalDetails = Some(backup::chat_item::DirectionalDetails::Outgoing(Default::default()));
        imp.add(frame(backup::frame::Item::ChatItem(item)));
        let (batch, _, _) = imp.finish();
        let m = &batch.messages[0].message;
        assert_eq!(
            (m.conversation_id.as_str(), m.author.as_str(), m.is_outgoing),
            (format!("c:{ME}").as_str(), ME, true)
        );
    }

    #[test]
    fn batches_cut_at_the_limit() {
        let mut imp = with_alice();
        let mut cut = None;
        for i in 0..BATCH {
            let got = imp.add(incoming_text(10, 1, 1_000 + i as u64, "hi", true));
            if got.is_some() {
                assert!(cut.is_none(), "only one cut");
                cut = got;
            }
        }
        let cut = cut.expect("one full batch");
        assert_eq!(cut.len(), BATCH);
        let (rest, _, counts) = imp.finish();
        assert_eq!(cut.messages.len() + rest.messages.len(), BATCH);
        assert_eq!(counts.messages, BATCH as u64);
    }

    #[test]
    fn quotes_come_along() {
        let mut imp = with_alice();
        let sm = backup::StandardMessage {
            text: MessageField::some(backup::Text {
                body: "answer".into(),
                ..Default::default()
            }),
            quote: MessageField::some(backup::Quote {
                targetSentTimestamp: Some(500),
                authorId: 2,
                text: MessageField::some(backup::Text {
                    body: "question".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        imp.add(frame(backup::frame::Item::ChatItem(chat_item(
            10,
            1,
            1_000,
            backup::chat_item::Item::StandardMessage(sm),
        ))));
        let (batch, _, _) = imp.finish();
        assert_eq!(
            batch.messages[0].message.quote,
            Some(Quote {
                author: ME.into(),
                sent_at: 500,
                text: Some("question".into())
            })
        );
    }

    /// An edited message's identity is its original timestamp: that is what live reactions,
    /// edits and deletes target (through message_revisions), and what a copy already here is
    /// keyed by.
    #[test]
    fn edits_keep_only_the_newest_body() {
        let mut imp = with_alice();
        let mut item = chat_item(10, 1, 3_000, standard("newest"));
        item.revisions = vec![
            chat_item(10, 1, 1_000, standard("original")),
            chat_item(10, 1, 2_000, standard("middle")),
        ];
        imp.add(frame(backup::frame::Item::ChatItem(item)));
        let (batch, _, _) = imp.finish();
        assert_eq!(batch.messages.len(), 1, "revisions are not separate rows");
        let m = &batch.messages[0];
        assert_eq!(
            (m.message.sent_at, m.message.body.as_deref(), m.message.edited),
            (1_000, Some("newest"), true),
            "keyed by the original send time, showing the newest body"
        );
        assert_eq!(m.edited_at, 3_000, "a later live edit must still win");
        assert_eq!(
            m.revisions,
            vec![2_000, 3_000],
            "later ops may target any revision's timestamp"
        );
    }
}
