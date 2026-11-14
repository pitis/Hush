//! How a message's files are drawn in its bubble (`AttachmentViews.swift`'s sizes).

use std::sync::Arc;

use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{h_flex, v_flex, Sizable};
use gpui_kit::prelude::*;
use gpui_kit::*;
use sig_core::projection::{Attachment, AttachmentState};

use super::{byte_count, clock, display_size, openable, Audio, Look, Shown, TRACK};
use crate::app::view::icon;
use crate::app::Hush;

/// A message's files, one under the other. `color` is the bubble's words', `behind` its fill.
pub fn render(files: &[Shown], this: &WeakEntity<Hush>, color: Hsla, behind: Hsla) -> Option<Div> {
    if files.is_empty() {
        return None;
    }
    let shown = files.iter().map(|f| match &f.look {
        Look::Picture(thumb) => picture(&f.file, thumb.clone(), this, color, behind),
        Look::Audio(audio) => recording(&f.file, audio, this, color, behind),
        Look::File => file_row(&f.file, this, color, behind),
    });
    Some(v_flex().gap(px(4.)).children(shown))
}

fn picture(
    a: &Attachment,
    thumb: Option<Arc<RenderImage>>,
    this: &WeakEntity<Hush>,
    color: Hsla,
    behind: Hsla,
) -> AnyElement {
    let (w, h) = display_size(a.width, a.height);
    let frame = div()
        .id(("file", a.id as u64))
        .w(px(w))
        .h(px(h))
        .flex_none()
        .rounded(px(12.))
        .overflow_hidden();
    match thumb {
        Some(thumb) => frame
            .role(Role::Button)
            .aria_label(a.file_name.clone().unwrap_or_else(|| "Photo".into()))
            .cursor_pointer()
            // GPUI clips children to a rectangle: the picture rounds its own corners
            .child(img(thumb).size_full().rounded(px(12.)).object_fit(ObjectFit::Cover))
            .on_click(on(this, a, Hush::open_file))
            .context_menu(menu(this, a))
            .into_any_element(),
        None => frame
            .role(Role::Image)
            .aria_label(format!("Photo, {}", byte_count(a.size)))
            .bg(black().opacity(0.25))
            .flex()
            .items_center()
            .justify_center()
            .children(download_state(a, color, behind))
            .when(wants_download(a), |this_| {
                this_.cursor_pointer().on_click(on(this, a, Hush::download))
            })
            .into_any_element(),
    }
}

/// Voice notes and audio files: play/pause, how far, how long.
fn recording(a: &Attachment, audio: &Audio, this: &WeakEntity<Hush>, color: Hsla, behind: Hsla) -> AnyElement {
    let name = match &a.file_name {
        _ if a.is_voice_note => "Voice message".to_string(),
        Some(name) => name.clone(),
        None => "Audio".into(),
    };
    let row = h_flex()
        .id(("file", a.id as u64))
        .role(Role::Group)
        .aria_label(name)
        .gap(px(10.));
    // one this can't play still opens in the system's player, from its menu
    let row = if audio.unplayable {
        row.child(icon("triangle-alert", 14., orange()))
            .child(div().text_size(px(12.)).child("This recording can't be played"))
    } else {
        playable(row, a, audio, this, color, behind)
    };
    match a.state {
        AttachmentState::Ready => row.context_menu(menu(this, a)).into_any_element(),
        _ => row.into_any_element(),
    }
}

/// Play/pause (or the download), the track and the time.
fn playable(
    row: Stateful<Div>,
    a: &Attachment,
    audio: &Audio,
    this: &WeakEntity<Hush>,
    color: Hsla,
    behind: Hsla,
) -> Stateful<Div> {
    let control = match a.state {
        AttachmentState::Ready => div()
            .id(("play", a.id as u64))
            .role(Role::Button)
            .aria_label(if audio.playing { "Pause" } else { "Play" })
            .cursor_pointer()
            .child(disc(if audio.playing { "pause" } else { "play" }, 28., color, behind))
            .on_click(on(this, a, Hush::toggle_play)),
        _ => div()
            .id(("play", a.id as u64))
            .children(download_state(a, color, behind))
            .when(wants_download(a), |this_| {
                this_.cursor_pointer().on_click(on(this, a, Hush::download))
            }),
    };
    let time = if audio.playing || audio.elapsed > 0. {
        audio.elapsed
    } else {
        audio.length
    };
    row.child(control)
        // how far it has played
        .child(
            div()
                .w(px(TRACK as f32))
                .h(px(4.))
                .flex_none()
                .rounded_full()
                .bg(color.opacity(0.25))
                .child(
                    div()
                        .h_full()
                        .rounded_full()
                        .bg(color)
                        .w(px((TRACK * audio.fraction) as f32)),
                ),
        )
        .child(
            div()
                .text_size(px(11.))
                .font_features(FontFeatures(Arc::new(vec![("tnum".into(), 1)])))
                .text_color(color.opacity(0.8))
                .child(clock(time)),
        )
}

/// Any other file: its icon, name and size; a click opens it, or downloads it.
fn file_row(a: &Attachment, this: &WeakEntity<Hush>, color: Hsla, behind: Hsla) -> AnyElement {
    let name = a.file_name.clone().unwrap_or_else(|| "File".into());
    let row = h_flex()
        .id(("file", a.id as u64))
        .role(Role::Button)
        .aria_label(format!("{name}, {}", byte_count(a.size)))
        .w(px(260.))
        .gap(px(10.))
        .child(icon(file_icon(&a.content_type), 32., color))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap(px(2.))
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(FontWeight::MEDIUM)
                        .truncate()
                        .child(name),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(color.opacity(0.7))
                        .child(byte_count(a.size)),
                ),
        )
        .children(download_state(a, color, behind).map(|state| div().pl(px(8.)).child(state)));
    match a.state {
        AttachmentState::Ready => row
            .cursor_pointer()
            .on_click(on(this, a, Hush::open_file))
            .context_menu(menu(this, a))
            .into_any_element(),
        _ if wants_download(a) => row
            .cursor_pointer()
            .on_click(on(this, a, Hush::download))
            .into_any_element(),
        _ => row.into_any_element(),
    }
}

/// Not here yet, and not on its way.
fn wants_download(a: &Attachment) -> bool {
    matches!(a.state, AttachmentState::Pending | AttachmentState::Failed)
}

/// What a file that isn't here yet shows: a download button, a spinner, or try again.
fn download_state(a: &Attachment, color: Hsla, behind: Hsla) -> Option<AnyElement> {
    Some(match a.state {
        AttachmentState::Ready => return None,
        AttachmentState::Downloading => Spinner::new().small().color(color).into_any_element(),
        AttachmentState::Failed => h_flex()
            .id(("retry", a.id as u64))
            .role(Role::Button)
            .aria_label("Retry")
            .gap(px(4.))
            .text_size(px(13.))
            .text_color(orange())
            .child(icon("refresh-ccw", 13., orange()))
            .child("Retry")
            .into_any_element(),
        AttachmentState::Pending => div()
            .id(("download", a.id as u64))
            .role(Role::Button)
            .aria_label("Download")
            .child(disc("arrow-down", 26., color, behind))
            .into_any_element(),
    })
}

/// The system's orange, which the Mac app says what went wrong with a file in.
fn orange() -> Hsla {
    rgb(0xff9500).into()
}

/// An SF Symbol's ".circle.fill": a circle of `color` with the glyph cut out of it in `behind`.
fn disc(glyph: &str, size: f32, color: Hsla, behind: Hsla) -> Div {
    div()
        .size(px(size))
        .flex_none()
        .rounded_full()
        .bg(color)
        .flex()
        .items_center()
        .justify_center()
        .child(icon(glyph, size * 0.55, behind))
}

/// The system's icon for the type, as near as Lucide has it.
fn file_icon(content_type: &str) -> &'static str {
    match content_type {
        t if t.starts_with("video/") => "file-video-camera",
        t if t.starts_with("audio/") => "file-music",
        t if t.starts_with("image/") => "file-image",
        t if t.starts_with("text/") || t == "application/pdf" => "file-text",
        "application/zip" | "application/gzip" | "application/x-tar" | "application/x-7z-compressed" => "file-archive",
        _ => "file",
    }
}

/// A file's Open and Save As… on right-click; a program or a script has Save As… only.
fn menu(
    this: &WeakEntity<Hush>,
    a: &Attachment,
) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
    let (this, a) = (this.clone(), a.clone());
    move |menu, _, _| {
        menu.when(openable(&a), |menu| {
            menu.item(PopupMenuItem::new("Open").on_click(on(&this, &a, Hush::open_file)))
        })
        .item(PopupMenuItem::new("Save As…").on_click(on(&this, &a, Hush::save_file)))
    }
}

/// A click that does `act` with the file.
fn on(
    this: &WeakEntity<Hush>,
    a: &Attachment,
    act: fn(&mut Hush, &Attachment, &mut Context<Hush>),
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let (this, a) = (this.clone(), a.clone());
    move |_, _, cx| {
        let _ = this.update(cx, |app, cx| act(app, &a, cx));
    }
}
