# History import merges into existing data (M2, #18)

- **Date:** 2026-10-16
- **Status:** accepted, to be validated by testing on a real phone
- **Decided by:** the owner

## Context

Signal's phone offers "Transfer message history" only while a device is being linked
(link-and-sync, `capabilities=backup5`). The phone uploads an encrypted archive; the new device
downloads it and imports it. The archive holds pointers to files, not the files.

## Decisions

1. **Import straight into Semaphore's own database (projection.db).** Only what presage needs to
   send (contacts with profile keys, group master keys) goes into presage's store. History is not
   replayed through the live message pipeline and not kept in a separate database. This matches
   Signal Desktop, whose importer writes straight into its own database.
2. **Relinking keeps existing data and merges the imported history in.** "Relink to transfer
   history" resets only presage's store, because the Mac becomes a new linked device; projection.db
   is kept. Messages are deduplicated by (author, sent_at), the same key live messages use.
   - **This is not what Desktop does.** Desktop asks for history only on a fresh install
     (`isLinkAndSyncEnabled()` is false once an install has registered), and unlinking wipes it.
   - **Why:** nothing that is on the Mac but no longer on the phone is lost.
   - **Risk:** merge edge cases, such as an item already here being imported with different state.
     Test on a real phone. If merging misbehaves, fall back to Desktop's behaviour: wipe on
     relink, then import into an empty database.
3. **Media: recent only, like Desktop.** Files whose transit-CDN upload is recent enough (about
   45 days) download as usual. Older ones show as no longer available. Desktop's
   attachment backfill (asking the phone to upload a file again) is out of scope for M2.

## References

- Issue #18 (M2 epic)
- Signal Desktop: `ts/services/backups/import.preload.ts`, `ts/textsecure/Provisioner.preload.ts`
- Signal-Server: `DeviceController` `GET /v1/devices/transfer_archive`
