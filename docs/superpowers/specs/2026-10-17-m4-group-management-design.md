# Group management and own profile (M4 design)

## Context
Issue #54, what's left of it: "Groups: create, add and remove members, admins, invite links"
and "Edit your own profile (name, about, avatar)". Avatars (#73) and contact names (#74) are
done. Also from #54: group messages are taken from anyone holding the group's master key,
removed members included.

presage has no group management on main; it is pinned to whisperfish/presage#448 (on
whisperfish/libsignal-service-rs#473), which adds `create_group`, `change_group(GroupEdit)`
(title, description, avatar, timer, add/remove members, admins, leave, accept/decline an
invitation, invite-link access and reset, approve/deny join requests), `group_join_info`,
`join_group`, `cancel_join_request` and `fetch_group`. Neither PR has run against Signal's
servers: Hush is their first real use. presage's `update_profile` sets name and about but can't
upload a profile picture.

presage keeps its group store current: a received group message whose revision is newer than
the stored group's fetches the group before the content reaches us.

## Pieces (one PR each, stacked)
1. **Groups stay current; only members post.** After each group message, the chat's title,
   members and timer are re-read from presage's group when its revision moved (migration:
   `conversations.group_revision`). A group's message, edit, reaction, typing or story from an
   ACI that isn't a member is dropped like a blocked sender's (its presage copy queued to go).
   A message claiming a revision newer than presage's group (presage's refresh failed) fetches
   the group once more, if presage knows the group, neither it nor the sender is blocked, and
   it wasn't fetched again in the last five minutes. The claimed revision never turns the check off (anyone can claim one):
   a stored group's members judge every message, so a just-added member's message is lost only
   if both fetches fail. A group never stored drops nothing. Senders are judged against the
   group's latest members, as Signal's apps do: a message delayed past its sender's removal
   is dropped too.
2. **Group info.** A panel from the chat header: picture, title, description, members (admins
   marked) and invited people. As the group's access control says, members or only admins
   edit title, description, picture and timer, and add members (picked from the people we
   have accepted chats with); admins remove members and make and unmake admins. Anyone
   leaves. Read live from presage's group (`Client::group_info`); each change goes through
   `Client::change_group`, which stores the group in its chat once the server has it. A
   picture is a file, sent as a JPEG of at most 1024 px. Join requests wait for piece 4.
3. **New group.** From the sidebar's compose menu: a name and members picked from the people
   we have accepted chats with (none is fine; members can be added later). Those we have a
   profile key for join, the others are invited (presage's `create_group`). The chat is listed
   and opened at once, before any message. If something fails after the server made the group
   (telling the members), the chat is still listed and opened, the failure shown: it's the
   one group presage didn't have before, under our name at its first revision. A picture is set afterwards in the group's info.
4. **Invitations and invite links.** A group that invites us (our account, or our number's
   PNI) lists its chat whenever it's stored so, unless the inviter or the group is blocked
   (`conversations.group_invited`; an invitation isn't a message request). The chat offers
   Join (which also accepts the chat), Decline (which also deletes it) and Block instead of
   the composer. An answer that took counts as answered even if telling the group failed. In the info panel, admins turn the link on (anyone
   joins, or admins approve) or off, and reset it; members copy it while it's on. It holds
   the group's key, so it is never shown or logged. Admins approve or deny requests to join.
5. **Joining by link.** A pasted `signal.group` link shows the group and joins it, or asks
   to; a pending request can be withdrawn.
6. **Own profile.** Settings shows and edits our name and about. The picture stays the phone's
   (presage can't upload one).

## Testing
Pure parts (membership decisions, revision gating, edit permissions) get unit tests; network
calls are untested as elsewhere in the core and are tried by hand against a real account.
