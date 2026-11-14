//! What Settings draws: the Mac app's Settings window (`HushApp.swift`'s `SettingsView`,
//! `ProfileSettings.swift`) as a panel over the window, its button by the chat list's search, and
//! the unlinked screen (`ContentView.swift`'s `UnlinkedView`).

use gpui_kit::component::button::{Button, ButtonGroup, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::radio::{Radio, RadioGroup};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{h_flex, v_flex, Disableable, FocusTrapElement, Selectable as _, Sizable};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::{chosen, Appearance, Choice, NotificationContent, Panel};
use crate::app::view::{card, empty_state, field, icon, label, modal};
use crate::app::{Hush, Screen};
use crate::style::Palette;

/// The labels' column, as a macOS form's.
const LABELS: f32 = 120.;

impl Hush {
    /// The Settings panel over the window: the account's part only while linked and in use.
    pub(in crate::app) fn render_settings(&self, p: Palette, cx: &mut Context<Self>) -> Option<Div> {
        let s = self.settings.panel.as_ref()?;
        let look = chosen::<Appearance>(cx);
        let this = cx.entity().downgrade();
        let looks = Appearance::ALL
            .iter()
            .map(|(each, _, name)| Button::new(*name).label(*name).selected(*each == look));
        let appearance = ButtonGroup::new("appearance")
            .outline()
            .small()
            .children(looks)
            .on_click(move |clicked: &Vec<usize>, window, cx| {
                let Some((look, _, _)) = clicked.first().and_then(|&ix| Appearance::ALL.get(ix)) else {
                    return;
                };
                let _ = this.update(cx, |app, cx| app.choose_appearance(*look, window, cx));
            });
        let content = chosen::<NotificationContent>(cx);
        let this = cx.entity().downgrade();
        let contents = NotificationContent::ALL
            .iter()
            .enumerate()
            .map(|(ix, (_, _, name))| Radio::new(("content", ix)).label(*name));
        let notifications = RadioGroup::vertical("notification-content")
            .children(contents)
            .selected_index(
                NotificationContent::ALL
                    .iter()
                    .position(|(each, _, _)| *each == content),
            )
            .on_click(move |&ix, _, cx| {
                let Some((content, _, _)) = NotificationContent::ALL.get(ix) else {
                    return;
                };
                let _ = this.update(cx, |app, cx| app.choose(*content, cx));
            });
        let account = matches!(self.screen, Screen::Chats).then(|| self.render_account(s, p, cx));
        Some(modal(
            card(p)
                .id("settings")
                .role(Role::Dialog)
                .aria_label("Settings")
                .w(px(440.))
                // a short window scrolls it
                .max_h_full()
                .overflow_y_scroll()
                .on_key_down(cx.listener(|this, e: &KeyDownEvent, window, cx| {
                    if e.keystroke.key == "escape" {
                        this.close_settings(window, cx);
                        cx.stop_propagation();
                    }
                }))
                .child(
                    label("settings-title", Role::Heading, "Settings")
                        .aria_level(1)
                        .text_size(px(17.))
                        .font_weight(FontWeight::SEMIBOLD),
                )
                .child(row("Appearance", appearance))
                .child(row("Notification content", notifications).items_start())
                .children(account)
                .children(s.problem.clone().map(|why| {
                    label("settings-problem", Role::Alert, why)
                        .text_size(px(12.))
                        .text_color(p.danger)
                }))
                .child(
                    h_flex().child(div().flex_1()).child(
                        Button::new("settings-done")
                            .primary()
                            .label("Done")
                            .on_click(cx.listener(|this, _, window, cx| this.close_settings(window, cx))),
                    ),
                )
                .focus_trap("settings", &s.focus),
        ))
    }

    /// Our profile, the history transfer, the sync switch and unlinking.
    fn render_account(&self, s: &Panel, p: Palette, cx: &mut Context<Self>) -> Div {
        let working = self.settings.working;
        let this = cx.entity().downgrade();
        let sync = Checkbox::new("sync")
            .label("Sync chat settings to your phone")
            .checked(s.sync.unwrap_or(false))
            .disabled(s.sync.is_none())
            .on_click(move |on, _, cx| {
                let _ = this.update(cx, |app, cx| app.set_sync(*on, cx));
            });
        v_flex()
            .gap(px(12.))
            .child(divider(p))
            .child(self.render_profile(s, p, cx))
            .child(divider(p))
            .child(row(
                "Message history",
                Button::new("relink")
                    .label("Transfer Message History…")
                    .disabled(working)
                    .on_click(cx.listener(|this, _, window, cx| this.confirm_relink(window, cx))),
            ))
            .child(row("", sync))
            .child(row(
                "Linked device",
                h_flex()
                    .gap(px(8.))
                    .child(
                        Button::new("unlink")
                            .label("Unlink This Computer…")
                            .text_color(p.danger)
                            .disabled(working)
                            .on_click(cx.listener(|this, _, window, cx| this.confirm_unlink(window, cx))),
                    )
                    .when(working, |this| this.child(Spinner::new().xsmall())),
            ))
    }

    /// Our name and about (`ProfileSettings`): loading, its failure, or the fields.
    fn render_profile(&self, s: &Panel, p: Palette, cx: &mut Context<Self>) -> Div {
        let caption = |id, text: String, color| label(id, Role::Label, text).text_size(px(12.)).text_color(color);
        if s.saved.is_none() {
            let status = match &s.read_failure {
                None => Spinner::new().xsmall().into_any_element(),
                Some(_) => Button::new("profile-retry")
                    .label("Try Again")
                    .on_click(cx.listener(|this, _, window, cx| this.read_profile(window, cx)))
                    .into_any_element(),
            };
            return v_flex().gap(px(6.)).child(row("Profile", status)).children(
                s.read_failure
                    .clone()
                    .map(|why| caption("profile-read", why, p.secondary)),
            );
        }
        let input = |state: &Entity<InputState>, name: &'static str| {
            row(
                name,
                field(Input::new(state).appearance(false).aria_label(name).w_full(), p).flex_1(),
            )
        };
        v_flex()
            .gap(px(8.))
            .child(input(&s.given, "First name"))
            .child(input(&s.family, "Last name"))
            .child(input(&s.about, "About"))
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(
                        caption(
                            "profile-picture",
                            "Your picture is set on your phone.".into(),
                            p.secondary,
                        )
                        .flex_1(),
                    )
                    .when(s.saving, |this| this.child(Spinner::new().xsmall()))
                    .child(
                        Button::new("profile-save")
                            .label("Save Profile")
                            .disabled(!s.can_save(cx))
                            .on_click(cx.listener(|this, _, window, cx| this.save_profile(window, cx))),
                    ),
            )
            .children(
                s.save_failure
                    .clone()
                    .map(|why| caption("profile-save-failure", why, p.danger)),
            )
            .children(
                s.read_failure
                    .clone()
                    .map(|why| caption("profile-read", why, p.secondary)),
            )
    }

    /// The gear by the chat list's search (no menu bar shows the Hush menu off macOS).
    pub(in crate::app) fn render_settings_button(&self, p: Palette, cx: &mut Context<Self>) -> Button {
        Button::new("open-settings")
            .ghost()
            .icon(icon("settings", 15., p.secondary))
            .accessibility_label("Settings")
            .tooltip("Settings")
            .on_click(cx.listener(|this, _, window, cx| this.open_settings(window, cx)))
    }

    /// Removed on the phone (or the account is gone): linking again starts from nothing.
    pub(in crate::app) fn render_unlinked(&self, p: Palette, cx: &mut Context<Self>) -> Div {
        let detail = "It was removed from your phone's linked devices. Link it again to keep using Hush here. \
                      Messages stored on this computer are removed first.";
        v_flex().size_full().child(
            empty_state("smartphone", "This computer is no longer linked", detail, p)
                .children(self.error.clone().map(|e| {
                    label("unlinked-error", Role::Alert, e)
                        .max_w(px(360.))
                        .text_center()
                        .text_color(p.danger)
                }))
                .child(
                    div().pt(px(8.)).child(
                        Button::new("link-again")
                            .primary()
                            .label("Link Again")
                            .disabled(self.settings.working)
                            .on_click(cx.listener(|this, _, window, cx| this.unlink(false, window, cx))),
                    ),
                ),
        )
    }
}

/// A form row: its label on the left, right-aligned, and its control.
fn row(name: &'static str, control: impl IntoElement) -> Div {
    h_flex()
        .gap(px(12.))
        .child(div().w(px(LABELS)).flex_none().text_right().child(name))
        .child(control)
}

fn divider(p: Palette) -> Div {
    div().h(px(1.)).bg(p.separator)
}
