# Setting the disappearing-message timer (M4 design)

## Context
#46 honours timers and shows them; it couldn't change one. Issue #53.

A direct chat's timer is a `DataMessage` with `flags = EXPIRATION_TIMER_UPDATE` and
`expireTimer` (0: off). presage fills in `expireTimerVersion` (its stored version + 1, so newer
clients accept it) and stores the new timer for our next messages. A group's timer lives in the
group itself and changes through a group change, which presage can't send (no
`PATCH /v2/groups`): groups keep showing their timer, read only.

## Design
- Core: `Client.set_disappearing_timer(id, seconds)`, direct chats only (`InvalidInput`
  otherwise), queued as `Job::Timer`. presage versions and keeps a timer only for a contact it
  knows (without one it would send version 1, which a phone with a later one ignores), so the
  job needs a known contact. It applies here (the projector's `SetTimer`, so the chat shows "You
  set the disappearing message time to …") only once sent and held by presage for our next
  messages; otherwise the user is told and both sides keep the old timer.
- App: an hourglass menu in the header of a direct chat (not blocked, not a message request, not
  a phone-number-only chat), with Desktop's choices: Off, 30 seconds, 5 minutes, 1 hour,
  8 hours, 1 day, 1 week, 4 weeks (and a custom one set from the phone); the current one
  checked; filled when a timer is on.

## Testing
- send: the update carries the timer (0 for off) and applies like an incoming one.
- client: groups and non-chats refused, a direct chat accepted.
- App: the choices read like Desktop's; choosing one asks the core; header checked visually.
