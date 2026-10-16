//! Link previews for what we send: the first https link in a message, and the title,
//! description and image its page declares (Open Graph tags, else its `<title>`).

use url::Url;

/// What a page says about itself.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct PageInfo {
    pub title: Option<String>,
    pub description: Option<String>,
    /// An https image URL, resolved against the page.
    pub image: Option<Url>,
}

/// A page's preview, with its image's bytes and type when it has one.
#[derive(Debug)]
pub(crate) struct Fetched {
    pub info: PageInfo,
    pub image: Option<(Vec<u8>, String)>,
}

const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const MAX_PAGE: usize = 1024 * 1024;
const MAX_IMAGE: usize = 2 * 1024 * 1024;

/// Fetches `page` and its preview image: https only (redirects too), a page of at most 1 MB
/// that is HTML, an image of at most 2 MB that is JPEG, PNG, WebP or GIF. Errors name the
/// cause, never the page's content.
pub(crate) async fn fetch(page: &Url) -> Result<Fetched, String> {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 || attempt.url().scheme() != "https" {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .user_agent("Mozilla/5.0 (Macintosh) link preview")
        .build()
        .map_err(|e| format!("no web client: {e}"))?;
    let (html, final_url) = get(&client, page, MAX_PAGE, true, |t| t.starts_with("text/html")).await?;
    let info = parse(&String::from_utf8_lossy(&html), &final_url);
    let image = match &info.image {
        Some(url) => get(&client, url, MAX_IMAGE, false, |t| {
            ["image/jpeg", "image/png", "image/webp", "image/gif"].contains(&t)
        })
        .await
        .ok()
        .map(|(bytes, _)| bytes)
        .and_then(|bytes| Some((image_type(&bytes)?.to_string(), bytes)))
        .map(|(t, bytes)| (bytes, t)),
        None => None,
    };
    Ok(Fetched { info, image })
}

/// GETs `url` (still https after redirects) when its type passes `accept`, up to `max` bytes:
/// past that a page is cut (its head is what's read), anything else refused.
async fn get(
    client: &reqwest::Client,
    url: &Url,
    max: usize,
    cut: bool,
    accept: impl Fn(&str) -> bool,
) -> Result<(Vec<u8>, Url), String> {
    let mut res = client
        .get(url.as_str())
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    if !res.status().is_success() || res.url().scheme() != "https" {
        return Err(format!("status {}", res.status()));
    }
    let content_type = res
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or("").trim().to_ascii_lowercase())
        .unwrap_or_default();
    if !accept(&content_type) {
        return Err(format!("not a preview: {content_type}"));
    }
    let final_url = res.url().clone();
    let mut body = Vec::new();
    while let Some(chunk) = res.chunk().await.map_err(|e| e.without_url().to_string())? {
        if body.len() + chunk.len() > max {
            if cut {
                body.extend_from_slice(&chunk[..max - body.len()]);
                break;
            }
            return Err("too large".into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok((body, final_url))
}

/// An image's type from its first bytes (what's sent must be what it is).
fn image_type(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        _ => None,
    }
}

/// The first https link in `text`, without the punctuation that ends a sentence around it: as
/// typed (a preview's link must be found in its message) and parsed.
pub(crate) fn first_link(text: &str) -> Option<(String, Url)> {
    text.split_whitespace().find_map(|word| {
        let start = word.find("https://")?;
        let link = word[start..].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '>', '"', '\'']);
        let url = Url::parse(link)
            .ok()
            .filter(|u| u.scheme() == "https" && u.host_str().is_some())?;
        Some((link.to_string(), url))
    })
}

/// The page's title, description and image from its HTML.
pub(crate) fn parse(html: &str, page: &Url) -> PageInfo {
    let mut info = PageInfo::default();
    let mut fallback_title = None;
    let lower = html.to_ascii_lowercase();
    let mut at = 0;
    while let Some(i) = lower[at..].find('<') {
        let start = at + i;
        let Some(len) = lower[start..].find('>') else { break };
        let tag = &html[start..start + len + 1];
        let tag_lower = &lower[start..start + len + 1];
        at = start + len + 1;
        if tag_lower.starts_with("<meta") {
            let key = attr(tag, "property")
                .or_else(|| attr(tag, "name"))
                .map(|k| k.to_ascii_lowercase());
            let Some(content) = attr(tag, "content")
                .map(|c| decode(&c))
                .filter(|c| !c.trim().is_empty())
            else {
                continue;
            };
            match key.as_deref() {
                Some("og:title") if info.title.is_none() => info.title = Some(content.trim().to_string()),
                Some("og:description") if info.description.is_none() => {
                    info.description = Some(content.trim().to_string())
                }
                Some("og:image" | "og:image:url" | "og:image:secure_url" | "twitter:image") if info.image.is_none() => {
                    info.image = page.join(content.trim()).ok().filter(|u| u.scheme() == "https");
                }
                _ => {}
            }
        } else if tag_lower.starts_with("<title") && fallback_title.is_none() {
            if let Some(end) = lower[at..].find("</title") {
                fallback_title = Some(decode(&html[at..at + end]).trim().to_string()).filter(|t| !t.is_empty());
            }
        }
    }
    info.title = info.title.or(fallback_title);
    info
}

/// An attribute's value in `tag` (quoted with " or ', or bare).
fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(name) {
        let at = from + i;
        from = at + name.len();
        // a whole attribute name, followed by =
        let before = lower[..at].chars().last();
        if !before.is_some_and(char::is_whitespace) {
            continue;
        }
        let rest = tag[at + name.len()..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else { continue };
        let rest = rest.trim_start();
        return match rest.chars().next() {
            Some(q @ ('"' | '\'')) => rest[1..].find(q).map(|end| rest[1..1 + end].to_string()),
            Some(_) => Some(
                rest.split(|c: char| c.is_whitespace() || c == '>' || c == '/')
                    .next()?
                    .to_string(),
            ),
            None => None,
        };
    }
    None
}

/// HTML's named and numeric character references, the common ones.
fn decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|&e| e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let ch = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                u32::from_str_radix(&entity[2..], 16).ok().and_then(char::from_u32)
            }
            _ if entity.starts_with('#') => entity[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real page over the network: run by hand (`cargo test -- --ignored live_page`).
    #[tokio::test]
    #[ignore]
    async fn live_page() {
        let f = fetch(&Url::parse("https://example.com/").unwrap()).await.unwrap();
        assert_eq!(f.info.title.as_deref(), Some("Example Domain"));
    }

    #[test]
    fn an_images_type_comes_from_its_bytes() {
        assert_eq!(image_type(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(image_type(b"\x89PNG\r\n"), Some("image/png"));
        assert_eq!(image_type(b"RIFF1234WEBPVP8 "), Some("image/webp"));
        assert_eq!(image_type(b"<html>"), None);
    }

    fn page() -> Url {
        Url::parse("https://example.com/articles/1").unwrap()
    }

    #[test]
    fn the_first_https_link_is_taken_without_the_sentence_around_it() {
        let link = |text| first_link(text).map(|(typed, _)| typed);
        assert_eq!(
            link("look (https://example.com/a?b=1). nice"),
            Some("https://example.com/a?b=1".into())
        );
        assert_eq!(link("plain http://example.com isn't fetched"), None);
        assert_eq!(link("no link here"), None);
        // as typed, though parsing would add the slash
        assert_eq!(
            link("two: https://one.example and https://two.example"),
            Some("https://one.example".into())
        );
    }

    #[test]
    fn open_graph_tags_say_what_the_page_is() {
        let html = r#"<html><head><title>Fallback</title>
            <meta property="og:title" content="Tom &amp; Jerry &#8212; a story">
            <META name='og:description' content='It&#39;s about a cat'>
            <meta property="og:image" content="/img/cover.jpg"/>
            </head></html>"#;
        assert_eq!(
            parse(html, &page()),
            PageInfo {
                title: Some("Tom & Jerry — a story".into()),
                description: Some("It's about a cat".into()),
                image: Some(Url::parse("https://example.com/img/cover.jpg").unwrap()),
            }
        );
    }

    #[test]
    fn without_tags_the_title_stands_and_no_http_image_is_taken() {
        let html = r#"<title> Plain page </title><meta property="og:image" content="http://example.com/x.png">"#;
        assert_eq!(
            parse(html, &page()),
            PageInfo {
                title: Some("Plain page".into()),
                ..Default::default()
            }
        );
        assert_eq!(parse("<p>nothing</p>", &page()), PageInfo::default());
        // a broken page doesn't panic
        assert_eq!(
            parse("<meta property=\"og:title\" content=\"x", &page()),
            PageInfo::default()
        );
    }
}
