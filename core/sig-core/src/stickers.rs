//! Sticker packs: the installed ones (presage keeps them, with their images) shown as files for
//! the picker, and the links that add one.

use std::path::Path;

use crate::projection::model::{StickerInfo, StickerPackInfo};

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

/// A pack as the picker shows it: each sticker presage has the image of, written once to
/// `<dir>/<pack id>/<sticker id>` (a sticker without one, or that can't be written, is left out).
pub(crate) fn pack_info(pack: &presage::store::StickerPack, dir: &Path) -> StickerPackInfo {
    let id = hex::encode(&pack.id);
    let folder = dir.join(&id);
    let stickers = pack
        .manifest
        .stickers
        .iter()
        .filter_map(|s| {
            let bytes = s.bytes.as_ref()?;
            let path = folder.join(s.id.to_string());
            if !path.exists() {
                std::fs::create_dir_all(&folder)
                    .and_then(|_| std::fs::write(&path, bytes))
                    .ok()?;
            }
            Some(StickerInfo {
                id: s.id,
                emoji: s.emoji.clone().filter(|e| !e.is_empty()),
                path: path.to_string_lossy().into_owned(),
            })
        })
        .collect();
    StickerPackInfo {
        id,
        title: pack.manifest.title.clone(),
        author: pack.manifest.author.clone(),
        stickers,
    }
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
        let info = pack_info(&pack, &dir);
        assert_eq!(
            (info.id.as_str(), info.title.as_str(), info.author.as_str()),
            (ID, "Foxes", "Ana")
        );
        let shown: Vec<_> = info.stickers.iter().map(|s| (s.id, s.emoji.as_deref())).collect();
        assert_eq!(shown, vec![(0, Some("🦊")), (2, None)]);
        assert_eq!(std::fs::read(&info.stickers[0].path).unwrap(), b"fox");
        // written once: listing again keeps the file
        std::fs::write(&info.stickers[0].path, b"kept").unwrap();
        let again = pack_info(&pack, &dir);
        assert_eq!(std::fs::read(&again.stickers[0].path).unwrap(), b"kept");
    }
}
