//! Sticker packs: the installed ones (presage keeps them, with their images) shown as files for
//! the picker, and the links that add one.

use std::path::{Path, PathBuf};

use crate::projection::model::{StickerInfo, StickerPackInfo};

/// A pack as the projection keeps it: what the picker shows and what sending needs, its images
/// written under the stickers folder.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct KeptPack {
    pub id: Vec<u8>,
    pub key: Vec<u8>,
    pub title: String,
    pub author: String,
    pub stickers: Vec<KeptSticker>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct KeptSticker {
    pub id: u32,
    pub emoji: Option<String>,
    /// presage's, else empty.
    pub content_type: String,
}

/// A pack id is 16 bytes, its key 32.
const PACK_ID_LEN: usize = 16;
const PACK_KEY_LEN: usize = 32;

/// The pack (id, key) a sticker link names: `https://signal.art/addstickers/#pack_id=…&pack_key=…`
/// or `sgnl://addstickers/?pack_id=…&pack_key=…`. None for anything else.
pub(crate) fn parse_link(link: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let url = url::Url::parse(link.trim()).ok()?;
    let params = match (url.scheme(), url.host_str()) {
        ("https", Some("signal.art")) if url.path() == "/addstickers/" || url.path() == "/addstickers" => {
            url.fragment()?.to_string()
        }
        ("sgnl", Some("addstickers")) => url.query()?.to_string(),
        _ => return None,
    };
    let (mut id, mut key) = (None, None);
    for (name, value) in url::form_urlencoded::parse(params.as_bytes()) {
        match name.as_ref() {
            "pack_id" => id = hex::decode(value.as_ref()).ok(),
            "pack_key" => key = hex::decode(value.as_ref()).ok(),
            _ => {}
        }
    }
    let (id, key) = (id?, key?);
    (id.len() == PACK_ID_LEN && key.len() == PACK_KEY_LEN).then_some((id, key))
}

/// What to keep of presage's `pack`: each sticker presage has the image of, written once to
/// `<dir>/<pack id>/<sticker id>` (a sticker without one, or that can't be written, is left out),
/// and whether every image presage has was written. Blocking file IO: off the core's thread.
pub(crate) fn keep(pack: &presage::store::StickerPack, dir: &Path) -> (KeptPack, bool) {
    let mut complete = true;
    let stickers = pack
        .manifest
        .stickers
        .iter()
        .filter_map(|s| {
            let bytes = s.bytes.as_ref()?;
            let path = image_path(dir, &pack.id, s.id);
            if !path.exists() {
                let written = path
                    .parent()
                    .map_or(Ok(()), std::fs::create_dir_all)
                    .and_then(|_| std::fs::write(&path, bytes));
                complete &= written.is_ok();
                written.ok()?;
            }
            Some(KeptSticker {
                id: s.id,
                emoji: s.emoji.clone().filter(|e| !e.is_empty()),
                content_type: s.content_type.clone().unwrap_or_default(),
            })
        })
        .collect();
    let kept = KeptPack {
        id: pack.id.clone(),
        key: pack.key.clone(),
        title: pack.manifest.title.clone(),
        author: pack.manifest.author.clone(),
        stickers,
    };
    (kept, complete)
}

/// Whether every kept sticker's image is still in `dir`. Blocking file IO.
pub(crate) fn has_images(pack: &KeptPack, dir: &Path) -> bool {
    pack.stickers.iter().all(|s| image_path(dir, &pack.id, s.id).exists())
}

/// A kept pack as the picker shows it.
pub(crate) fn pack_info(pack: &KeptPack, dir: &Path) -> StickerPackInfo {
    StickerPackInfo {
        id: hex::encode(&pack.id),
        title: pack.title.clone(),
        author: pack.author.clone(),
        stickers: pack.stickers.iter().map(|s| sticker_info(pack, s, dir)).collect(),
    }
}

/// One sticker of a kept pack as `pack_info` shows it.
pub(crate) fn sticker_info(pack: &KeptPack, s: &KeptSticker, dir: &Path) -> StickerInfo {
    StickerInfo {
        id: s.id,
        emoji: s.emoji.clone(),
        path: image_path(dir, &pack.id, s.id).to_string_lossy().into_owned(),
    }
}

fn image_path(dir: &Path, pack_id: &[u8], sticker_id: u32) -> PathBuf {
    dir.join(hex::encode(pack_id)).join(sticker_id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0123456789abcdef0123456789abcdef";
    const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    #[test]
    fn a_sticker_link_names_its_pack() {
        let want = Some((hex::decode(ID).unwrap(), hex::decode(KEY).unwrap()));
        assert_eq!(
            parse_link(&format!("https://signal.art/addstickers/#pack_id={ID}&pack_key={KEY}")),
            want
        );
        assert_eq!(
            parse_link(&format!("  sgnl://addstickers/?pack_key={KEY}&pack_id={ID}\n")),
            want
        );
        for bad in [
            format!("https://example.com/addstickers/#pack_id={ID}&pack_key={KEY}"),
            format!("http://signal.art/addstickers/#pack_id={ID}&pack_key={KEY}"),
            format!("https://signal.art/addstickers/#pack_id={ID}"),
            format!("https://signal.art/addstickers/#pack_id={ID}&pack_key=zz"),
            format!("https://signal.art/addstickers/#pack_id=0123&pack_key={KEY}"),
            "not a link".into(),
        ] {
            assert_eq!(parse_link(&bad), None, "{bad}");
        }
    }

    #[test]
    fn a_pack_shows_the_stickers_it_has_images_of() {
        let dir = std::env::temp_dir().join(format!("sig-stickers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sticker = |id, emoji: &str, bytes: Option<&[u8]>| presage::store::Sticker {
            id,
            emoji: Some(emoji.into()),
            content_type: Some("image/webp".into()),
            bytes: bytes.map(<[u8]>::to_vec),
        };
        let pack = presage::store::StickerPack {
            id: hex::decode(ID).unwrap(),
            key: hex::decode(KEY).unwrap(),
            manifest: presage::store::StickerPackManifest {
                title: "Foxes".into(),
                author: "Ana".into(),
                cover: None,
                stickers: vec![
                    sticker(0, "🦊", Some(b"fox")),
                    sticker(1, "", None),
                    sticker(2, "", Some(b"den")),
                ],
            },
        };
        let (kept, complete) = keep(&pack, &dir);
        assert!(complete);
        assert_eq!(kept.stickers[0].content_type, "image/webp");
        let info = pack_info(&kept, &dir);
        assert_eq!(
            (info.id.as_str(), info.title.as_str(), info.author.as_str()),
            (ID, "Foxes", "Ana")
        );
        let shown: Vec<_> = info.stickers.iter().map(|s| (s.id, s.emoji.as_deref())).collect();
        assert_eq!(shown, vec![(0, Some("🦊")), (2, None)]);
        assert_eq!(std::fs::read(&info.stickers[0].path).unwrap(), b"fox");
        // written once: keeping again keeps the file
        std::fs::write(&info.stickers[0].path, b"kept").unwrap();
        let (again, _) = keep(&pack, &dir);
        assert_eq!(
            std::fs::read(&pack_info(&again, &dir).stickers[0].path).unwrap(),
            b"kept"
        );
        assert!(has_images(&kept, &dir));
        std::fs::remove_file(&info.stickers[1].path).unwrap();
        assert!(!has_images(&kept, &dir));
    }
}
