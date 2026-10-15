//! When to tell a conversation that we are typing: the app reports keystrokes, this decides which
//! Started/Stopped messages go out.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Typing stops on its own after this long without a keystroke. Started is also repeated this
/// often while typing goes on, because recipients drop the indicator after ~15 s.
pub const TYPING_IDLE: Duration = Duration::from_secs(10);

struct State {
    last_started: Instant,
    last_keystroke: Instant,
}

#[derive(Default)]
pub struct Typing {
    conversations: HashMap<String, State>,
}

impl Typing {
    /// The user typed in `conversation_id`. True if a Started should go out now.
    pub fn started(&mut self, conversation_id: &str, now: Instant) -> bool {
        match self.conversations.get_mut(conversation_id) {
            Some(s) => {
                s.last_keystroke = now;
                if now.duration_since(s.last_started) >= TYPING_IDLE {
                    s.last_started = now;
                    return true;
                }
                false
            }
            None => {
                self.conversations.insert(
                    conversation_id.to_string(),
                    State {
                        last_started: now,
                        last_keystroke: now,
                    },
                );
                true
            }
        }
    }

    /// The user stopped typing (sent, cleared the field, left). True if a Stopped should go out.
    pub fn stopped(&mut self, conversation_id: &str) -> bool {
        self.conversations.remove(conversation_id).is_some()
    }

    /// Conversations idle for `TYPING_IDLE`: typing ends there and each needs a Stopped.
    pub fn expire(&mut self, now: Instant) -> Vec<String> {
        let idle: Vec<String> = self
            .conversations
            .iter()
            .filter(|(_, s)| now.duration_since(s.last_keystroke) >= TYPING_IDLE)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &idle {
            self.conversations.remove(id);
        }
        idle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn first_keystroke_starts_and_later_ones_are_quiet() {
        let t0 = Instant::now();
        let mut t = Typing::default();
        assert!(t.started("c:a", t0));
        assert!(!t.started("c:a", t0 + S));
        assert!(!t.started("c:a", t0 + 9 * S));
        // another conversation is independent
        assert!(t.started("c:b", t0 + S));
    }

    #[test]
    fn started_is_repeated_while_typing_goes_on() {
        let t0 = Instant::now();
        let mut t = Typing::default();
        assert!(t.started("c:a", t0));
        for s in 1..10 {
            assert!(!t.started("c:a", t0 + s * S), "{s}s");
        }
        assert!(t.started("c:a", t0 + 10 * S));
        assert!(!t.started("c:a", t0 + 11 * S));
    }

    #[test]
    fn stops_after_ten_idle_seconds_only() {
        let t0 = Instant::now();
        let mut t = Typing::default();
        t.started("c:a", t0);
        t.started("c:b", t0);
        t.started("c:a", t0 + 5 * S);
        assert!(t.expire(t0 + 9 * S).is_empty());
        assert_eq!(t.expire(t0 + 10 * S), vec!["c:b".to_string()]);
        assert!(t.expire(t0 + 14 * S).is_empty());
        assert_eq!(t.expire(t0 + 15 * S), vec!["c:a".to_string()]);
        // typing again after a stop starts afresh
        assert!(t.started("c:a", t0 + 16 * S));
    }

    #[test]
    fn explicit_stop_only_when_typing() {
        let t0 = Instant::now();
        let mut t = Typing::default();
        assert!(!t.stopped("c:a"));
        t.started("c:a", t0);
        assert!(t.stopped("c:a"));
        assert!(!t.stopped("c:a"));
        assert!(t.expire(t0 + 20 * S).is_empty());
        assert!(t.started("c:a", t0 + 21 * S));
    }
}
