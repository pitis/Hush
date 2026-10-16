//! Chat settings follow the phone through Signal's Storage Service: the account's encrypted,
//! server-stored records (contacts, groups, the account) that every device reads and writes.
//! Spec: `docs/superpowers/specs/2026-10-16-m4-storage-sync-design.md`.
#![allow(dead_code)] // wired into the actor by a later task of the same plan

use crate::projection::ChatFlags;

pub(crate) mod client;
pub(crate) mod records;
pub(crate) mod sync;

/// `ManifestRecord.Identifier.Type` values we understand.
pub(crate) const KIND_CONTACT: i32 = 1;
pub(crate) const KIND_GROUPV2: i32 = 3;
pub(crate) const KIND_ACCOUNT: i32 = 4;

/// What a storage record means to us.
#[derive(Debug, PartialEq)]
pub(crate) enum Remote {
    /// A contact's or group's chat and its flags.
    Chat { conversation_id: String, flags: ChatFlags },
    /// The account: the pinned list, and Note to Self's (archived, marked unread).
    Account { pins: Vec<Pin>, note_to_self: (bool, bool) },
    /// A record we don't interpret (a contact without an ACI, an unknown type…): kept as is.
    Other,
}

/// One entry of the account's pinned list.
#[derive(Debug, PartialEq, Clone)]
pub(crate) enum Pin {
    /// A chat we can name (`c:<aci>` or `g:<hex master key>`).
    Chat(String),
    /// An entry we can't map (an e164-only contact…): its encoded `PinnedConversation`, carried
    /// unchanged when we write.
    Raw(Vec<u8>),
}
