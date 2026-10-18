//! A chat exported to a folder anyone can open: `chat.html`, a plain transcript, and `files/`
//! with its attachments.

use crate::projection::{Message, MessageKind, TextStyle};

/// A message as exported: who wrote it and its files, each a path relative to the page (None:
/// not downloaded) with its type and name.
pub(crate) struct Exported<'a> {
    pub message: &'a Message,
    pub author: String,
    pub files: Vec<ExportedFile>,
}

pub(crate) struct ExportedFile {
    pub path: Option<String>,
    /// Downloaded once, but its file is gone (only with no `path`).
    pub missing: bool,
    pub content_type: String,
    pub name: String,
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// A file or folder name for `s`: no path separators, control characters or characters that
/// mean something in a link (`#?%`), not hidden, not empty, at most 200 bytes (names may have
/// 255).
pub(crate) fn file_name(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if matches!(c, '/' | ':' | '#' | '?' | '%') || c.is_control() {
                '-'
            } else {
                c
            }
        })
        .collect();
    let mut out = String::new();
    for c in cleaned.trim().trim_start_matches('.').chars() {
        if out.len() + c.len_utf8() > 200 {
            break;
        }
        out.push(c);
    }
    if out.is_empty() {
        "chat".into()
    } else {
        out
    }
}

/// `text` with each mention's placeholder as "@name" (`name` from an ACI).
pub(crate) fn with_mentions(
    text: &str,
    ranges: &[crate::projection::BodyRange],
    name: impl Fn(&str) -> String,
) -> String {
    let mut mentions: Vec<_> = ranges
        .iter()
        .filter_map(|r| match &r.style {
            TextStyle::Mention { aci } => Some((r.start, aci.as_str())),
            _ => None,
        })
        .collect();
    mentions.sort();
    let mut names = mentions.into_iter().map(|(_, aci)| name(aci));
    text.chars()
        .map(|c| match c {
            '\u{fffc}' => names.next().map(|n| format!("@{n}")).unwrap_or_else(|| "@?".into()),
            c => c.to_string(),
        })
        .collect()
}

/// What a message that isn't text says.
fn kind_text(kind: &MessageKind) -> Option<String> {
    Some(match kind {
        MessageKind::Normal => return None,
        MessageKind::Sticker { emoji } => format!("Sticker {}", emoji.as_deref().unwrap_or("")).trim().into(),
        MessageKind::Contact { name } => format!("Contact: {}", name.as_deref().unwrap_or("unnamed")),
        MessageKind::Payment => "Payment".into(),
        MessageKind::Poll { question } => format!("Poll: {}", question.as_deref().unwrap_or("")),
        MessageKind::GiftBadge => "Gift badge".into(),
        MessageKind::ViewOnce { .. } => "View-once media".into(),
    })
}

const STYLE: &str = "body{font:15px -apple-system,system-ui,sans-serif;max-width:720px;margin:24px auto;padding:0 16px;color:#1d1d1f;background:#fff}\
h1{font-size:22px}.day{color:#86868b;font-size:13px;text-align:center;margin:20px 0 8px}\
.m{margin:6px 0;padding:8px 12px;border-radius:12px;background:#f2f2f7}.m.out{background:#e3efff}\
.who{font-weight:600}.at{color:#86868b;font-size:12px;margin-left:6px}.body{white-space:pre-wrap;margin-top:2px}\
.note{color:#86868b;font-style:italic}blockquote{margin:4px 0;padding:4px 8px;border-left:3px solid #c7c7cc;color:#3a3a3c}\
img{max-width:100%;max-height:360px;border-radius:8px;display:block;margin-top:4px}\
@media(prefers-color-scheme:dark){body{background:#1c1c1e;color:#f5f5f7}.m{background:#2c2c2e}.m.out{background:#1e3a5f}blockquote{color:#d1d1d6}}";

/// The page for the chat `title`, messages oldest first; times in `tz`. `name` names a quote's
/// author or a mention from an ACI.
pub(crate) fn page<Tz: chrono::TimeZone>(
    title: &str,
    messages: &[Exported],
    tz: &Tz,
    name: impl Fn(&str) -> String,
) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let mut out = format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\">\
         <title>{t}</title><style>{STYLE}</style></head><body>\n<h1>{t}</h1>\n",
        t = escape(title)
    );
    let mut day = String::new();
    for e in messages {
        let m = e.message;
        // a time out of range still shows its message, under the day before it
        let at = tz.timestamp_millis_opt(m.sent_at).single();
        if let Some(at) = &at {
            let this_day = at.format("%A, %-d %B %Y").to_string();
            if this_day != day {
                out.push_str(&format!("<div class=\"day\">{}</div>\n", escape(&this_day)));
                day = this_day;
            }
        }
        out.push_str(&format!(
            "<div class=\"m{}\"><span class=\"who\">{}</span><span class=\"at\">{}{}</span>",
            if m.is_outgoing { " out" } else { "" },
            escape(&e.author),
            at.map_or_else(|| "unknown time".into(), |at| at.format("%H:%M").to_string()),
            if m.edited && !m.deleted { " · edited" } else { "" },
        ));
        if m.deleted {
            out.push_str("<div class=\"note\">This message was deleted.</div></div>\n");
            continue;
        }
        if let Some(q) = &m.quote {
            let text = q
                .text
                .as_deref()
                .map(|t| with_mentions(t, &q.ranges, &name))
                .unwrap_or_default();
            out.push_str(&format!(
                "<blockquote><span class=\"who\">{}</span><br>{}</blockquote>",
                escape(&name(&q.author)),
                escape(&text)
            ));
        }
        if let Some(kind) = kind_text(&m.kind) {
            out.push_str(&format!("<div class=\"note\">{}</div>", escape(&kind)));
        }
        for f in &e.files {
            match &f.path {
                Some(path) if f.content_type.starts_with("image/") => out.push_str(&format!(
                    "<a href=\"{p}\"><img src=\"{p}\" alt=\"{n}\"></a>",
                    p = escape(path),
                    n = escape(&f.name)
                )),
                Some(path) => out.push_str(&format!(
                    "<div><a href=\"{}\">{}</a></div>",
                    escape(path),
                    escape(&f.name)
                )),
                None => out.push_str(&format!(
                    "<div class=\"note\">{} ({})</div>",
                    escape(&f.name),
                    if f.missing { "file missing" } else { "not downloaded" }
                )),
            }
        }
        if let Some(body) = m.body.as_deref().filter(|b| !b.is_empty()) {
            out.push_str(&format!(
                "<div class=\"body\">{}</div>",
                escape(&with_mentions(body, &m.ranges, &name))
            ));
        }
        out.push_str("</div>\n");
    }
    out.push_str("</body></html>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::{BodyRange, MessageStatus, Quote};

    fn msg(sent_at: i64, body: &str) -> Message {
        Message {
            conversation_id: "c:a".into(),
            author: "a".into(),
            sent_at,
            received_at: sent_at,
            body: Some(body.into()),
            ranges: vec![],
            is_outgoing: false,
            status: MessageStatus::Delivered,
            edited: false,
            deleted: false,
            quote: None,
            kind: MessageKind::Normal,
            expires_in: 0,
            expires_at: None,
        }
    }

    fn one(m: &Message, files: Vec<ExportedFile>) -> String {
        let e = [Exported {
            message: m,
            author: "Ada".into(),
            files,
        }];
        page("Ada & co", &e, &chrono::Utc, |aci| format!("name-of-{aci}"))
    }

    #[test]
    fn text_is_escaped_so_a_message_cant_be_markup() {
        let html = one(&msg(0, "<script>alert('x')</script> & \"so\""), vec![]);
        assert!(html.contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt; &amp; &quot;so&quot;"));
        assert!(!html.contains("<script>"));
        assert!(html.contains("<title>Ada &amp; co</title>"));
    }

    #[test]
    fn days_times_authors_and_edits_show() {
        let mut edited = msg(86_400_000 + 3_600_000, "later");
        edited.edited = true;
        let (first, second) = (msg(0, "first"), edited);
        let e = [
            Exported {
                message: &first,
                author: "Ada".into(),
                files: vec![],
            },
            Exported {
                message: &second,
                author: "You".into(),
                files: vec![],
            },
        ];
        let html = page("Ada", &e, &chrono::Utc, |a| a.into());
        assert!(html.contains("Thursday, 1 January 1970") && html.contains("Friday, 2 January 1970"));
        assert!(html.contains("00:00") && html.contains("01:00 · edited"));
        assert!(
            html.find("first").unwrap() < html.find("later").unwrap(),
            "oldest first"
        );
    }

    #[test]
    fn mentions_and_quotes_are_named() {
        let mut m = msg(0, "hi \u{fffc} and \u{fffc}");
        let mention = |start, aci: &str| BodyRange {
            start,
            length: 1,
            style: TextStyle::Mention { aci: aci.into() },
        };
        m.ranges = vec![mention(10, "b"), mention(3, "a")];
        m.quote = Some(Quote {
            author: "q".into(),
            sent_at: 1,
            text: Some("quoted".into()),
            ranges: vec![],
        });
        let html = one(&m, vec![]);
        assert!(html.contains("hi @name-of-a and @name-of-b"), "{html}");
        assert!(html.contains("name-of-q</span><br>quoted"));
    }

    #[test]
    fn a_placeholder_without_its_mention_stays_visible() {
        assert_eq!(with_mentions("hi \u{fffc}", &[], |a| a.into()), "hi @?");
    }

    #[test]
    fn a_message_with_an_impossible_time_still_shows() {
        let html = one(&msg(i64::MAX, "still here"), vec![]);
        assert!(html.contains("still here") && html.contains("unknown time"));
    }

    #[test]
    fn a_deleted_message_shows_nothing_of_itself() {
        let mut m = msg(0, "secret");
        m.deleted = true;
        let html = one(&m, vec![]);
        assert!(!html.contains("secret") && html.contains("This message was deleted."));
    }

    #[test]
    fn files_link_pictures_show_and_missing_ones_say_so() {
        let html = one(
            &msg(0, ""),
            vec![
                ExportedFile {
                    path: None,
                    missing: true,
                    content_type: "image/png".into(),
                    name: "gone.png".into(),
                },
                ExportedFile {
                    path: Some("files/1-cat.jpg".into()),
                    missing: false,
                    content_type: "image/jpeg".into(),
                    name: "cat.jpg".into(),
                },
                ExportedFile {
                    path: Some("files/2-notes.pdf".into()),
                    missing: false,
                    content_type: "application/pdf".into(),
                    name: "notes.pdf".into(),
                },
                ExportedFile {
                    path: None,
                    missing: false,
                    content_type: "video/mp4".into(),
                    name: "clip.mp4".into(),
                },
            ],
        );
        assert!(html.contains("<img src=\"files/1-cat.jpg\""));
        assert!(html.contains("<a href=\"files/2-notes.pdf\">notes.pdf</a>"));
        assert!(html.contains("clip.mp4 (not downloaded)"));
        assert!(html.contains("gone.png (file missing)"));
    }

    #[test]
    fn other_kinds_say_what_they_were() {
        let mut m = msg(0, "");
        m.kind = MessageKind::Poll {
            question: Some("Lunch?".into()),
        };
        assert!(one(&m, vec![]).contains("Poll: Lunch?"));
    }

    #[test]
    fn names_become_safe_file_names() {
        assert_eq!(file_name("Ada / Bob: plans"), "Ada - Bob- plans");
        assert_eq!(file_name("..hidden"), "hidden");
        assert_eq!(file_name("  "), "chat");
        assert_eq!(
            file_name("Q3 #2? 100%.pdf"),
            "Q3 -2- 100-.pdf",
            "nothing a link reads as more"
        );
        assert_eq!(file_name(&"x".repeat(300)).len(), 200);
        let cjk = file_name(&"雨".repeat(100));
        assert!(
            cjk.len() <= 200 && cjk.chars().all(|c| c == '雨'),
            "cut by bytes, at a character"
        );
    }
}
