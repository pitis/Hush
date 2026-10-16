# Polls (M4 design)

## Context
#46 shows polls as a placeholder ("📊 Poll: question"). Issue #53: "Polls: render and vote."
Signal's `DataMessage`: `pollCreate {question, allowMultiple, options}`, `pollVote
{targetAuthorAciBinary, targetSentTimestamp, optionIndexes, voteCount}` (each voter's latest
vote, by `voteCount`, replaces the earlier; no indexes: taken back), `pollTerminate
{targetSentTimestamp}` (its author ends it). The backup's `Poll` carries options with their
votes and `hasEnded`.

## Goals
1. A poll shows its question, "Choose one" or "Choose one or more", each option with its votes
   (a bar and a count) and ours ticked, and "N votes" ("· Ended" once ended).
2. A click votes: in a single-choice poll it is the choice (on ours it takes the vote back);
   with several it toggles that option. Ended polls take no votes.
3. Our own open poll has "End poll".
4. Votes and ends from others and from our phone apply here; imported polls keep their votes.
   Creating polls from the Mac is not in this slice.

## Design
- Migration 0015: `polls(author, sent_at, options JSON, allow_multiple, ended)` and
  `poll_votes(target, voter, vote_count, options JSON)`. Ops `PollCreated` (with the poll's
  `InsertMessage`), `PollVote` (kept only if its count is higher), `PollEnded` (the sender's own
  poll; an end before the poll stays). Delete for everyone drops both.
- `ProjectionDb::poll` reads the standing: a single-choice poll counts each voter's first pick;
  picks it doesn't have don't count.
- `Client.vote(key, options)` (checked against the poll: open, its options, one choice unless
  several) sends our next `voteCount`; `Client.end_poll(key)` (ours, open); both apply here once
  sent, like a reaction. `Client.poll(key)` for the app.
- App: `PollCard` in the bubble; `PollChoice` decides what a click votes.

## Testing
- projector: a poll's options, a vote (ours from the phone too), an end aimed at the sender's
  own poll.
- db: latest vote per voter, late older votes ignored, single choice, out-of-range picks, a
  taken-back vote, the end (also before the poll), delete for everyone; import keeps votes and a
  newer live vote wins.
- send: our vote and end apply like incoming ones. client: vote counts, checks, end rules.
- import: options and votes regrouped per voter.
- App: what a click votes, the summary words, the store's calls; cards checked from a snapshot.
