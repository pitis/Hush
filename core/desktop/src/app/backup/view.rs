//! What Back Up and Restore draw: the Mac app's sheets (`BackupSheet`, `RestoreSheet`) as a
//! dialog over the window, and the link screen's note and Restore from a Backup….

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::Input;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{h_flex, Disableable, FocusTrapElement, Sizable};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::{acceptable, Kind};
use crate::app::view::{card, field, label, modal};
use crate::app::Hush;
use crate::style::Palette;

impl Hush {
    /// Back Up's or Restore's dialog over the window.
    pub(in crate::app) fn render_backup(&self, p: Palette, cx: &mut Context<Self>) -> Option<Div> {
        let d = self.backup.dialog.as_ref()?;
        let secure = |state, name: &'static str| {
            field(
                Input::new(state)
                    .appearance(false)
                    .aria_label(name)
                    .disabled(d.working)
                    .w_full(),
                p,
            )
        };
        let (title, detail, fields, primary, ready) = match &d.kind {
            Kind::BackUp { saved: Some(name), .. } => (
                "Back Up Your Chats".to_string(),
                format!("Backed up to {name}."),
                vec![],
                "Done",
                true,
            ),
            Kind::BackUp { again, saved: None } => (
                "Back Up Your Chats".to_string(),
                "Every chat and file on this computer goes into one file, encrypted with a passphrase. On a new \
                 computer, restore it before linking. Without the passphrase, nobody can open it, you included."
                    .to_string(),
                vec![secure(&d.passphrase, "Passphrase"), secure(again, "Passphrase again")],
                "Back Up…",
                acceptable(&d.passphrase.read(cx).value(), &again.read(cx).value()),
            ),
            Kind::Restore(file) => (
                format!("Restore {}?", file.file_name().unwrap_or_default().to_string_lossy()),
                "Its chats and files replace any on this computer. Then link this computer to your phone: \
                 history the phone transfers is merged in."
                    .to_string(),
                vec![secure(&d.passphrase, "Passphrase")],
                "Restore",
                !d.passphrase.read(cx).value().is_empty(),
            ),
        };
        let done = matches!(d.kind, Kind::BackUp { saved: Some(_), .. });
        Some(modal(
            card(p)
                .id("backup")
                .role(Role::Dialog)
                .aria_label(title.clone())
                .w(px(420.))
                .on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| {
                    if e.keystroke.key == "escape" {
                        this.close_backup(cx);
                    }
                }))
                .child(
                    label("backup-title", Role::Heading, title)
                        .aria_level(1)
                        .text_size(px(17.))
                        .font_weight(FontWeight::SEMIBOLD),
                )
                .child(
                    label("backup-detail", Role::Label, detail)
                        .when(!done, |this| this.text_size(px(12.)).text_color(p.secondary)),
                )
                .children(fields)
                .children(d.failure.clone().map(|why| {
                    label("backup-failure", Role::Alert, why)
                        .text_size(px(12.))
                        .text_color(p.danger)
                }))
                .child(
                    h_flex()
                        .gap(px(8.))
                        .child(div().flex_1())
                        .when(d.working, |this| this.child(Spinner::new().xsmall()))
                        .when(!done, |this| {
                            this.child(
                                Button::new("backup-cancel")
                                    .label("Cancel")
                                    .disabled(d.working)
                                    .on_click(cx.listener(|this, _, _, cx| this.close_backup(cx))),
                            )
                        })
                        .child(
                            Button::new("backup-primary")
                                .primary()
                                .label(primary)
                                .disabled(d.working || !ready)
                                .on_click(cx.listener(|this, _, window, cx| this.submit_backup(window, cx))),
                        ),
                )
                .focus_trap("backup", &d.focus),
        ))
    }

    /// On the link screen, above the code: how the last restore went.
    pub(in crate::app) fn render_restore_note(&self) -> Option<Stateful<Div>> {
        let note = self.backup.note.clone()?;
        Some(
            label("restore-note", Role::Status, note)
                .max_w(px(360.))
                .text_center()
                .text_size(px(12.))
                .font_weight(FontWeight::BOLD),
        )
    }

    /// The link screen's last line (`RestoreButton`), in the accent as macOS draws a link button.
    pub(in crate::app) fn render_restore_button(&self, p: Palette, cx: &mut Context<Self>) -> Button {
        Button::new("restore")
            .link()
            .label("Restore from a Backup…")
            .text_color(p.accent)
            .on_click(cx.listener(|this, _, window, cx| this.choose_restore(window, cx)))
    }
}
