//! Storage records as we read them, and as we rewrite them. A write patches the record's raw
//! bytes (drops the fields we own, appends ours) so every field our proto doesn't know survives;
//! a decode–encode round trip through prost would lose them.

use crate::projection::ChatFlags;
use crate::storage::{Pin, Remote};
use presage::libsignal_service::prelude::{ProtobufMessage as _, Uuid};
use presage::libsignal_service::proto::{
    account_record::{pinned_conversation, PinnedConversation},
    storage_record::Record,
    ContactRecord, StorageRecord,
};

/// `StorageRecord`'s oneof field numbers.
const OUTER_CONTACT: u32 = 1;
const OUTER_GROUPV2: u32 = 3;
const OUTER_ACCOUNT: u32 = 4;
/// archived, markedUnread, mutedUntilTimestamp, blocked, whitelisted in `ContactRecord` and
/// `GroupV2Record`.
const CONTACT_FLAGS: [u32; 5] = [11, 12, 13, 9, 10];
const GROUP_FLAGS: [u32; 5] = [4, 5, 6, 2, 3];
/// `AccountRecord`: noteToSelfArchived, noteToSelfMarkedUnread, pinnedConversations.
const ACCOUNT_NOTE_ARCHIVED: u32 = 5;
const ACCOUNT_NOTE_MARKED_UNREAD: u32 = 10;
const ACCOUNT_PINNED: u32 = 14;

/// The name the phone shows for a contact: the nickname set for them, else their address-book
/// name (as Signal: nickname, then system contact, then profile).
fn contact_name(c: &ContactRecord) -> Option<String> {
    let join = |given: &str, family: &str| {
        let full = [given.trim(), family.trim()]
            .iter()
            .filter(|p| !p.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" ");
        Some(full).filter(|f| !f.is_empty())
    };
    c.nickname
        .as_ref()
        .and_then(|n| join(&n.given, &n.family))
        .or_else(|| join(&c.system_given_name, &c.system_family_name))
        .or_else(|| Some(c.system_nickname.trim().to_string()).filter(|n| !n.is_empty()))
}

/// What a decrypted record means to us. `Err` only for bytes that aren't a protobuf message.
pub(crate) fn decode(plaintext: &[u8]) -> Result<Remote, String> {
    let record = StorageRecord::decode(plaintext).map_err(|e| format!("storage record: {e}"))?;
    Ok(match record.record {
        Some(Record::Contact(c)) => match contact_aci(&c) {
            Some(aci) => Remote::Chat {
                conversation_id: format!("c:{aci}"),
                flags: flags(
                    c.archived,
                    c.marked_unread,
                    c.muted_until_timestamp,
                    c.blocked,
                    c.whitelisted,
                ),
                phone_contact: !(c.system_given_name.is_empty()
                    && c.system_family_name.is_empty()
                    && c.system_nickname.is_empty()),
                username: Some(c.username.clone()).filter(|u| !u.is_empty()),
                name: contact_name(&c),
                note: Some(c.note.clone()).filter(|n| !n.trim().is_empty()),
            },
            None => Remote::Other,
        },
        Some(Record::GroupV2(g)) if g.master_key.len() == 32 => Remote::Chat {
            conversation_id: format!("g:{}", hex::encode(&g.master_key)),
            flags: flags(
                g.archived,
                g.marked_unread,
                g.muted_until_timestamp,
                g.blocked,
                g.whitelisted,
            ),
            phone_contact: false,
            username: None,
            name: None,
            note: None,
        },
        Some(Record::Account(a)) => Remote::Account {
            pins: a.pinned_conversations.iter().map(pin_of).collect(),
            note_to_self: (a.note_to_self_archived, a.note_to_self_marked_unread),
        },
        _ => Remote::Other,
    })
}

fn flags(archived: bool, marked_unread: bool, muted_until: u64, blocked: bool, whitelisted: bool) -> ChatFlags {
    ChatFlags {
        archived,
        marked_unread,
        // INT64_MAX (and anything past it) is "always"
        mute_until: i64::try_from(muted_until).unwrap_or(i64::MAX),
        blocked,
        whitelisted,
    }
}

/// The contact's ACI as our ids spell it (lowercase, hyphenated), from either encoding.
fn contact_aci(c: &ContactRecord) -> Option<String> {
    let uuid = if c.aci.is_empty() {
        Uuid::from_slice(&c.aci_binary).ok()?
    } else {
        Uuid::parse_str(&c.aci).ok()?
    };
    Some(uuid.hyphenated().to_string())
}

/// A pinned entry as a chat id, or its bytes when we can't name it (an e164-only contact, a PNI,
/// a legacy group, the release notes…).
fn pin_of(p: &PinnedConversation) -> Pin {
    use pinned_conversation::Identifier as I;
    let chat = match &p.identifier {
        Some(I::Contact(c)) => {
            let uuid = if c.service_id.is_empty() {
                Uuid::from_slice(&c.service_id_binary).ok()
            } else {
                Uuid::parse_str(&c.service_id).ok()
            };
            uuid.map(|u| format!("c:{}", u.hyphenated()))
        }
        Some(I::GroupMasterKey(k)) if k.len() == 32 => Some(format!("g:{}", hex::encode(k))),
        _ => None,
    };
    match chat {
        Some(id) => Pin::Chat(id),
        None => Pin::Raw(p.encode_to_vec()),
    }
}

/// A contact's or group's record with our flags in place of its own; every other byte as it
/// was.
pub(crate) fn patch_chat(plaintext: &[u8], f: &ChatFlags) -> Result<Vec<u8>, String> {
    let (outer, inner) = wire::first_field(plaintext)?;
    let fields = match outer {
        OUTER_CONTACT => CONTACT_FLAGS,
        OUTER_GROUPV2 => GROUP_FLAGS,
        other => return Err(format!("not a chat record (field {other})")),
    };
    let mut inner = wire::strip_fields(inner, &fields)?;
    // proto3: a false or zero value is simply absent
    if f.archived {
        wire::append_varint(&mut inner, fields[0], 1);
    }
    if f.marked_unread {
        wire::append_varint(&mut inner, fields[1], 1);
    }
    if f.mute_until > 0 {
        wire::append_varint(&mut inner, fields[2], f.mute_until as u64);
    }
    if f.blocked {
        wire::append_varint(&mut inner, fields[3], 1);
    }
    if f.whitelisted {
        wire::append_varint(&mut inner, fields[4], 1);
    }
    Ok(wrap(outer, &inner))
}

/// The account record with the pinned list and Note to Self's flags replaced; every other byte
/// as it was.
pub(crate) fn patch_account(plaintext: &[u8], pins: &[Pin], note_to_self: (bool, bool)) -> Result<Vec<u8>, String> {
    let (outer, inner) = wire::first_field(plaintext)?;
    if outer != OUTER_ACCOUNT {
        return Err(format!("not the account record (field {outer})"));
    }
    let mut inner = wire::strip_fields(
        inner,
        &[ACCOUNT_NOTE_ARCHIVED, ACCOUNT_NOTE_MARKED_UNREAD, ACCOUNT_PINNED],
    )?;
    for p in pins {
        let entry = match p {
            Pin::Chat(id) => pinned_entry(id)?.encode_to_vec(),
            Pin::Raw(bytes) => bytes.clone(),
        };
        wire::append_bytes(&mut inner, ACCOUNT_PINNED, &entry);
    }
    if note_to_self.0 {
        wire::append_varint(&mut inner, ACCOUNT_NOTE_ARCHIVED, 1);
    }
    if note_to_self.1 {
        wire::append_varint(&mut inner, ACCOUNT_NOTE_MARKED_UNREAD, 1);
    }
    Ok(wrap(outer, &inner))
}

fn pinned_entry(id: &str) -> Result<PinnedConversation, String> {
    use pinned_conversation::{Contact, Identifier as I};
    let identifier = if let Some(aci) = id.strip_prefix("c:") {
        I::Contact(Contact {
            service_id: aci.into(),
            ..Default::default()
        })
    } else if let Some(key) = id.strip_prefix("g:") {
        I::GroupMasterKey(hex::decode(key).map_err(|e| format!("group id: {e}"))?)
    } else {
        return Err(format!("not a chat id: {id}"));
    };
    Ok(PinnedConversation {
        identifier: Some(identifier),
    })
}

fn wrap(outer: u32, inner: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(inner.len() + 6);
    wire::append_bytes(&mut out, outer, inner);
    out
}

/// Just enough of the protobuf wire format to drop and append top-level fields.
pub(crate) mod wire {
    /// (field number, start, end) of each top-level field, in order.
    fn fields(msg: &[u8]) -> Result<Vec<(u32, usize, usize)>, String> {
        let mut out = vec![];
        let mut i = 0;
        while i < msg.len() {
            let start = i;
            let (tag, n) = varint(msg, i)?;
            i += n;
            let field = u32::try_from(tag >> 3).map_err(|_| "field number too large".to_string())?;
            let advance = match tag & 7 {
                0 => varint(msg, i)?.1,
                1 => 8,
                2 => {
                    let (len, n) = varint(msg, i)?;
                    usize::try_from(len)
                        .ok()
                        .and_then(|len| len.checked_add(n))
                        .ok_or_else(|| "length too large".to_string())?
                }
                5 => 4,
                other => return Err(format!("unsupported wire type {other}")),
            };
            i = i.checked_add(advance).ok_or_else(|| "length too large".to_string())?;
            if i > msg.len() {
                return Err("truncated field".into());
            }
            out.push((field, start, i));
        }
        Ok(out)
    }

    fn varint(msg: &[u8], at: usize) -> Result<(u64, usize), String> {
        let mut value = 0u64;
        for (n, &b) in msg.get(at..).unwrap_or(&[]).iter().enumerate() {
            if n >= 10 {
                return Err("varint too long".into());
            }
            value |= u64::from(b & 0x7F) << (7 * n);
            if b & 0x80 == 0 {
                return Ok((value, n + 1));
            }
        }
        Err("truncated varint".into())
    }

    /// `msg` without the top-level fields numbered in `drop`, every other byte as it was.
    pub fn strip_fields(msg: &[u8], drop: &[u32]) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(msg.len());
        for (field, start, end) in fields(msg)? {
            if !drop.contains(&field) {
                out.extend_from_slice(&msg[start..end]);
            }
        }
        Ok(out)
    }

    /// The one length-delimited field of a message that has exactly one: its number and payload.
    pub fn first_field(msg: &[u8]) -> Result<(u32, &[u8]), String> {
        let all = fields(msg)?;
        let [(field, _, _)] = all[..] else {
            return Err(format!("{} top-level fields, expected one", all.len()));
        };
        let (tag, n) = varint(msg, 0)?;
        if tag & 7 != 2 {
            return Err("not a length-delimited field".into());
        }
        let (len, m) = varint(msg, n)?;
        let start = n + m;
        Ok((field, &msg[start..start + len as usize]))
    }

    fn put_varint(out: &mut Vec<u8>, mut v: u64) {
        loop {
            let b = (v & 0x7F) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return;
            }
            out.push(b | 0x80);
        }
    }

    pub fn append_varint(out: &mut Vec<u8>, field: u32, v: u64) {
        put_varint(out, u64::from(field) << 3);
        put_varint(out, v);
    }

    pub fn append_bytes(out: &mut Vec<u8>, field: u32, b: &[u8]) {
        put_varint(out, (u64::from(field) << 3) | 2);
        put_varint(out, b.len() as u64);
        out.extend_from_slice(b);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::libsignal_service::proto::{AccountRecord, GroupV2Record};

    const ALICE: &str = "22222222-2222-2222-2222-222222222222";

    fn contact(aci: &str) -> Vec<u8> {
        StorageRecord {
            record: Some(Record::Contact(ContactRecord {
                aci: aci.into(),
                archived: true,
                muted_until_timestamp: u64::MAX,
                blocked: true,
                whitelisted: false,
                given_name: "A".into(),
                ..Default::default()
            })),
        }
        .encode_to_vec()
    }

    fn wrap(record: Record) -> Vec<u8> {
        StorageRecord { record: Some(record) }.encode_to_vec()
    }

    #[test]
    fn a_contact_record_gives_the_chat_and_its_flags() {
        let Remote::Chat {
            conversation_id, flags, ..
        } = decode(&contact(ALICE)).unwrap()
        else {
            panic!()
        };
        assert_eq!(conversation_id, format!("c:{ALICE}"));
        assert_eq!(
            (
                flags.archived,
                flags.marked_unread,
                flags.mute_until,
                flags.blocked,
                flags.whitelisted
            ),
            (true, false, i64::MAX, true, false)
        );
    }

    #[test]
    fn a_contact_is_called_by_its_nickname_else_its_address_book_name() {
        use presage::libsignal_service::proto::contact_record::Name;
        let record = |nickname: Option<(&str, &str)>, system: (&str, &str)| {
            wrap(Record::Contact(ContactRecord {
                aci: ALICE.into(),
                nickname: nickname.map(|(given, family)| Name {
                    given: given.into(),
                    family: family.into(),
                }),
                system_given_name: system.0.into(),
                system_family_name: system.1.into(),
                note: "met at the conference".into(),
                ..Default::default()
            }))
        };
        let name_of = |bytes: Vec<u8>| match decode(&bytes).unwrap() {
            Remote::Chat { name, note, .. } => (name, note),
            _ => panic!(),
        };
        assert_eq!(
            name_of(record(Some(("Ali", "")), ("Alice", "Smith"))),
            (Some("Ali".into()), Some("met at the conference".into()))
        );
        assert_eq!(name_of(record(None, ("Alice", "Smith"))).0, Some("Alice Smith".into()));
        assert_eq!(name_of(record(Some(("", "")), ("", ""))).0, None);
    }

    #[test]
    fn a_contacts_username_comes_along() {
        let named = wrap(Record::Contact(ContactRecord {
            aci: ALICE.into(),
            username: "alice.42".into(),
            ..Default::default()
        }));
        assert!(matches!(decode(&named).unwrap(), Remote::Chat { username: Some(u), .. } if u == "alice.42"));
        assert!(matches!(
            decode(&contact(ALICE)).unwrap(),
            Remote::Chat { username: None, .. }
        ));
    }

    #[test]
    fn a_binary_aci_is_read_and_a_contact_without_aci_is_not_ours() {
        let bin = wrap(Record::Contact(ContactRecord {
            aci_binary: Uuid::parse_str(ALICE).unwrap().as_bytes().to_vec(),
            ..Default::default()
        }));
        assert!(
            matches!(decode(&bin).unwrap(), Remote::Chat { conversation_id, .. } if conversation_id == format!("c:{ALICE}"))
        );
        let pni_only = wrap(Record::Contact(ContactRecord {
            pni: "x".into(),
            ..Default::default()
        }));
        assert!(matches!(decode(&pni_only).unwrap(), Remote::Other));
        let bad_aci = wrap(Record::Contact(ContactRecord {
            aci: "not-a-uuid".into(),
            ..Default::default()
        }));
        assert!(matches!(decode(&bad_aci).unwrap(), Remote::Other));
    }

    #[test]
    fn a_group_record_gives_the_group_by_master_key() {
        let g = wrap(Record::GroupV2(GroupV2Record {
            master_key: vec![0xAB; 32],
            marked_unread: true,
            whitelisted: true,
            ..Default::default()
        }));
        let Remote::Chat {
            conversation_id, flags, ..
        } = decode(&g).unwrap()
        else {
            panic!()
        };
        assert_eq!(conversation_id, format!("g:{}", "ab".repeat(32)));
        assert!(flags.marked_unread && flags.whitelisted && !flags.archived);
        let short = wrap(Record::GroupV2(GroupV2Record {
            master_key: vec![1; 4],
            ..Default::default()
        }));
        assert!(matches!(decode(&short).unwrap(), Remote::Other));
    }

    #[test]
    fn the_account_record_gives_pins_in_order_and_keeps_what_it_cannot_map() {
        let pin = |id| PinnedConversation { identifier: Some(id) };
        let e164 = pin(pinned_conversation::Identifier::Contact(pinned_conversation::Contact {
            e164: "+1555".into(),
            ..Default::default()
        }));
        let acct = wrap(Record::Account(AccountRecord {
            pinned_conversations: vec![
                pin(pinned_conversation::Identifier::GroupMasterKey(vec![0xCD; 32])),
                e164.clone(),
                pin(pinned_conversation::Identifier::Contact(pinned_conversation::Contact {
                    service_id: ALICE.into(),
                    ..Default::default()
                })),
                pin(pinned_conversation::Identifier::Contact(pinned_conversation::Contact {
                    service_id_binary: Uuid::parse_str(ALICE).unwrap().as_bytes().to_vec(),
                    ..Default::default()
                })),
            ],
            note_to_self_archived: true,
            ..Default::default()
        }));
        let Remote::Account { pins, note_to_self } = decode(&acct).unwrap() else {
            panic!()
        };
        assert_eq!(note_to_self, (true, false));
        assert!(matches!(&pins[0], Pin::Chat(id) if id == &format!("g:{}", "cd".repeat(32))));
        assert!(matches!(&pins[1], Pin::Raw(b) if b == &e164.encode_to_vec()));
        assert!(matches!(&pins[2], Pin::Chat(id) if id == &format!("c:{ALICE}")));
        assert!(matches!(&pins[3], Pin::Chat(id) if id == &format!("c:{ALICE}")));
    }

    #[test]
    fn unknown_record_types_and_garbage_are_other_or_errors() {
        let folder = wrap(Record::ChatFolder(Default::default()));
        assert!(matches!(decode(&folder).unwrap(), Remote::Other));
        assert!(
            matches!(decode(&[]).unwrap(), Remote::Other),
            "an empty record has no oneof"
        );
        assert!(decode(&[0xFF, 0xFF, 0xFF]).is_err());
    }

    #[test]
    fn patching_a_contact_keeps_every_other_byte_including_unknown_fields() {
        // a field number no client knows (900), next to a known one we don't own (givenName)
        let raw = contact(ALICE);
        let (_, inner) = wire::first_field(&raw).unwrap();
        let mut inner = inner.to_vec();
        wire::append_bytes(&mut inner, 900, b"future");
        let mut raw = vec![];
        wire::append_bytes(&mut raw, 1, &inner);
        let patched = patch_chat(
            &raw,
            &ChatFlags {
                archived: false,
                marked_unread: true,
                mute_until: 1_000,
                blocked: false,
                whitelisted: true,
            },
        )
        .unwrap();
        let rec = StorageRecord::decode(&*patched).unwrap();
        let Some(Record::Contact(c)) = rec.record else { panic!() };
        assert_eq!(
            (
                c.archived,
                c.marked_unread,
                c.muted_until_timestamp,
                c.given_name.as_str(),
                c.blocked,
                c.whitelisted
            ),
            (false, true, 1_000, "A", false, true),
            "the fixture's blocked is replaced too"
        );
        let (_, inner2) = wire::first_field(&patched).unwrap();
        assert!(inner2.windows(6).any(|w| w == b"future"), "the unknown field survived");
        // the owned fields occur once each: stripping them again removes exactly those bytes
        let stripped = wire::strip_fields(inner2, &[11, 12, 13, 9, 10]).unwrap();
        // false values are absent; markedUnread 2 bytes, mute 1_000 = tag + 2 bytes, whitelisted 2
        assert_eq!(stripped.len(), inner2.len() - (2 + 3 + 2));
    }

    #[test]
    fn patching_a_group_and_refusing_other_records() {
        let g = wrap(Record::GroupV2(GroupV2Record {
            master_key: vec![1; 32],
            archived: true,
            whitelisted: true,
            ..Default::default()
        }));
        let p = patch_chat(
            &g,
            &ChatFlags {
                mute_until: i64::MAX,
                blocked: true,
                ..Default::default()
            },
        )
        .unwrap();
        let Some(Record::GroupV2(g2)) = StorageRecord::decode(&*p).unwrap().record else {
            panic!()
        };
        assert_eq!(
            (
                g2.archived,
                g2.muted_until_timestamp,
                g2.blocked,
                g2.whitelisted,
                g2.master_key.len()
            ),
            (false, i64::MAX as u64, true, false, 32)
        );
        let acct = wrap(Record::Account(Default::default()));
        assert!(patch_chat(&acct, &ChatFlags::default()).is_err());
        assert!(patch_account(&g, &[], (false, false)).is_err());
    }

    #[test]
    fn patching_the_account_rewrites_pins_and_note_to_self_only() {
        let acct = wrap(Record::Account(AccountRecord {
            pinned_conversations: vec![PinnedConversation {
                identifier: Some(pinned_conversation::Identifier::GroupMasterKey(vec![9; 32])),
            }],
            note_to_self_archived: true,
            read_receipts: true,
            username: "u".into(),
            ..Default::default()
        }));
        let raw_pin = PinnedConversation {
            identifier: Some(pinned_conversation::Identifier::Contact(pinned_conversation::Contact {
                e164: "+1".into(),
                ..Default::default()
            })),
        }
        .encode_to_vec();
        let p = patch_account(
            &acct,
            &[
                Pin::Chat(format!("c:{ALICE}")),
                Pin::Chat(format!("g:{}", "ab".repeat(32))),
                Pin::Raw(raw_pin.clone()),
            ],
            (false, true),
        )
        .unwrap();
        let Some(Record::Account(a)) = StorageRecord::decode(&*p).unwrap().record else {
            panic!()
        };
        assert_eq!(a.pinned_conversations.len(), 3);
        assert!(
            matches!(&a.pinned_conversations[0].identifier, Some(pinned_conversation::Identifier::Contact(c)) if c.service_id == ALICE)
        );
        assert!(
            matches!(&a.pinned_conversations[1].identifier, Some(pinned_conversation::Identifier::GroupMasterKey(k)) if k == &vec![0xAB; 32])
        );
        assert_eq!(a.pinned_conversations[2].encode_to_vec(), raw_pin);
        assert_eq!(
            (
                a.note_to_self_archived,
                a.note_to_self_marked_unread,
                a.read_receipts,
                a.username.as_str()
            ),
            (false, true, true, "u")
        );
    }

    /// `raw` with unknown fields inside its record: a bytes field 900 at the end, and a varint
    /// 901 in the middle when `middle` (between two pinned entries, say).
    fn with_unknown_fields(raw: &[u8], middle: Option<&[u8]>) -> Vec<u8> {
        let (outer, inner) = wire::first_field(raw).unwrap();
        let mut inner = inner.to_vec();
        if let Some(rest) = middle {
            wire::append_varint(&mut inner, 901, 7);
            inner.extend_from_slice(rest);
        }
        wire::append_bytes(&mut inner, 900, b"future");
        let mut out = vec![];
        wire::append_bytes(&mut out, outer, &inner);
        out
    }

    #[test]
    fn patching_the_account_keeps_unknown_fields_byte_for_byte() {
        let one = |k: u8| PinnedConversation {
            identifier: Some(pinned_conversation::Identifier::GroupMasterKey(vec![k; 32])),
        };
        let acct = wrap(Record::Account(AccountRecord {
            pinned_conversations: vec![one(1)],
            username: "u".into(),
            ..Default::default()
        }));
        let mut second = vec![];
        wire::append_bytes(&mut second, 14, &one(2).encode_to_vec());
        let raw = with_unknown_fields(&acct, Some(&second));
        let patched = patch_account(
            &raw,
            &[Pin::Chat(format!("g:{}", hex::encode([3u8; 32])))],
            (true, false),
        )
        .unwrap();
        let (_, before) = wire::first_field(&raw).unwrap();
        let (_, after) = wire::first_field(&patched).unwrap();
        // apart from the fields we own, the record is exactly what it was
        assert_eq!(
            wire::strip_fields(after, &[5, 10, 14]).unwrap(),
            wire::strip_fields(before, &[5, 10, 14]).unwrap()
        );
        let Some(Record::Account(a)) = StorageRecord::decode(&*patched).unwrap().record else {
            panic!()
        };
        assert_eq!((a.pinned_conversations.len(), a.note_to_self_archived), (1, true));
    }

    #[test]
    fn patching_a_group_writes_marked_unread_at_its_number_and_keeps_unknown_fields() {
        let g = with_unknown_fields(
            &wrap(Record::GroupV2(GroupV2Record {
                master_key: vec![1; 32],
                ..Default::default()
            })),
            None,
        );
        let p = patch_chat(
            &g,
            &ChatFlags {
                marked_unread: true,
                ..Default::default()
            },
        )
        .unwrap();
        let Some(Record::GroupV2(g2)) = StorageRecord::decode(&*p).unwrap().record else {
            panic!()
        };
        assert!(g2.marked_unread && !g2.archived && g2.muted_until_timestamp == 0);
        let (_, before) = wire::first_field(&g).unwrap();
        let (_, after) = wire::first_field(&p).unwrap();
        assert_eq!(
            wire::strip_fields(after, &[4, 5, 6]).unwrap(),
            wire::strip_fields(before, &[4, 5, 6]).unwrap()
        );
    }

    #[test]
    fn the_wire_walker_handles_every_scalar_wire_type_and_rejects_groups() {
        let mut m = vec![];
        wire::append_varint(&mut m, 1, 300);
        m.extend_from_slice(&[0x11, 1, 2, 3, 4, 5, 6, 7, 8]); // field 2, 64-bit
        wire::append_bytes(&mut m, 3, b"abc");
        m.extend_from_slice(&[0x25, 1, 2, 3, 4]); // field 4, 32-bit
        let mut kept = vec![];
        wire::append_varint(&mut kept, 1, 300);
        wire::append_bytes(&mut kept, 3, b"abc");
        assert_eq!(wire::strip_fields(&m, &[2, 4]).unwrap(), kept);
        assert_eq!(wire::strip_fields(&m, &[]).unwrap(), m);
        assert!(wire::strip_fields(&[0x0B], &[]).is_err(), "start-group");
        assert!(
            wire::strip_fields(&[0x0A, 0x05, 1], &[]).is_err(),
            "truncated length-delimited"
        );
        assert!(wire::strip_fields(&[0x08], &[]).is_err(), "missing varint");
        assert!(wire::first_field(&m).is_err(), "several fields");
        assert_eq!(wire::first_field(&[0x1A, 0x01, 0x07]).unwrap(), (3, &[7u8][..]));
    }

    #[test]
    fn crypto_round_trip_through_libsignal_service() {
        use presage::libsignal_service::master_key::StorageServiceKey;
        use presage::libsignal_service::StorageService;
        let key = StorageServiceKey::from_slice(&[7u8; 32]).unwrap();
        let ikm = [3u8; 32];
        let raw_id = vec![5u8; 16];
        let item = StorageService::encrypt_item(
            &key,
            raw_id.clone(),
            &StorageRecord::decode(&*contact(ALICE)).unwrap(),
            Some(&ikm),
        );
        let back = StorageService::decrypt_item(&key, &item, Some(&ikm)).unwrap();
        assert!(matches!(decode(&back.encode_to_vec()).unwrap(), Remote::Chat { .. }));
    }
}
