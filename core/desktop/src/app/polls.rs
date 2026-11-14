//! Polls in their bubbles (the Mac app's `PollView.swift`): each option with its votes, ours
//! ticked; a click votes, and the author can end an open poll. Creating polls isn't here (nor on
//! the Mac). A vote or an end shows once sent: the core then says the message changed.

use std::collections::{HashMap, HashSet};

use gpui_kit::*;
use sig_core::projection::{Message, MessageDetails, MessageKey, MessageKind, Poll};

use super::Hush;

mod view;

pub(super) use view::card;

type Key = (String, i64);

/// The open chat's polls, as their votes stand.
#[derive(Default)]
pub(super) struct Polls {
    by_message: HashMap<Key, Poll>,
}

impl Polls {
    /// Some messages' details were read: their polls as read, the others' as they were (a message
    /// the core couldn't read isn't in them). True if any changed.
    fn loaded(&mut self, details: &[MessageDetails]) -> bool {
        let mut changed = false;
        for d in details {
            let k = (d.key.author.clone(), d.key.sent_at);
            changed |= match &d.poll {
                Some(p) => self.by_message.insert(k, p.clone()).as_ref() != Some(p),
                None => self.by_message.remove(&k).is_some(),
            };
        }
        changed
    }

    /// Only the `loaded` messages' polls stay.
    pub(super) fn keep(&mut self, loaded: &HashSet<Key>) {
        self.by_message.retain(|k, _| loaded.contains(k));
    }
}

/// A poll as its bubble shows it.
pub(super) struct Card {
    pub key: MessageKey,
    pub question: Option<String>,
    pub poll: Poll,
    /// Our ACI: who is ticked.
    pub me: Option<String>,
    /// Ours: we can end it.
    pub mine: bool,
}

impl Card {
    /// Votes are taken: open, and we're linked.
    pub fn votable(&self) -> bool {
        !self.poll.ended && self.me.is_some()
    }

    /// "End poll" shows: ours, still open.
    pub fn endable(&self) -> bool {
        self.mine && !self.poll.ended
    }

    /// Whether we chose option `i`.
    pub fn chosen(&self, i: usize) -> bool {
        let voters = self
            .poll
            .options
            .get(i)
            .map(|o| o.voters.as_slice())
            .unwrap_or_default();
        self.me.as_ref().is_some_and(|me| voters.contains(me))
    }
}

/// Our options after clicking `index` (`PollChoice.after`): in a single-choice poll the click is
/// the choice (on our choice it takes the vote back); with several, it toggles that option.
pub fn after(index: u32, poll: &Poll, me: &str) -> Vec<u32> {
    let mine: Vec<u32> = (0..poll.options.len() as u32)
        .filter(|&i| poll.options[i as usize].voters.iter().any(|v| v == me))
        .collect();
    if poll.allow_multiple {
        let mut chosen: Vec<u32> = mine.iter().copied().filter(|&i| i != index).collect();
        if !mine.contains(&index) {
            chosen.push(index);
            chosen.sort_unstable();
        }
        return chosen;
    }
    if mine == [index] {
        vec![]
    } else {
        vec![index]
    }
}

/// How many voted: each voter once, whatever they chose.
fn voters(poll: &Poll) -> usize {
    let all = poll.options.iter().flat_map(|o| &o.voters);
    all.collect::<HashSet<_>>().len()
}

/// "3 votes", "No votes · Ended" (`PollChoice.summary`): each voter once.
pub fn summary(poll: &Poll) -> String {
    let votes = match voters(poll) {
        0 => "No votes".to_string(),
        1 => "1 vote".to_string(),
        n => format!("{n} votes"),
    };
    if poll.ended {
        format!("{votes} · Ended")
    } else {
        votes
    }
}

/// "1 vote", "2 votes": an option's.
pub fn votes(n: usize) -> String {
    match n {
        1 => "1 vote".into(),
        n => format!("{n} votes"),
    }
}

impl Hush {
    /// Some of the open chat's messages' details were read: their polls show.
    pub(super) fn polls_loaded(&mut self, details: &[MessageDetails], cx: &mut Context<Self>) {
        if self.polls.loaded(details) {
            // a card came in for its words, or votes changed: its row's height may have too
            self.timeline.update(cx, |t, cx| t.remeasure(cx));
        }
    }

    /// A poll's card in place of its words, once its votes are read; none for a deleted message.
    pub(super) fn poll_card(&self, m: &Message) -> Option<Card> {
        let MessageKind::Poll { question } = &m.kind else {
            return None;
        };
        if m.deleted {
            return None;
        }
        let poll = self.polls.by_message.get(&(m.author.clone(), m.sent_at))?.clone();
        let me = self.own_aci.clone();
        Some(Card {
            key: m.key(),
            question: question.clone(),
            mine: me.as_deref() == Some(m.author.as_str()),
            poll,
            me,
        })
    }

    /// A click on option `index` of the poll `key`: our vote after it goes (none takes it back).
    pub(super) fn vote(&mut self, key: &MessageKey, index: u32, cx: &mut Context<Self>) {
        let Some(me) = self.own_aci.clone() else { return };
        let Some(poll) = self.polls.by_message.get(&(key.author.clone(), key.sent_at)) else {
            return;
        };
        if poll.ended {
            return;
        }
        let (key, options) = (key.clone(), after(index, poll, &me));
        self.change("vote", cx, move |client| async move { client.vote(key, options).await });
    }

    /// "End poll" on ours.
    pub(super) fn end_poll(&mut self, key: &MessageKey, cx: &mut Context<Self>) {
        let key = key.clone();
        self.change(
            "end the poll",
            cx,
            move |client| async move { client.end_poll(key).await },
        );
    }
}

#[cfg(feature = "snapshot")]
impl Hush {
    /// `--snapshot`'s polls: their messages (with view-once media), and their votes.
    pub(super) fn demo_polls(&mut self, cx: &mut Context<Self>) {
        let (messages, polls) = crate::demo::poll_messages();
        for (key, poll) in polls {
            self.polls.by_message.insert((key.author, key.sent_at), poll);
        }
        self.files = super::files::Files::demo(crate::demo::view_once_files(&messages));
        self.set_messages(messages, cx);
    }
}

#[cfg(test)]
mod tests {
    use sig_core::projection::{MessageDetails, MessageKey, Poll, PollOption};

    use super::{after, summary, votes, Card, Polls};

    fn poll(voters: &[&[&str]], multiple: bool, ended: bool) -> Poll {
        Poll {
            options: voters
                .iter()
                .enumerate()
                .map(|(i, v)| PollOption {
                    text: format!("o{i}"),
                    voters: v.iter().map(|s| s.to_string()).collect(),
                })
                .collect(),
            allow_multiple: multiple,
            ended,
        }
    }

    #[test]
    fn a_single_choice_click_chooses_switches_or_takes_back() {
        assert_eq!(after(1, &poll(&[&[], &[]], false, false), "me"), [1]);
        // a vote on another changes it
        assert_eq!(after(1, &poll(&[&["me"], &[]], false, false), "me"), [1]);
        // on ours: taken back
        assert!(after(0, &poll(&[&["me"], &[]], false, false), "me").is_empty());
        // others' votes aren't ours
        assert_eq!(after(0, &poll(&[&["ana"], &["me"]], false, false), "me"), [0]);
    }

    #[test]
    fn with_several_choices_a_click_toggles_that_option() {
        assert_eq!(after(1, &poll(&[&["me"], &[], &[]], true, false), "me"), [0, 1]);
        assert_eq!(after(0, &poll(&[&["me"], &["me"]], true, false), "me"), [1]);
        assert_eq!(after(2, &poll(&[&[], &[], &[]], true, false), "me"), [2]);
        assert!(after(0, &poll(&[&["me"], &[]], true, false), "me").is_empty());
    }

    #[test]
    fn the_summary_counts_each_voter_once_and_says_when_it_ended() {
        assert_eq!(summary(&poll(&[&["a", "b"], &["a"]], true, false)), "2 votes");
        assert_eq!(summary(&poll(&[&["a"], &[]], false, false)), "1 vote");
        assert_eq!(summary(&poll(&[&[], &[]], false, true)), "No votes · Ended");
        assert_eq!(votes(0), "0 votes");
        assert_eq!(votes(1), "1 vote");
    }

    fn card(p: Poll, me: Option<&str>, mine: bool) -> Card {
        Card {
            key: MessageKey {
                author: "a".into(),
                sent_at: 1,
            },
            question: Some("Lunch?".into()),
            poll: p,
            me: me.map(Into::into),
            mine,
        }
    }

    #[test]
    fn an_ended_poll_takes_no_votes_and_only_its_author_ends_an_open_one() {
        let open = poll(&[&["me"], &[]], false, false);
        let theirs = card(open.clone(), Some("me"), false);
        assert!(theirs.votable() && !theirs.endable());
        assert!(theirs.chosen(0) && !theirs.chosen(1) && !theirs.chosen(7));
        let ours = card(open.clone(), Some("me"), true);
        assert!(ours.votable() && ours.endable());
        let ended = card(poll(&[&["me"], &[]], false, true), Some("me"), true);
        assert!(!ended.votable() && !ended.endable(), "ended: no votes, nothing to end");
        assert!(ended.chosen(0), "our vote still shows");
        let unlinked = card(open, None, false);
        assert!(!unlinked.votable() && !unlinked.chosen(0));
    }

    fn details(sent_at: i64, poll: Option<Poll>) -> MessageDetails {
        MessageDetails {
            key: MessageKey {
                author: "a".into(),
                sent_at,
            },
            reactions: vec![],
            attachments: vec![],
            preview: None,
            poll,
        }
    }

    #[test]
    fn polls_come_from_the_details_and_go_with_their_rows() {
        let mut polls = Polls::default();
        let open = poll(&[&[], &[]], false, false);
        assert!(polls.loaded(&[details(1, Some(open.clone())), details(2, None)]));
        assert!(!polls.loaded(&[details(1, Some(open.clone()))]), "the same votes");
        let voted = poll(&[&["ana"], &[]], false, false);
        assert!(polls.loaded(&[details(1, Some(voted.clone()))]), "a vote came");
        assert_eq!(polls.by_message.get(&("a".into(), 1)), Some(&voted));
        polls.keep(&[("a".to_string(), 2)].into());
        assert!(polls.by_message.is_empty(), "its row went");
    }
}

/// Voting in a real (headless) window: `cargo test -p hush-desktop --features snapshot`. With a
/// core that isn't linked, a vote fails at once: its message shows it was asked.
#[cfg(all(test, feature = "snapshot"))]
mod window_tests {
    use std::time::Duration;

    use gpui_kit::{gpui, AppContext as _, Entity, TestAppContext, WindowOptions};

    use super::super::attach::window_tests::with_core;
    use super::super::Hush;
    use crate::Demo;

    fn said(hush: &Entity<Hush>, cx: &mut TestAppContext) -> String {
        for _ in 0..200 {
            cx.run_until_parked();
            if let Some(error) = hush.update(cx, |app, _| app.error.take()) {
                return error;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("nothing was said");
    }

    #[gpui::test]
    fn a_vote_and_an_end_ask_the_core_but_an_ended_poll_takes_none(cx: &mut TestAppContext) {
        let (_, hush) = cx.update(|cx| {
            gpui_kit::init(cx);
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Hush::demo(Demo::Polls, window, cx))
            })
            .expect("the window opens")
        });
        let data = std::env::temp_dir().join(format!("hush-desktop-polls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        with_core(&hush, &data, cx);
        cx.executor().allow_parking();
        let cards = hush.read_with(cx, |app, _| {
            let cards = app.messages.iter().filter_map(|m| app.poll_card(m));
            cards.map(|c| (c.key.clone(), c.poll.ended, c.mine)).collect::<Vec<_>>()
        });
        let (open, _, _) = cards
            .iter()
            .find(|(_, ended, _)| !ended)
            .cloned()
            .expect("an open poll");
        let (ended, _, _) = cards
            .iter()
            .find(|(_, ended, _)| *ended)
            .cloned()
            .expect("an ended poll");
        let (ours, _, _) = cards
            .iter()
            .find(|(_, ended, mine)| !ended && *mine)
            .cloned()
            .expect("ours");

        hush.update(cx, |app, cx| app.vote(&open, 1, cx));
        assert!(said(&hush, cx).starts_with("Couldn't vote: "));
        hush.update(cx, |app, cx| app.end_poll(&ours, cx));
        assert!(said(&hush, cx).starts_with("Couldn't end the poll: "));
        // nothing is sent, so nothing fails
        hush.update(cx, |app, cx| app.vote(&ended, 0, cx));
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(100));
        cx.run_until_parked();
        assert_eq!(hush.read_with(cx, |app, _| app.error.clone()), None);
        let _ = std::fs::remove_dir_all(&data);
    }
}
