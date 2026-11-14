//! The composer's styles and mentions (the Mac app's `ComposerText.swift`, `ComposerField.swift`
//! and `MentionPicker`): Desktop's shortcuts and the Format menu style the selection, or what's
//! typed next; in a group, "@" and the start of a name offer its members.
//!
//! The field is gpui-component's `Textarea`, which can't draw styled runs (only a code editor's
//! decorations, and no font changes at all): each byte's styles are kept here, beside its text,
//! and shown in a preview above it (`view.rs`). A mention is the field's own inline token
//! ("@Name", its id the ACI), so it moves and deletes as one.

use std::ops::Range;

use async_compat::Compat;
use gpui_kit::component::input::{InlineToken, InputContent, InputEditsEvent};
use gpui_kit::*;
use sig_core::projection::{BodyRange, TextStyle};

use super::Hush;
use crate::text::{self, BOLD, ITALIC, MENTION, MONOSPACE, SPOILER, STRIKETHROUGH};

pub(super) mod view;

gpui_kit::actions!(compose, [Bold, Italic, Strikethrough, Monospace, Spoiler]);

/// Desktop's shortcuts (`ComposerText.style(forKey:)`): ⌘B, ⌘I, ⇧⌘X, ⌘E, ⇧⌘B; Ctrl off macOS.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("secondary-b", Bold, Some("Input")),
        KeyBinding::new("secondary-i", Italic, Some("Input")),
        KeyBinding::new("secondary-shift-x", Strikethrough, Some("Input")),
        KeyBinding::new("secondary-e", Monospace, Some("Input")),
        KeyBinding::new("secondary-shift-b", Spoiler, Some("Input")),
    ]);
}

/// How many matches the picker shows (`MentionPicker.shown`).
const SHOWN: usize = 8;

/// Each style once, with Signal's name for it, in the order ranges go out.
fn all() -> [(u8, TextStyle); 5] {
    [
        (BOLD, TextStyle::Bold),
        (ITALIC, TextStyle::Italic),
        (STRIKETHROUGH, TextStyle::Strikethrough),
        (MONOSPACE, TextStyle::Monospace),
        (SPOILER, TextStyle::Spoiler),
    ]
}

/// What the composer holds: its text with the mention tokens, and each byte's styles.
#[derive(Clone)]
pub(super) struct Typed {
    content: InputContent,
    styles: Vec<u8>,
}

impl Default for Typed {
    fn default() -> Self {
        Self::plain(String::new())
    }
}

impl Typed {
    pub(super) fn plain(text: String) -> Self {
        Self {
            styles: vec![0; text.len()],
            content: InputContent::new(text),
        }
    }

    /// The words alone, as drafts keep them (a mention as its "@Name").
    pub(super) fn words(&self) -> String {
        self.content.text().to_string()
    }
}

/// The composer's styles and mention picker.
#[derive(Default)]
pub(super) struct Compose {
    /// The field's text as last seen, and each of its bytes' styles.
    text: String,
    styles: Vec<u8>,
    /// Styles toggled with nothing selected, for what's typed next at this offset.
    pending: Option<(usize, u8)>,
    /// The open group's members but us (ACIs); none in a direct chat.
    members: Vec<String>,
    /// The picker's choice, for the "@word" it was made in.
    chosen: (String, usize),
    /// Esc closed the picker for the "@word" starting here, until none is typed.
    dismissed: Option<usize>,
}

/// The mention picker as it is now.
pub(super) struct Picker {
    range: Range<usize>,
    query: String,
    /// (ACI, name)
    pub matches: Vec<(String, String)>,
    pub selected: usize,
}

/// The styles text typed over `range` takes (`NSTextView`'s typing attributes): those toggled
/// there, else those of the first character it replaces, else of the one before.
fn typing(styles: &[u8], range: &Range<usize>, pending: Option<(usize, u8)>) -> u8 {
    match pending {
        Some((at, s)) if range.is_empty() && at == range.start => s,
        _ if !range.is_empty() => styles[range.start],
        _ if range.start > 0 => styles[range.start - 1],
        _ => styles.first().copied().unwrap_or(0),
    }
}

/// `styles` (of `old`) once `old[range]` became `new`; what's typed takes the typing styles. What
/// an edit rewrites unchanged keeps its styles: an input method commits the whole text.
fn edited(styles: &mut Vec<u8>, old: &str, range: Range<usize>, new: &str, pending: Option<(usize, u8)>) {
    if styles.len() != old.len() || old.get(range.clone()).is_none() {
        // out of step (can't happen): plain, rather than styles on the wrong words
        *styles = vec![0; old.len()];
    }
    let Some(replaced) = old.get(range.clone()) else { return };
    let same =
        |a: &mut dyn Iterator<Item = (char, char)>| a.take_while(|(x, y)| x == y).map(|(x, _)| x.len_utf8()).sum();
    let head: usize = same(&mut replaced.chars().zip(new.chars()));
    let (replaced, new) = (&replaced[head..], &new[head..]);
    let tail: usize = same(&mut replaced.chars().rev().zip(new.chars().rev()));
    let range = range.start + head..range.end - tail;
    let style = typing(styles, &range, pending);
    styles.splice(range, std::iter::repeat_n(style, new.len() - tail));
}

/// Turns `bit` on over `range`, or off if all of it has it already (`ComposerText.toggle`).
fn toggle(styles: &mut [u8], range: Range<usize>, bit: u8) {
    let Some(span) = styles.get_mut(range) else { return };
    let all = !span.is_empty() && span.iter().all(|s| s & bit != 0);
    for s in span {
        *s = if all { *s & !bit } else { *s | bit };
    }
}

/// The text to send, trimmed, and its ranges in UTF-16 units (`ComposerText.message`): each
/// mention becomes Signal's placeholder (U+FFFC) under a mention range, in its styles; touching
/// runs of one style are one range.
pub(super) fn message(typed: &Typed) -> (String, Vec<BodyRange>) {
    let text = typed.content.text().as_ref();
    let style = |i: usize| typed.styles.get(i).copied().unwrap_or(0);
    // each character, its styles and the mentioned ACI
    let mut chars: Vec<(char, u8, Option<&str>)> = vec![];
    let mut i = 0;
    while let Some(c) = text[i..].chars().next() {
        match typed.content.tokens().iter().find(|t| t.range().start == i) {
            Some(t) => {
                chars.push(('\u{FFFC}', style(i), Some(t.token().id().as_ref())));
                i = t.range().end;
            }
            None => {
                chars.push((c, style(i), None));
                i += c.len_utf8();
            }
        }
    }
    let Some(start) = chars.iter().position(|c| !c.0.is_whitespace()) else {
        return (String::new(), vec![]);
    };
    let end = chars
        .iter()
        .rposition(|c| !c.0.is_whitespace())
        .map_or(start, |e| e + 1);
    let kept = &chars[start..end];
    let mut ranges: Vec<BodyRange> = vec![];
    for (bit, name) in all() {
        let mut at = 0;
        for &(c, styles, _) in kept {
            let units = c.len_utf16() as u32;
            if styles & bit != 0 {
                match ranges.last_mut() {
                    Some(r) if r.style == name && r.start + r.length == at => r.length += units,
                    _ => ranges.push(BodyRange {
                        start: at,
                        length: units,
                        style: name.clone(),
                    }),
                }
            }
            at += units;
        }
    }
    let mut at = 0;
    for &(c, _, aci) in kept {
        if let Some(aci) = aci {
            ranges.push(BodyRange {
                start: at,
                length: 1,
                style: TextStyle::Mention { aci: aci.into() },
            });
        }
        at += c.len_utf16() as u32;
    }
    (kept.iter().map(|c| c.0).collect(), ranges)
}

/// `body` with its styles and mentions (named by `name`), to edit (`ComposerText.attributed`). A
/// range that doesn't fit is left out; a mention covers one placeholder.
pub(super) fn load(body: &str, ranges: &[BodyRange], name: impl Fn(&str) -> String) -> Typed {
    let mut units: Vec<(u16, u8)> = body.encode_utf16().map(|u| (u, 0)).collect();
    let fit = |r: &&BodyRange| r.length > 0 && (r.start as usize).saturating_add(r.length as usize) <= units.len();
    let fitting: Vec<&BodyRange> = ranges.iter().filter(fit).collect();
    for r in &fitting {
        let bit = text::bit(&r.style);
        let span = r.start as usize..r.start as usize + r.length as usize;
        units[span].iter_mut().for_each(|u| u.1 |= bit);
    }
    let mention = |i: usize| {
        fitting.iter().find_map(|r| match &r.style {
            TextStyle::Mention { aci } if r.start as usize == i && r.length == 1 => Some(aci.as_str()),
            _ => None,
        })
    };
    let (mut text, mut styles, mut tokens) = (String::new(), vec![], vec![]);
    let mut i = 0;
    while i < units.len() {
        let style = units[i].1;
        let (piece, used) = match mention(i) {
            Some(aci) => {
                let shown = format!("@{}", name(aci));
                tokens.push((
                    text.len()..text.len() + shown.len(),
                    InlineToken::new(aci.to_owned(), shown.clone()),
                ));
                (shown, 1)
            }
            None => {
                let c = char::decode_utf16(units[i..].iter().take(2).map(|u| u.0)).next();
                let c = c.and_then(Result::ok).unwrap_or(char::REPLACEMENT_CHARACTER);
                (c.to_string(), c.len_utf16().min(units.len() - i))
            }
        };
        styles.extend(std::iter::repeat_n(style, piece.len()));
        text.push_str(&piece);
        i += used;
    }
    // shortcut: a mention the field can't hold as a token (a control character in the name, a
    // combining mark after it) stays plain "@Name" and the edit loses it; hardly ever sent
    let content = tokens
        .into_iter()
        .fold(InputContent::new(text), |content, (range, token)| {
            content.clone().with_token(range, token).unwrap_or(content)
        });
    Typed { content, styles }
}

/// The runs of like styles (`styles` of each byte), the mentions' bytes marked: what the preview
/// draws.
fn runs(styles: &[u8], tokens: &[Range<usize>]) -> text::Runs {
    let mut runs: text::Runs = vec![];
    for (i, &s) in styles.iter().enumerate() {
        let s = if tokens.iter().any(|t| t.contains(&i)) {
            s | MENTION
        } else {
            s
        };
        match runs.last_mut() {
            Some((r, last)) if *last == s && r.end == i => r.end = i + 1,
            _ if s != 0 => runs.push((i..i + 1, s)),
            _ => {}
        }
    }
    runs
}

/// The "@word" being typed just before `caret` (at the start or after a space) and the word
/// (`ComposerText.mentionQuery`); none right after a mention (`tokens`).
fn mention_query(text: &str, tokens: &[Range<usize>], caret: usize) -> Option<(Range<usize>, String)> {
    let before = text.get(..caret)?;
    for (i, c) in before.char_indices().rev() {
        if tokens.iter().any(|t| t.contains(&i)) || c.is_whitespace() {
            return None;
        }
        if c == '@' {
            let after_space = before[..i].chars().next_back().is_none_or(char::is_whitespace);
            return after_space.then(|| (i..caret, before[i + 1..].to_string()));
        }
    }
    None
}

/// The members whose name has a word starting with `query`, in any case, by name; the first
/// eight (`ComposerText.matches`).
fn matches(members: &[(String, String)], query: &str) -> Vec<(String, String)> {
    let q = query.to_lowercase();
    let mut found: Vec<(String, String)> = members
        .iter()
        .filter(|(_, name)| name.to_lowercase().split(' ').any(|w| w.starts_with(&q)))
        .cloned()
        .collect();
    found.sort_by_key(|(_, name)| name.to_lowercase());
    found.truncate(SHOWN);
    found
}

impl Hush {
    /// What the composer holds now.
    pub(super) fn typed(&self, cx: &App) -> Typed {
        Typed {
            content: self.composer.read(cx).content(),
            styles: self.compose.styles.clone(),
        }
    }

    /// Puts `typed` in the composer (a draft, an edit, what was typed before it), the caret at
    /// its end.
    pub(super) fn set_typed(&mut self, typed: Typed, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.update(cx, |input, cx| {
            input.set_value(typed.content, window, cx);
            let end = input.text().len();
            input.set_selected_range(end..end, cx);
            input.value().to_string()
        });
        // the field may have normalised the text (line breaks): then its styles don't fit it
        self.compose.styles = match typed.styles.len() == text.len() {
            true => typed.styles,
            false => vec![0; text.len()],
        };
        self.composer_empty = text.trim().is_empty();
        self.compose.text = text;
        self.compose.pending = None;
        cx.notify();
    }

    /// The composer's text changed: its styles follow, and the picker's Esc is forgotten once no
    /// "@word" is typed.
    pub(super) fn composer_edited(&mut self, e: &InputEditsEvent, cx: &mut Context<Self>) {
        for (edit, after) in e.edits() {
            let after = after.to_string();
            let new = after.get(edit.start_byte..edit.new_end_byte).unwrap_or_default();
            let c = &mut self.compose;
            edited(
                &mut c.styles,
                &c.text,
                edit.start_byte..edit.old_end_byte,
                new,
                c.pending,
            );
            c.pending = None;
            c.text = after;
        }
        if self.compose.styles.len() != self.compose.text.len() {
            self.compose.styles = vec![0; self.compose.text.len()];
        }
        if self.picker(cx).is_none() {
            self.compose.dismissed = None;
        }
    }

    /// ⌘B and the others: the selection takes the style (or loses it, if all of it has it), or
    /// what's typed next does.
    pub(super) fn toggle_style(&mut self, bit: u8, cx: &mut Context<Self>) {
        let range = self.composer.read(cx).selected_range();
        let c = &mut self.compose;
        if range.is_empty() {
            let current = typing(&c.styles, &range, c.pending);
            c.pending = Some((range.start, current ^ bit));
        } else {
            toggle(&mut c.styles, range, bit);
        }
        cx.notify();
    }

    /// The composer's text and its runs, while any of it is styled.
    pub(super) fn preview(&self, cx: &App) -> Option<(String, text::Runs)> {
        if self.compose.styles.iter().all(|&s| s == 0) {
            return None;
        }
        let tokens: Vec<Range<usize>> = self.composer.read(cx).tokens().iter().map(|t| t.range()).collect();
        Some((self.compose.text.clone(), runs(&self.compose.styles, &tokens)))
    }

    /// The open group's members, for mentions; none in a direct chat.
    pub(super) fn load_members(&mut self, cx: &mut Context<Self>) {
        self.compose.members.clear();
        self.compose.dismissed = None;
        let (Some(client), Some(id)) = (self.client.clone(), self.selected.clone()) else {
            return;
        };
        if !id.starts_with("g:") {
            return;
        }
        cx.spawn(async move |this, cx| {
            let found = Compat::new(client.group_members(id.clone())).await;
            let _ = this.update(cx, |app, cx| {
                if app.selected.as_ref() != Some(&id) {
                    return; // another chat was opened meanwhile: its own load runs
                }
                match found {
                    Ok(members) => {
                        let me = app.own_aci.clone();
                        app.compose.members = members.into_iter().filter(|a| Some(a) != me.as_ref()).collect();
                        app.load_names(app.compose.members.clone(), cx);
                    }
                    Err(e) => app.error = Some(format!("Couldn't load the group's members: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The members matching the "@word" before the caret, unless Esc closed it.
    pub(super) fn picker(&self, cx: &App) -> Option<Picker> {
        if self.compose.members.is_empty() {
            return None;
        }
        let input = self.composer.read(cx);
        let selection = input.selected_range();
        let tokens: Vec<Range<usize>> = input.tokens().iter().map(|t| t.range()).collect();
        let (range, query) = selection
            .is_empty()
            .then(|| mention_query(&input.value(), &tokens, selection.start))??;
        if self.compose.dismissed == Some(range.start) {
            return None;
        }
        let members: Vec<(String, String)> = self.compose.members.iter().map(|a| (a.clone(), self.name(a))).collect();
        let matches = matches(&members, &query);
        let selected = match &self.compose.chosen {
            (q, i) if *q == query => (*i).min(matches.len().checked_sub(1)?),
            _ if matches.is_empty() => return None,
            _ => 0,
        };
        Some(Picker {
            range,
            query,
            matches,
            selected,
        })
    }

    /// ↑/↓ in the open picker. False if it isn't open.
    pub(super) fn move_mention(&mut self, step: isize, cx: &mut Context<Self>) -> bool {
        let Some(p) = self.picker(cx) else { return false };
        let count = p.matches.len() as isize;
        self.compose.chosen = (p.query, (p.selected as isize + step).rem_euclid(count) as usize);
        cx.notify();
        true
    }

    /// Puts the chosen (or `index`th) member in place of the "@word", then a space. False if the
    /// picker isn't open.
    pub(super) fn pick_mention(&mut self, index: Option<usize>, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(p) = self.picker(cx) else { return false };
        let Some((aci, name)) = p.matches.get(index.unwrap_or(p.selected)) else {
            return false;
        };
        let token = InlineToken::new(aci.clone(), format!("@{name}"));
        let added = self.composer.update(cx, |input, cx| {
            input.replace_range_with_token(p.range, token, window, cx)?;
            input.insert(" ", window, cx);
            input.focus(window, cx);
            Ok::<_, gpui_kit::component::input::InlineTokenError>(())
        });
        if let Err(e) = added {
            self.error = Some(format!("Couldn't add the mention: {e}"));
        }
        cx.notify();
        true
    }

    /// Esc in the open picker: closed until another "@word" is typed. False if it isn't open.
    pub(super) fn dismiss_mentions(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(p) = self.picker(cx) else { return false };
        self.compose.dismissed = Some(p.range.start);
        cx.notify();
        true
    }
}

#[cfg(feature = "snapshot")]
impl Hush {
    /// `--snapshot`'s composer: a styled message with a mention, and the picker open.
    pub(super) fn demo_compose(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.compose.members = crate::demo::members();
        let typed = load(
            "Thanks \u{FFFC}! Who brings the big pot? @bo",
            &crate::demo::compose_ranges(),
            |aci| self.name(aci),
        );
        self.set_typed(typed, window, cx);
    }
}

#[cfg(test)]
mod tests {
    // not `super::*`: GPUI's glob brings its own `test` macro
    use std::ops::Range;

    use gpui_kit::component::input::{InlineToken, InputContent};
    use sig_core::projection::{BodyRange, TextStyle};

    use super::{edited, load, matches, mention_query, message, runs, toggle, Typed, SHOWN};
    use crate::text::{BOLD, ITALIC, MENTION};

    /// Text typed at `at` into `text` with `styles`.
    fn typed_into(text: &str, styles: &mut Vec<u8>, at: Range<usize>, new: &str, pending: Option<(usize, u8)>) {
        edited(styles, text, at, new, pending);
    }

    #[test]
    fn what_is_typed_takes_the_styles_before_it_or_those_toggled() {
        let mut styles = vec![BOLD, BOLD, 0, 0];
        typed_into("ab  ", &mut styles, 2..2, "c", None);
        assert_eq!(styles, [BOLD, BOLD, BOLD, 0, 0], "after a bold one, bold");
        typed_into("abc  ", &mut styles, 0..0, "x", None);
        assert_eq!(styles[0], BOLD, "at the start, the first one's");
        let mut styles = vec![BOLD, 0];
        typed_into("ab", &mut styles, 1..1, "é", Some((1, BOLD | ITALIC)));
        assert_eq!(
            styles,
            [BOLD, BOLD | ITALIC, BOLD | ITALIC, 0],
            "what was toggled there"
        );
        let mut styles = vec![BOLD, 0];
        typed_into("ab", &mut styles, 2..2, "c", Some((1, ITALIC)));
        assert_eq!(styles, [BOLD, 0, 0], "toggled elsewhere: not here");
        let mut styles = vec![];
        typed_into("", &mut styles, 0..0, "hi", None);
        assert_eq!(styles, [0, 0]);
    }

    #[test]
    fn replacing_a_selection_takes_its_first_characters_styles_and_deleting_drops_them() {
        let mut styles = vec![0, ITALIC, BOLD, 0];
        typed_into("abcd", &mut styles, 1..3, "xyz", None);
        assert_eq!(styles, [0, ITALIC, ITALIC, ITALIC, 0]);
        typed_into("axyzd", &mut styles, 1..4, "", None);
        assert_eq!(styles, [0, 0]);
    }

    #[test]
    fn an_input_methods_whole_text_commit_keeps_the_styles_of_what_stayed() {
        let mut styles = vec![BOLD, BOLD, 0, ITALIC, ITALIC];
        // "ab" + "か" composed + "cd", committed as one edit of the whole text
        typed_into("ab cd", &mut styles, 0..5, "ab か cd", None);
        assert_eq!(styles, [BOLD, BOLD, 0, 0, 0, 0, 0, ITALIC, ITALIC]);
    }

    #[test]
    fn a_style_toggles_over_the_selection_on_unless_all_of_it_has_it() {
        let mut styles = vec![BOLD, 0, 0];
        toggle(&mut styles, 0..2, BOLD);
        assert_eq!(styles, [BOLD, BOLD, 0]);
        toggle(&mut styles, 0..2, BOLD);
        assert_eq!(styles, [0, 0, 0]);
        toggle(&mut styles, 1..9, BOLD);
        assert_eq!(styles, [0, 0, 0], "past the end: nothing");
    }

    fn range(start: u32, length: u32, style: TextStyle) -> BodyRange {
        BodyRange { start, length, style }
    }

    fn mention(start: u32, aci: &str) -> BodyRange {
        range(start, 1, TextStyle::Mention { aci: aci.into() })
    }

    fn name(aci: &str) -> String {
        format!("Name {aci}")
    }

    #[test]
    fn a_message_is_trimmed_with_its_ranges_in_utf16_and_mentions_as_placeholders() {
        // " 👋 bold @Name a hi " with "bold" and the mention bold, "hi" italic
        let text = " 👋 bold @Name a hi ";
        let token = text.find('@').unwrap()..text.find(" hi").unwrap();
        let mut styles = vec![0; text.len()];
        let bold = text.find("bold").unwrap();
        styles[bold..token.end].iter_mut().for_each(|s| *s = BOLD);
        let hi = text.find("hi").unwrap();
        styles[hi..hi + 2].iter_mut().for_each(|s| *s = ITALIC);
        let content = InputContent::new(text)
            .with_token(token.clone(), InlineToken::new("a", &text[token]))
            .unwrap();
        let (body, ranges) = message(&Typed { content, styles });
        assert_eq!(body, "👋 bold \u{FFFC} hi");
        assert_eq!(
            ranges,
            [
                range(3, 6, TextStyle::Bold),
                range(10, 2, TextStyle::Italic),
                mention(8, "a")
            ],
            "touching runs of bold (the word, the space, the mention) are one range"
        );
        assert_eq!(message(&Typed::plain("  \n ".into())), (String::new(), vec![]));
    }

    #[test]
    fn an_edit_loads_its_styles_and_mentions_and_sends_them_back_the_same() {
        let body = "👋 \u{FFFC}, look: big pot";
        let ranges = vec![
            range(0, 6, TextStyle::Bold),
            range(5, 6, TextStyle::Italic),
            range(12, 3, TextStyle::Strikethrough),
            range(16, 3, TextStyle::Monospace),
            range(3, 1, TextStyle::Spoiler),
            mention(3, "a"),
        ];
        let typed = load(body, &ranges, name);
        assert_eq!(typed.words(), "👋 @Name a, look: big pot");
        assert_eq!(typed.content.tokens().len(), 1);
        assert_eq!(typed.content.tokens()[0].token().id().as_ref(), "a");
        let (sent, mut back) = message(&typed);
        assert_eq!(sent, body);
        let mut ranges = ranges;
        back.sort_by_key(|r| (r.start, r.length));
        ranges.sort_by_key(|r| (r.start, r.length));
        assert_eq!(back, ranges);
    }

    #[test]
    fn an_edit_leaves_out_ranges_that_dont_fit() {
        let typed = load(
            "hi \u{FFFC}",
            &[
                range(1, 9, TextStyle::Bold),
                range(3, 2, TextStyle::Mention { aci: "a".into() }),
            ],
            name,
        );
        assert_eq!(typed.words(), "hi \u{FFFC}");
        assert!(typed.content.tokens().is_empty());
        assert!(typed.styles.iter().all(|&s| s == 0));
    }

    #[test]
    fn the_preview_draws_runs_of_like_styles_and_marks_mentions() {
        let styles = [BOLD, BOLD, 0, 0, 0, ITALIC];
        assert_eq!(
            runs(&styles, std::slice::from_ref(&(2..4))),
            [(0..2, BOLD), (2..4, MENTION), (5..6, ITALIC)]
        );
    }

    #[test]
    fn an_at_word_at_the_start_or_after_a_space_is_a_query() {
        let q = |text: &str, tokens: &[Range<usize>]| mention_query(text, tokens, text.len());
        let ana = std::slice::from_ref(&(0..4));
        assert_eq!(q("@an", &[]), Some((0..3, "an".into())));
        assert_eq!(q("hi @", &[]), Some((3..4, String::new())));
        assert_eq!(q("hi\n@Ana", &[]), Some((3..7, "Ana".into())));
        assert_eq!(q("mail@an", &[]), None, "inside a word");
        assert_eq!(q("@an x", &[]), None, "a space ends it");
        assert_eq!(q("@Ana", ana), None, "a finished mention isn't one");
        assert_eq!(q("@Ana@b", ana), None);
        assert_eq!(mention_query("@an", &[], 9), None, "past the end");
    }

    #[test]
    fn the_picker_offers_members_with_a_word_starting_so_by_name() {
        let members: Vec<(String, String)> = [("b", "bogdan Ionescu"), ("a", "Ana Popescu"), ("i", "Ioana")]
            .map(|(a, n)| (a.to_owned(), n.to_owned()))
            .into();
        let found = |q: &str| -> Vec<String> { matches(&members, q).into_iter().map(|m| m.0).collect() };
        assert_eq!(found(""), ["a", "b", "i"]);
        assert_eq!(found("io"), ["b", "i"]);
        assert_eq!(found("POP"), ["a"]);
        assert!(found("na").is_empty(), "a word's start, not its middle");
        let many: Vec<(String, String)> = (0..12).map(|i| (i.to_string(), format!("Ana {i:02}"))).collect();
        assert_eq!(matches(&many, "a").len(), SHOWN);
    }
}
