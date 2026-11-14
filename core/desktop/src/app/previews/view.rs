//! How link previews are drawn (`LinkPreviewCard`, `PreviewDraftCard`): in a bubble under its
//! files, and above the composer.

use std::sync::Arc;

use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::{host, openable, Card};
use crate::app::view::icon;
use crate::app::Hush;
use crate::style::Palette;

/// A received preview: picture, title and site, 280pt wide; a click opens its https link. `color`
/// is the bubble's words'.
pub fn card(card: &Option<Card>, this: &WeakEntity<Hush>, color: Hsla) -> Option<Stateful<Div>> {
    let Card { preview, picture } = card.as_ref()?;
    let site = host(&preview.url).to_string();
    let label = match &preview.title {
        Some(title) => format!("{title}, {site}"),
        None => site.clone(),
    };
    let opens = openable(&preview.url).is_some().then(|| preview.url.clone());
    let this = this.clone();
    Some(
        h_flex()
            .id("preview")
            .role(Role::Link)
            .aria_label(label)
            // the Mac's 280 and its 8 of padding around
            .w(px(296.))
            .p(px(8.))
            .gap(px(10.))
            .rounded(px(10.))
            .bg(black().opacity(0.18))
            .children(picture.clone().map(|p| thumb(p, 52., 8.)))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .children(preview.title.clone().map(|title| {
                        div()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .line_clamp(2)
                            .child(title)
                    }))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(color.opacity(0.7))
                            .truncate()
                            .child(site),
                    ),
            )
            .when_some(opens, |card, url| {
                card.cursor_pointer().on_click(move |_, _, cx| {
                    let _ = this.update(cx, |app, cx| app.open_link(&url, cx));
                })
            }),
    )
}

/// A square of a picture, a grey one until it is decoded.
fn thumb(picture: Option<Arc<RenderImage>>, side: f32, corner: f32) -> Div {
    let frame = div().size(px(side)).flex_none().rounded(px(corner)).overflow_hidden();
    match picture {
        // GPUI clips children to a rectangle: the picture rounds its own corners
        Some(p) => frame.child(img(p).size_full().rounded(px(corner)).object_fit(ObjectFit::Cover)),
        None => frame.bg(black().opacity(0.25)),
    }
}

impl Hush {
    /// The preview that will go with the message, with ✕ to leave it off.
    pub(in crate::app) fn render_link_draft(&self, p: Palette, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let draft = self.previews.draft.as_ref()?;
        let site = host(&draft.url).to_string();
        let picture = draft
            .image
            .as_ref()
            .map(|i| self.previews.pictures.get(&i.path).cloned().flatten());
        Some(
            h_flex()
                .id("link-draft")
                .role(Role::Group)
                .aria_label("Link preview")
                .gap(px(10.))
                .children(picture.map(|picture| thumb(picture, 44., 6.)))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(px(2.))
                        .child(
                            div()
                                .text_size(px(12.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .truncate()
                                .child(draft.title.clone().unwrap_or_else(|| site.clone())),
                        )
                        .children(draft.description.clone().map(|description| {
                            div()
                                .text_size(px(11.))
                                .text_color(p.secondary)
                                .line_clamp(2)
                                .child(description)
                        }))
                        .child(div().text_size(px(11.)).text_color(p.secondary).truncate().child(site)),
                )
                .child(
                    div()
                        .id("link-draft-close")
                        .role(Role::Button)
                        .aria_label("Remove the link preview")
                        .cursor_pointer()
                        .child(icon("circle-x", 14., p.secondary))
                        .on_click(cx.listener(|this, _, _, cx| this.dismiss_preview(cx))),
                ),
        )
    }
}
