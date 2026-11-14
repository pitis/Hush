//! How a poll is drawn in its bubble (`PollCard`): its question, each option with a bar of its
//! share of the voters, ours ticked, and what the votes add up to.

use std::collections::HashSet;
use std::sync::Arc;

use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::{summary, votes, Card};
use crate::app::files::disc;
use crate::app::view::icon;
use crate::app::Hush;

/// A poll's card, 220 to 320 wide. `color` is the bubble's words', `behind` its fill.
pub fn card(card: &Option<Card>, this: &WeakEntity<Hush>, color: Hsla, behind: Hsla) -> Option<Div> {
    let card = card.as_ref()?;
    let poll = &card.poll;
    // the bars' share: of everyone who voted, each once
    let all: HashSet<&String> = poll.options.iter().flat_map(|o| &o.voters).collect();
    let total = all.len().max(1) as f32;
    let options = poll.options.iter().enumerate().map(|(i, option)| {
        let chosen = card.chosen(i);
        let count = option.voters.len();
        let mark = if chosen {
            disc("check", 16., color, behind)
        } else {
            div().child(icon("circle", 16., color.opacity(0.7)))
        };
        let row = v_flex()
            .id(("poll-option", i))
            .role(Role::Button)
            .aria_label(format!(
                "{}, {}{}",
                option.text,
                votes(count),
                if chosen { ", your choice" } else { "" }
            ))
            .gap(px(3.))
            .child(
                h_flex()
                    .gap(px(6.))
                    .child(mark)
                    .child(div().flex_1().min_w_0().text_size(px(13.)).child(option.text.clone()))
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_features(FontFeatures(Arc::new(vec![("tnum".into(), 1)])))
                            .child(count.to_string()),
                    ),
            )
            .child(
                div().h(px(4.)).w_full().rounded_full().bg(color.opacity(0.2)).child(
                    div()
                        .h_full()
                        .rounded_full()
                        .bg(color.opacity(0.7))
                        .w(relative(count as f32 / total)),
                ),
            );
        if !card.votable() {
            return row;
        }
        let (this, key) = (this.clone(), card.key.clone());
        row.cursor_pointer().on_click(move |_, _, cx| {
            let _ = this.update(cx, |app, cx| app.vote(&key, i as u32, cx));
        })
    });
    let end = card.endable().then(|| {
        let (this, key) = (this.clone(), card.key.clone());
        div()
            .id("end-poll")
            .role(Role::Button)
            .aria_label("End poll")
            .px(px(8.))
            .py(px(2.))
            .rounded(px(6.))
            .border_1()
            .border_color(color.opacity(0.5))
            .text_size(px(11.))
            .cursor_pointer()
            .child("End poll")
            .on_click(move |_, _, cx| {
                let _ = this.update(cx, |app, cx| app.end_poll(&key, cx));
            })
    });
    let question = card.question.clone().unwrap_or_else(|| "Poll".into());
    Some(
        v_flex()
            .min_w(px(220.))
            .max_w(px(320.))
            .gap(px(8.))
            .child(
                h_flex()
                    .gap(px(6.))
                    .text_size(px(14.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(icon("chart-column", 14., color))
                    .child(div().min_w_0().child(question)),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(color.opacity(0.7))
                    .child(if poll.allow_multiple {
                        "Choose one or more"
                    } else {
                        "Choose one"
                    }),
            )
            .children(options)
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(11.))
                            .text_color(color.opacity(0.7))
                            .child(summary(poll)),
                    )
                    .children(end),
            ),
    )
}
