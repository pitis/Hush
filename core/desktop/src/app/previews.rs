//! Link previews and links in message text (the Mac app's `LinkPreviewCard`, `PreviewDraftCard`,
//! `AppStore.composerChanged`, `PreviewLink` and `MessageText.groupLinks`): a received message's
//! card under its files, opening its https link; while writing, after a pause, a card for the first
//! https link above the composer that goes with the message (✕ leaves it off); a group's invite
//! link in a message's text opens the group's preview. Whether a page may be fetched at all is the
//! phone's "Generate link previews", which the core checks (`Client::link_preview_for`).

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use async_compat::Compat;
use gpui_kit::*;
use sig_core::projection::{LinkPreview, Message, MessageDetails, MessageKey, MessageKind, PreviewDraft};

use super::{files, DialogKind, Hush};
use crate::text::LINK;

mod view;

pub(super) use view::card;

/// How long the composer waits after typing before asking for a preview (`ComposerView`).
const WAIT: Duration = Duration::from_millis(700);

type Key = (String, i64);

/// The open chat's link previews, and the composer's.
#[derive(Default)]
pub(super) struct Previews {
    /// The open chat's messages' previews, by message (author, sent at).
    by_message: HashMap<Key, LinkPreview>,
    /// Their pictures and the draft's, by file, decoded small; None while asked for, or one that
    /// didn't read (asked for again when the chat opens again).
    pictures: HashMap<String, Option<Arc<RenderImage>>>,
    /// The link preview for the composer's text, to go with the next message.
    draft: Option<PreviewDraft>,
    /// The link `draft` is (or will be) for.
    link: Option<String>,
    fetching: Option<String>,
    /// Links already asked about (or dismissed) in this chat: not fetched again.
    asked: HashSet<String>,
    /// The pause in typing being waited for: a new keystroke drops it.
    wait: Option<Task<()>>,
}

/// A message's preview as its bubble shows it.
pub(super) struct Card {
    pub preview: LinkPreview,
    /// Its picture, once decoded; none if it has none (or it isn't downloaded yet).
    pub picture: Option<Option<Arc<RenderImage>>>,
}

impl Previews {
    /// The composer's text after a pause: a draft whose link is gone goes; the link to fetch a
    /// preview for, if it wasn't asked about yet in this chat.
    fn typed(&mut self, text: &str) -> Option<String> {
        let link = first_link(text).map(str::to_owned);
        if link != self.link {
            self.draft = None;
            self.link = link.clone();
        }
        let link = link?;
        if self.draft.is_some() || self.fetching.as_ref() == Some(&link) {
            return None;
        }
        // asked once per link in a chat: a page without a preview isn't fetched at every pause
        if !self.asked.insert(link.clone()) {
            return None;
        }
        self.fetching = Some(link.clone());
        Some(link)
    }

    /// The fetch for `link` ended: its draft, if the composer still has that link.
    fn fetched(&mut self, link: &str, draft: Option<PreviewDraft>) {
        if self.fetching.as_deref() == Some(link) {
            self.fetching = None;
        }
        if self.link.as_deref() == Some(link) {
            self.draft = draft;
        }
    }

    /// ✕ on the draft: none for that link in this chat.
    fn dismiss(&mut self) {
        if let Some(link) = &self.link {
            self.asked.insert(link.clone());
        }
        self.draft = None;
    }

    /// The draft to go with `body`, taken out of the composer: only one whose link is still in it.
    pub(super) fn take(&mut self, body: &str) -> Option<PreviewDraft> {
        self.link = None;
        self.draft.take().filter(|d| body.contains(&d.url))
    }

    /// The send failed and its text went back into the composer: so does its preview.
    pub(super) fn put_back(&mut self, draft: Option<PreviewDraft>) {
        if let Some(d) = draft {
            self.link = Some(d.url.clone());
            self.draft = Some(d);
        }
    }

    /// The page's previews from the core's details of its messages (`keys`); a message the core
    /// couldn't read isn't in them, and keeps what it showed. True if any changed.
    fn loaded(&mut self, keys: &[MessageKey], details: &[MessageDetails]) -> bool {
        let read: HashMap<Key, &Option<LinkPreview>> = details
            .iter()
            .map(|d| ((d.key.author.clone(), d.key.sent_at), &d.preview))
            .collect();
        let previews = keys.iter().filter_map(|k| {
            let k = (k.author.clone(), k.sent_at);
            let preview = match read.get(&k) {
                Some(read) => read.as_ref()?,
                None => self.by_message.get(&k)?,
            };
            Some((k, preview.clone()))
        });
        let previews: HashMap<Key, LinkPreview> = previews.collect();
        let changed = previews != self.by_message;
        self.by_message = previews;
        changed
    }

    /// The pictures to decode: the page's previews' and the draft's, not asked for yet.
    fn wanted(&mut self) -> Vec<String> {
        let received = self.by_message.values().filter_map(|p| ready(p)).map(str::to_owned);
        let draft = self.draft.iter().filter_map(|d| Some(d.image.as_ref()?.path.clone()));
        let paths: Vec<String> = received.chain(draft).collect();
        let wanted = paths.into_iter().filter(|path| {
            let new = !self.pictures.contains_key(path);
            if new {
                self.pictures.insert(path.clone(), None);
            }
            new
        });
        wanted.collect()
    }
}

/// A received preview's picture, once downloaded.
fn ready(p: &LinkPreview) -> Option<&str> {
    let image = p.image.as_ref()?;
    image
        .path
        .as_deref()
        .filter(|_| image.state == sig_core::projection::AttachmentState::Ready)
}

/// The link a preview is for: the first https link, as typed, without the punctuation that ends a
/// sentence after it (as the core finds it: `link_preview::first_link`, `PreviewLink.first`).
fn first_link(text: &str) -> Option<&str> {
    text.split_whitespace().find_map(|word| {
        let link = word[word.find("https://")?..].trim_end_matches(CLOSING);
        // shortcut: a host is anything up to the path; the core parses it, and finds none for
        // one that isn't a host (the draft then stays empty)
        let host = link["https://".len()..].split(['/', '?', '#']).next()?;
        (!host.is_empty()).then_some(link)
    })
}

/// The punctuation that can end a sentence or close brackets around a link (the core's).
const CLOSING: [char; 11] = ['.', ',', ';', ':', '!', '?', ')', ']', '>', '"', '\''];

/// Where `text` has a group's invite link (`MessageText.groupLinks`), as byte ranges, in order.
pub(in crate::app) fn group_links(text: &str) -> Vec<Range<usize>> {
    let mut found: Vec<Range<usize>> = ["https://signal.group/#", "sgnl://signal.group/#"]
        .iter()
        .flat_map(|prefix| {
            text.match_indices(prefix).filter_map(|(start, _)| {
                let rest = &text[start + prefix.len()..];
                let key = rest
                    .find(|c: char| !(c.is_ascii_alphanumeric() || "_=%-".contains(c)))
                    .unwrap_or(rest.len());
                (key > 0).then(|| start..start + prefix.len() + key)
            })
        })
        .collect();
    found.sort_by_key(|r| r.start);
    found
}

/// `runs` (of a text `len` bytes long) with the bytes of `links` marked `LINK`.
pub(in crate::app) fn linked(len: usize, runs: &[(Range<usize>, u8)], links: &[Range<usize>]) -> crate::text::Runs {
    let mut styles = vec![0; len];
    for (range, s) in runs {
        styles
            .get_mut(range.clone())
            .into_iter()
            .flatten()
            .for_each(|b| *b = *s);
    }
    for range in links {
        styles
            .get_mut(range.clone())
            .into_iter()
            .flatten()
            .for_each(|b| *b |= LINK);
    }
    super::compose::runs(&styles, &[])
}

/// What the card says the link is: its host (`URL.host()`), else the link.
fn host(url: &str) -> &str {
    let rest = url.split_once("://").map_or("", |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => host.split(':').next().unwrap_or_default(),
    };
    if host.is_empty() {
        url
    } else {
        host
    }
}

/// `url` as handed to the system to open: https only (`LinkPreviewCard`), and nothing in it a
/// command line could read as more than one link (anything but plain ASCII is percent-encoded).
fn openable(url: &str) -> Option<String> {
    if !url.get(..8).is_some_and(|s| s.eq_ignore_ascii_case("https://")) {
        return None;
    }
    let mut out = String::with_capacity(url.len());
    for c in url.chars() {
        if c.is_ascii_graphic() && !"\"<>\\^`{|}".contains(c) {
            out.push(c);
        } else {
            let mut bytes = [0; 4];
            for b in c.encode_utf8(&mut bytes).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    Some(out)
}

impl Hush {
    /// The open chat's messages' details were read: their previews show, and their pictures load.
    pub(super) fn previews_loaded(&mut self, keys: &[MessageKey], details: &[MessageDetails], cx: &mut Context<Self>) {
        if self.previews.loaded(keys, details) {
            // a card came or went: its row's height did too
            self.timeline.update(cx, |t, cx| t.remeasure(cx));
        }
        self.load_preview_pictures(cx);
    }

    /// A message's preview card; none for a deleted message, a view-once one or a sticker
    /// (`MessageRow.bubble`).
    pub(super) fn preview_card(&self, m: &Message) -> Option<Card> {
        if m.deleted || matches!(m.kind, MessageKind::ViewOnce { .. } | MessageKind::Sticker { .. }) {
            return None;
        }
        let preview = self.previews.by_message.get(&(m.author.clone(), m.sent_at))?.clone();
        let picture = ready(&preview).map(|path| self.previews.pictures.get(path).cloned().flatten());
        Some(Card { preview, picture })
    }

    /// The composer's text changed: a preview is asked for once typing pauses.
    pub(super) fn composer_changed(&mut self, cx: &mut Context<Self>) {
        // the wait before is dropped with its task
        self.previews.wait = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(WAIT).await;
            let _ = this.update(cx, |app, cx| app.composer_paused(cx));
        }));
    }

    /// Typing paused: fetches a preview for the first https link, and drops one whose link is
    /// gone. A preview is extra: a failed fetch shows nothing.
    fn composer_paused(&mut self, cx: &mut Context<Self>) {
        // an edit goes without one (`composerChanged`)
        if self.actions.editing() {
            return;
        }
        let text = self.composer.read(cx).value().to_string();
        let fetch = self.previews.typed(&text);
        cx.notify();
        let (Some(link), Some(client)) = (fetch, self.client.clone()) else {
            return;
        };
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let found = Compat::new(client.link_preview_for(text)).await;
            let _ = this.update(cx, |app, cx| {
                if app.generation != generation {
                    return; // another chat was opened meanwhile
                }
                // the core's reason names no link
                let draft = found.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "link preview failed");
                    None
                });
                app.previews.fetched(&link, draft);
                app.load_preview_pictures(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// ✕ on the composer's preview.
    pub(super) fn dismiss_preview(&mut self, cx: &mut Context<Self>) {
        self.previews.dismiss();
        cx.notify();
    }

    /// Leaving a chat: its previews, their pictures and the composer's go.
    pub(super) fn clear_previews(&mut self, cx: &mut Context<Self>) {
        let shown: Vec<_> = std::mem::take(&mut self.previews)
            .pictures
            .into_values()
            .flatten()
            .collect();
        // once no window is being drawn, so each lets its copy go
        cx.defer(move |cx| {
            for picture in shown {
                cx.drop_image(picture, None);
            }
        });
    }

    /// Decodes the pictures not asked for yet, one at a time, while their chat stays open. They
    /// are encrypted on disk: the core decrypts them, in memory.
    fn load_preview_pictures(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        let wanted = self.previews.wanted();
        if wanted.is_empty() {
            return;
        }
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            for path in wanted {
                // shortcut: decoded as the chat's photos are (640 pixels at most), more than a
                // 52pt card needs; decode to the card's size if many previews show at once
                let picture = match Compat::new(client.file_bytes(path.clone())).await {
                    Ok(bytes) => cx.background_spawn(async move { files::thumbnail(&bytes) }).await,
                    Err(e) => Err(e.to_string()),
                };
                let shown = this.update(cx, |app, cx| {
                    if app.generation != generation {
                        return false;
                    }
                    match picture {
                        Ok(picture) => {
                            app.previews.pictures.insert(path, Some(Arc::new(picture)));
                            cx.notify();
                        }
                        // the card shows without it
                        Err(e) => tracing::warn!(error = %e, "a link preview's picture isn't shown"),
                    }
                    true
                });
                if !shown.unwrap_or(false) {
                    return;
                }
            }
        })
        .detach();
    }

    /// A click on a preview card: its https link opens in the browser.
    pub(super) fn open_link(&mut self, url: &str, cx: &mut Context<Self>) {
        if let Some(url) = openable(url) {
            files::open_link(url, cx);
        }
    }

    /// A click on a group's invite link in a message: the group it shows, to join or open
    /// (`AppStore.handleLink`, `JoinGroupSheet`).
    pub(super) fn open_group_link(&mut self, link: String, window: &mut Window, cx: &mut Context<Self>) {
        // one still asking the server stays
        if self.dialog.as_ref().is_some_and(|d| d.busy) {
            return;
        }
        self.open_dialog(DialogKind::Join(None), window, cx);
        if let Some(d) = &self.dialog {
            d.field.update(cx, |field, cx| field.set_value(link, window, cx));
        }
        self.submit_dialog(window, cx);
    }
}

#[cfg(feature = "snapshot")]
impl Hush {
    /// `--snapshot`'s link previews: a received one and a draft for the composer's link, with
    /// their pictures.
    pub(super) fn demo_previews(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut messages = self.messages.clone();
        let (received, preview) = crate::demo::link_messages();
        let picture = files::thumbnail(&crate::demo::picture()).expect("the made-up picture decodes");
        let picture = Some(Arc::new(picture));
        let with = &received[0];
        if let Some(path) = ready(&preview) {
            self.previews.pictures.insert(path.into(), picture.clone());
        }
        self.previews
            .by_message
            .insert((with.author.clone(), with.sent_at), preview);
        messages.extend(received);
        self.set_messages(messages, cx);
        let draft = crate::demo::preview_draft();
        if let Some(image) = &draft.image {
            self.previews.pictures.insert(image.path.clone(), picture);
        }
        let text = format!("Booked for 8! Directions: {}", draft.url);
        self.previews.link = Some(draft.url.clone());
        self.previews.draft = Some(draft);
        self.set_typed(super::compose::Typed::plain(text), window, cx);
    }
}

#[cfg(test)]
mod tests {
    // not `super::*`: GPUI's glob brings its own `test` macro
    use sig_core::projection::{
        Attachment, AttachmentState, LinkPreview, MessageDetails, MessageKey, OutgoingFile, PreviewDraft,
    };

    use super::{first_link, group_links, host, linked, openable, Previews};
    use crate::text::{BOLD, LINK};

    fn draft(url: &str) -> PreviewDraft {
        PreviewDraft {
            url: url.into(),
            title: Some("A page".into()),
            description: None,
            image: Some(OutgoingFile {
                path: "previews/1".into(),
                content_type: "image/png".into(),
                width: None,
                height: None,
                is_voice_note: false,
            }),
        }
    }

    #[test]
    fn the_first_https_link_is_taken_as_typed_without_the_sentence_around_it() {
        assert_eq!(first_link("see https://example.com/a."), Some("https://example.com/a"));
        assert_eq!(
            first_link("(https://example.com/a?b=1)!"),
            Some("https://example.com/a?b=1")
        );
        assert_eq!(
            first_link("http://plain.example then https://Example.com/É"),
            Some("https://Example.com/É"),
            "http isn't previewed; as typed, not normalised"
        );
        assert_eq!(first_link("x:https://example.com"), Some("https://example.com"));
        assert_eq!(first_link("https:// nothing https:///path"), None, "no host");
        assert_eq!(first_link("no link here"), None);
    }

    #[test]
    fn a_preview_is_asked_for_once_per_link_and_one_whose_link_went_is_dropped() {
        let mut p = Previews::default();
        assert_eq!(p.typed("look https://a.example"), Some("https://a.example".into()));
        assert_eq!(p.typed("look https://a.example "), None, "already being fetched");
        p.fetched("https://a.example", Some(draft("https://a.example")));
        assert!(p.draft.is_some());
        assert_eq!(
            p.typed("look https://a.example, nice"),
            None,
            "the same link keeps its draft"
        );
        assert!(p.draft.is_some());

        assert_eq!(p.typed("look https://b.example"), Some("https://b.example".into()));
        assert!(p.draft.is_none(), "another link: the first one's draft goes");
        p.fetched("https://b.example", None);
        assert_eq!(
            p.typed("look https://a.example"),
            None,
            "asked about already in this chat"
        );
        assert_eq!(p.typed("no link"), None);
        assert_eq!(p.link, None);
    }

    #[test]
    fn a_fetch_that_ends_after_its_link_went_shows_nothing() {
        let mut p = Previews::default();
        p.typed("https://a.example");
        p.typed("https://b.example");
        p.fetched("https://a.example", Some(draft("https://a.example")));
        assert!(p.draft.is_none());
        assert_eq!(
            p.fetching.as_deref(),
            Some("https://b.example"),
            "the newer fetch still runs"
        );
    }

    #[test]
    fn a_dismissed_preview_isnt_fetched_again_in_the_chat() {
        let mut p = Previews::default();
        assert!(p.typed("https://a.example").is_some());
        p.fetched("https://a.example", Some(draft("https://a.example")));
        p.dismiss();
        assert!(p.draft.is_none());
        p.typed("");
        assert_eq!(p.typed("https://a.example"), None);
        assert!(p.draft.is_none());
    }

    #[test]
    fn a_preview_goes_only_with_its_link_in_the_text_and_comes_back_with_a_failed_send() {
        let mut p = Previews::default();
        p.typed("https://a.example");
        p.fetched("https://a.example", Some(draft("https://a.example")));
        let sent = p.take("see https://a.example");
        assert_eq!(sent.as_ref().map(|d| d.url.as_str()), Some("https://a.example"));
        assert!(p.draft.is_none() && p.link.is_none(), "taken: not sent twice");

        p.put_back(sent);
        assert!(p.draft.is_some());
        assert_eq!(p.typed("see https://a.example"), None, "back with its text, kept");
        assert!(p.draft.is_some());

        assert_eq!(p.take("the link went"), None, "its link isn't in the text");
        assert!(p.draft.is_none());
    }

    fn key(author: &str, sent_at: i64) -> MessageKey {
        MessageKey {
            author: author.into(),
            sent_at,
        }
    }

    fn details(k: &MessageKey, preview: Option<&str>) -> MessageDetails {
        MessageDetails {
            key: k.clone(),
            reactions: vec![],
            attachments: vec![],
            preview: preview.map(|url| LinkPreview {
                url: url.into(),
                title: Some("A page".into()),
                description: None,
                image: None,
            }),
            poll: None,
        }
    }

    #[test]
    fn previews_come_from_the_details_and_a_message_the_core_couldnt_read_keeps_its_own() {
        let (a, b, c) = (key("x", 1), key("x", 2), key("y", 3));
        let mut p = Previews::default();
        let keys = [a.clone(), b.clone(), c.clone()];
        assert!(p.loaded(
            &keys,
            &[
                details(&a, Some("https://a.example")),
                details(&b, Some("https://b.example"))
            ]
        ));
        assert_eq!(p.by_message.len(), 2);
        assert!(!p.loaded(
            &keys,
            &[
                details(&a, Some("https://a.example")),
                details(&b, Some("https://b.example"))
            ]
        ));
        // a unread now: kept; b lost its preview (edited); c not on the page any more
        assert!(p.loaded(&[a.clone(), b.clone()], &[details(&b, None)]));
        let urls: Vec<&str> = p.by_message.values().map(|p| p.url.as_str()).collect();
        assert_eq!(urls, ["https://a.example"]);
    }

    #[test]
    fn only_downloaded_pictures_and_the_drafts_are_decoded_once() {
        let mut p = Previews::default();
        let image = |path: Option<&str>, state| Attachment {
            id: 1,
            message: key("x", 1),
            content_type: "image/jpeg".into(),
            file_name: None,
            size: 1,
            width: None,
            height: None,
            is_voice_note: false,
            path: path.map(Into::into),
            state,
        };
        let mut a = details(&key("x", 1), Some("https://a.example"));
        a.preview.as_mut().unwrap().image = Some(image(Some("files/1"), AttachmentState::Ready));
        let mut b = details(&key("x", 2), Some("https://b.example"));
        b.preview.as_mut().unwrap().image = Some(image(None, AttachmentState::Downloading));
        p.loaded(&[a.key.clone(), b.key.clone()], &[a, b]);
        p.draft = Some(draft("https://c.example"));
        let mut wanted = p.wanted();
        wanted.sort();
        assert_eq!(wanted, ["files/1", "previews/1"]);
        assert!(p.wanted().is_empty(), "asked for already");
    }

    #[test]
    fn group_invite_links_are_found_as_the_mac_app_finds_them() {
        let text = "join https://signal.group/#CjQKIA_x-1%3D= or sgnl://signal.group/#abc, not https://signal.group/# nor https://example.com/#abc";
        let found: Vec<&str> = group_links(text).into_iter().map(|r| &text[r]).collect();
        assert_eq!(
            found,
            ["https://signal.group/#CjQKIA_x-1%3D=", "sgnl://signal.group/#abc"]
        );
        assert!(
            group_links("HTTPS://SIGNAL.GROUP/#abc").is_empty(),
            "as the Mac's, case counts"
        );
    }

    #[test]
    fn a_links_bytes_are_marked_over_the_styles_around_them() {
        assert_eq!(
            linked(8, &[(0..4, BOLD)], std::slice::from_ref(&(2..6))),
            [(0..2, BOLD), (2..4, BOLD | LINK), (4..6, LINK)]
        );
        assert_eq!(
            linked(3, &[], std::slice::from_ref(&(1..9))),
            [],
            "past the end: left out"
        );
    }

    #[test]
    fn the_card_names_the_links_host() {
        assert_eq!(host("https://www.example.com/a/b?c#d"), "www.example.com");
        assert_eq!(host("https://example.com:8443"), "example.com");
        assert_eq!(host("https://[2001:db8::1]:443/x"), "2001:db8::1");
        assert_eq!(host("HTTPS://Example.com"), "Example.com");
        assert_eq!(host("not a link"), "not a link");
        assert_eq!(host("https://"), "https://");
    }

    #[test]
    fn only_https_links_open_and_nothing_in_them_reads_as_more_than_one() {
        assert_eq!(
            openable("https://example.com/a?b=1#c").as_deref(),
            Some("https://example.com/a?b=1#c")
        );
        assert_eq!(openable("HTTPS://example.com").as_deref(), Some("HTTPS://example.com"));
        assert_eq!(
            openable("https://ro.example/România\"&x|y").as_deref(),
            Some("https://ro.example/Rom%C3%A2nia%22&x%7Cy")
        );
        assert_eq!(openable("http://example.com"), None);
        assert_eq!(openable("file:///etc/passwd"), None);
        assert_eq!(openable("javascript:alert(1)"), None);
    }
}

/// Links in a real (headless) window: `cargo test -p hush-desktop --features snapshot`.
#[cfg(all(test, feature = "snapshot"))]
mod window_tests {
    use std::time::Duration;

    use gpui_kit::{gpui, AppContext as _, Entity, TestAppContext, WindowOptions};

    use super::super::{DialogKind, Hush};
    use super::WAIT;
    use crate::Demo;

    fn open(screen: Demo, cx: &mut TestAppContext) -> (gpui_kit::AnyWindowHandle, Entity<Hush>) {
        let (window, hush) = cx.update(|cx| {
            gpui_kit::init(cx);
            cx.bind_keys(crate::key_bindings());
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Hush::demo(screen, window, cx))
            })
            .expect("the window opens")
        });
        window
            .update(cx, |_, window, cx| {
                hush.update(cx, |app, cx| app.composer.update(cx, |c, cx| c.focus(window, cx)))
            })
            .unwrap();
        cx.run_until_parked();
        (window, hush)
    }

    #[gpui::test]
    fn a_link_is_looked_at_once_typing_pauses(cx: &mut TestAppContext) {
        let (window, hush) = open(Demo::Chat, cx);
        cx.simulate_input(window, "see https://example.com/a");
        let link =
            |hush: &Entity<Hush>, cx: &mut TestAppContext| hush.read_with(cx, |app, _| app.previews.link.clone());
        cx.executor().advance_clock(WAIT - Duration::from_millis(1));
        cx.run_until_parked();
        assert_eq!(link(&hush, cx), None, "still typing");
        cx.executor().advance_clock(Duration::from_millis(1));
        cx.run_until_parked();
        assert_eq!(link(&hush, cx).as_deref(), Some("https://example.com/a"));
    }

    #[gpui::test]
    fn a_group_link_shows_its_group_in_the_join_dialog(cx: &mut TestAppContext) {
        let (window, hush) = open(Demo::Chat, cx);
        let link = "https://signal.group/#CjQKIA";
        window
            .update(cx, |_, window, cx| {
                hush.update(cx, |app, cx| app.open_group_link(link.into(), window, cx))
            })
            .unwrap();
        let asked = hush.read_with(cx, |app, cx| {
            let d = app.dialog.as_ref().expect("the dialog is open");
            assert!(matches!(d.kind, DialogKind::Join(None)));
            d.field.read(cx).value().to_string()
        });
        assert_eq!(asked, link);
    }
}
