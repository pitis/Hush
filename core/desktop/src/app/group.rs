//! A group's info: its picture, name, description, timer and members, its link and the requests to
//! join it, and the changes its access control lets us make (the Mac app's `GroupInfo.swift`).

use std::collections::HashSet;

use async_compat::Compat;
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::*;
use sig_core::{GroupChange, GroupInfo, GroupMember};

use super::{describe, files, server_failure, Hush};
use crate::text;

pub(super) mod view;

/// Signal's apps send a group's picture at most this many pixels on its longer side.
const PICTURE_SIDE: u32 = 1024;

/// The open Group Info panel (the Mac app's sheet).
pub(super) struct Panel {
    /// The group's chat.
    id: String,
    /// None until read, or while the group isn't known yet.
    info: Option<GroupInfo>,
    loaded: bool,
    /// A read is running; another is wanted once it's in (the chat changed meanwhile).
    reading: bool,
    read_again: bool,
    /// Changes on their way to the server: the controls wait.
    busy: bool,
    failure: Option<String>,
    /// The link was copied.
    copied: bool,
    /// Tab and Shift-Tab stay inside it.
    focus: FocusHandle,
    /// The same for a sheet over it.
    sheet_focus: FocusHandle,
    sheet: Option<Sheet>,
}

/// A dialog over the panel.
enum Sheet {
    /// Edit Name and Description….
    Details {
        name: Entity<InputState>,
        description: Entity<TextareaState>,
    },
    /// Add Members…: the people ticked.
    Add {
        search: Entity<InputState>,
        chosen: HashSet<String>,
    },
}

impl Hush {
    /// Chat Options › Group Info…: the panel over the chat, read from the core.
    pub(super) fn open_group_info(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.dialog.is_some() {
            return;
        }
        let focus = cx.focus_handle();
        // the composer behind doesn't keep the keys
        focus.focus(window, cx);
        self.group = Some(Panel {
            id,
            info: None,
            loaded: false,
            reading: false,
            read_again: false,
            busy: false,
            failure: None,
            copied: false,
            focus,
            sheet_focus: cx.focus_handle(),
            sheet: None,
        });
        self.read_group(cx);
        cx.notify();
    }

    /// Done or Esc: back to the chat's composer. Changes on their way still land.
    pub(super) fn close_group_info(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.group.take().is_some() {
            self.composer.update(cx, |input, cx| input.focus(window, cx));
            cx.notify();
        }
    }

    /// Reads the open panel's group again; calls while a read runs are folded into one more. A
    /// failed read keeps what shows and says why.
    pub(super) fn read_group(&mut self, cx: &mut Context<Self>) {
        let (Some(client), Some(g)) = (self.client.clone(), self.group.as_mut()) else {
            return;
        };
        if g.reading {
            g.read_again = true;
            return;
        }
        g.reading = true;
        let id = g.id.clone();
        cx.spawn(async move |this, cx| loop {
            let read = Compat::new(client.group_info(id.clone())).await;
            let again = this.update(cx, |app, cx| {
                let me = app.own_aci.clone();
                let Some(g) = app.group.as_mut().filter(|g| g.id == id) else {
                    return false;
                };
                let mut people = vec![];
                match read {
                    Ok(info) => {
                        if let Some(i) = &info {
                            people.extend(i.members.iter().map(|m| m.aci.clone()));
                            people.extend(i.invited.iter().chain(&i.requesting).cloned());
                        }
                        g.info = info;
                    }
                    Err(e) => g.failure = Some(format!("Couldn't read the group: {}", describe(&e))),
                }
                g.loaded = true;
                g.reading = std::mem::take(&mut g.read_again);
                let again = g.reading;
                people.retain(|aci| Some(aci) != me.as_ref());
                app.load_names(people, cx);
                cx.notify();
                again
            });
            if !again.unwrap_or(false) {
                break;
            }
        })
        .detach();
    }

    /// Makes the changes to the open panel's group one after the other (the core updates the chat
    /// as each lands); the first failure shows, in the panel or, once it's closed, over the chats.
    pub(super) fn change_group(&mut self, changes: Vec<GroupChange>, cx: &mut Context<Self>) {
        let (Some(client), Some(g)) = (self.client.clone(), self.group.as_mut()) else {
            return;
        };
        if changes.is_empty() || g.busy {
            return;
        }
        g.busy = true;
        g.failure = None;
        let id = g.id.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let mut failure = None;
            for change in changes {
                if let Err(e) = Compat::new(client.change_group(id.clone(), change)).await {
                    failure.get_or_insert_with(|| server_failure("Couldn't change the group", &e));
                }
            }
            let _ = this.update(cx, |app, cx| {
                match app.group.as_mut().filter(|g| g.id == id) {
                    Some(g) => {
                        g.busy = false;
                        g.failure = failure;
                    }
                    None => app.error = failure.or(app.error.take()),
                }
                app.read_group(cx);
                // the mention picker offers whoever is in it now
                if app.selected.as_deref() == Some(id.as_str()) {
                    app.load_members(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Asks first, as the Mac app's confirmations, then makes the change.
    fn confirm_group_change(
        &mut self,
        (title, detail, button): (&str, Option<&str>, &str),
        change: GroupChange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let answer = window.prompt(PromptLevel::Warning, title, detail, &[button, "Cancel"], cx);
        cx.spawn(async move |this, cx| {
            if answer.await == Ok(0) {
                let _ = this.update(cx, |app, cx| app.change_group(vec![change], cx));
            }
        })
        .detach();
    }

    pub(super) fn confirm_leave(&mut self, title: &str, window: &mut Window, cx: &mut Context<Self>) {
        let asked = format!("Leave {title}?");
        let detail = "You won't get its messages any more. The chat stays here.";
        let says = (asked.as_str(), Some(detail), "Leave Group");
        self.confirm_group_change(says, GroupChange::Leave, window, cx);
    }

    pub(super) fn confirm_remove(&mut self, aci: String, window: &mut Window, cx: &mut Context<Self>) {
        let asked = format!("Remove {}?", self.name(&aci));
        let says = (asked.as_str(), None, "Remove from Group");
        self.confirm_group_change(says, GroupChange::RemoveMember { aci }, window, cx);
    }

    pub(super) fn confirm_reset_link(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let says = (
            "Reset the group link?",
            Some("The link given out so far stops working."),
            "Reset Link",
        );
        self.confirm_group_change(says, GroupChange::ResetLink, window, cx);
    }

    /// Copy Link: the link holds the group's key, so it is copied, never shown or logged.
    pub(super) fn copy_group_link(&mut self, cx: &mut Context<Self>) {
        let Some(g) = &mut self.group else { return };
        if let Some(link) = g.info.as_ref().and_then(|i| i.link.clone()) {
            cx.write_to_clipboard(ClipboardItem::new_string(link));
            g.copied = true;
            cx.notify();
        }
    }

    /// Picture › Choose…: a picture file, sent as Signal's apps send one.
    pub(super) fn choose_group_picture(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            let path = match chosen.await {
                Ok(Ok(Some(mut paths))) if !paths.is_empty() => paths.swap_remove(0),
                Ok(Err(e)) => {
                    let _ = this.update(cx, |app, cx| {
                        app.group_failed(format!("Couldn't choose a picture: {e}"), cx)
                    });
                    return;
                }
                // cancelled
                _ => return,
            };
            let jpeg = cx
                .background_spawn(async move { std::fs::read(path).ok().and_then(|b| group_picture(&b).ok()) })
                .await;
            let _ = this.update(cx, |app, cx| match jpeg {
                Some(image) => app.change_group(vec![GroupChange::Avatar { image }], cx),
                None => app.group_failed("That file isn't a picture Hush can read.".into(), cx),
            });
        })
        .detach();
    }

    fn group_failed(&mut self, why: String, cx: &mut Context<Self>) {
        if let Some(g) = &mut self.group {
            g.failure = Some(why);
            cx.notify();
        }
    }

    /// Edit Name and Description…, filled in with them.
    pub(super) fn edit_group_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(g) = &self.group else { return };
        let title = self.chats.iter().find(|c| c.id == g.id).map(|c| c.title.clone());
        let described = g.info.as_ref().map(|i| i.description.clone()).unwrap_or_default();
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Group name")
                .default_value(title.unwrap_or_default())
        });
        cx.subscribe_in(&name, window, |this, _, event: &InputEvent, window, cx| match event {
            InputEvent::PressEnter { .. } => this.save_sheet(window, cx),
            // Save waits for a name
            InputEvent::Change => cx.notify(),
            _ => {}
        })
        .detach();
        let description = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 6)
                .placeholder("Description")
                .default_value(described)
        });
        name.update(cx, |n, cx| n.focus(window, cx));
        self.open_sheet(Sheet::Details { name, description }, cx);
    }

    /// Add Members…: whom to add, from the people we chat with.
    pub(super) fn add_group_members(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        cx.subscribe_in(&search, window, |this, _, event: &InputEvent, window, cx| match event {
            InputEvent::PressEnter { .. } => this.save_sheet(window, cx),
            InputEvent::Change => cx.notify(),
            _ => {}
        })
        .detach();
        search.update(cx, |s, cx| s.focus(window, cx));
        let chosen = HashSet::new();
        self.open_sheet(Sheet::Add { search, chosen }, cx);
    }

    fn open_sheet(&mut self, sheet: Sheet, cx: &mut Context<Self>) {
        if let Some(g) = self.group.as_mut().filter(|g| !g.busy) {
            g.sheet = Some(sheet);
            cx.notify();
        }
    }

    /// Cancel or Esc: back to the panel.
    pub(super) fn close_sheet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(g) = &mut self.group {
            g.sheet = None;
            g.focus.focus(window, cx);
            cx.notify();
        }
    }

    pub(super) fn toggle_new_member(&mut self, aci: String, cx: &mut Context<Self>) {
        if let Some(Sheet::Add { chosen, .. }) = self.group.as_mut().and_then(|g| g.sheet.as_mut()) {
            if !chosen.remove(&aci) {
                chosen.insert(aci);
            }
            cx.notify();
        }
    }

    /// Who Add Members… offers: the people we chat with who aren't members yet, by name.
    fn candidates(&self) -> Vec<(String, String)> {
        let info = self.group.as_ref().and_then(|g| g.info.as_ref());
        let members: HashSet<&str> = info.iter().flat_map(|i| &i.members).map(|m| m.aci.as_str()).collect();
        let addable = self.contacts.iter().filter(|(aci, _)| !members.contains(aci.as_str()));
        addable.cloned().collect()
    }

    /// The sheet's Save or Add: its changes, made from the panel.
    pub(super) fn save_sheet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(g) = &self.group else { return };
        let changes = match &g.sheet {
            Some(Sheet::Details { name, description }) => {
                let title = name.read(cx).value().to_string();
                if title.trim().is_empty() {
                    return;
                }
                let old_title = self.chats.iter().find(|c| c.id == g.id).map(|c| c.title.as_str());
                let old_description = g.info.as_ref().map(|i| i.description.as_str());
                detail_changes(
                    (old_title.unwrap_or_default(), old_description.unwrap_or_default()),
                    (&title, &description.read(cx).value()),
                )
            }
            Some(Sheet::Add { chosen, .. }) if !chosen.is_empty() => {
                let candidates = self.candidates();
                let acis = candidates
                    .into_iter()
                    .map(|(aci, _)| aci)
                    .filter(|a| chosen.contains(a));
                vec![GroupChange::AddMembers { acis: acis.collect() }]
            }
            Some(Sheet::Add { .. }) | None => return,
        };
        self.close_sheet(window, cx);
        self.change_group(changes, cx);
    }
}

/// What saving the name and description changes: each that differs from what it was, trimmed.
fn detail_changes(was: (&str, &str), now: (&str, &str)) -> Vec<GroupChange> {
    let (title, description) = (now.0.trim(), now.1.trim());
    let mut changes = vec![];
    if title != was.0.trim() {
        changes.push(GroupChange::Title { title: title.into() });
    }
    if description != was.1.trim() {
        changes.push(GroupChange::Description {
            description: description.into(),
        });
    }
    changes
}

/// Us first, then admins, then by name (`GroupInfoView.sorted`).
fn sorted<'a>(members: &'a [GroupMember], me: Option<&str>, name: impl Fn(&str) -> String) -> Vec<&'a GroupMember> {
    let mut sorted: Vec<&GroupMember> = members.iter().collect();
    sorted.sort_by_cached_key(|m| (Some(m.aci.as_str()) != me, !m.admin, name(&m.aci).to_lowercase()));
    sorted
}

/// The timers offered, as in Desktop (seconds; 0: off), with `current` among them when the phone
/// set another (`Placeholder.timerChoices`).
fn timer_choices(current: u32) -> Vec<u32> {
    let mut choices = vec![0, 30, 300, 3_600, 28_800, 86_400, 604_800, 2_419_200];
    if !choices.contains(&current) {
        choices.push(current);
        choices.sort();
    }
    choices
}

fn timer_choice(seconds: u32) -> String {
    match seconds {
        0 => "Off".into(),
        s => text::duration(s),
    }
}

/// A group picture as Signal's apps send one: a JPEG at most `PICTURE_SIDE` on its longer side,
/// upright, transparency on white (`GroupPicture.jpeg`); or why not.
/// shortcut: its colours are taken as sRGB (a colour profile isn't applied, as the Mac app does);
/// convert with the profile if a wide-gamut photo looks dull on the phones.
fn group_picture(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let picture = files::upright(bytes, PICTURE_SIDE)?.into_rgba8();
    let on_white = |c: u8, a: u8| ((u32::from(c) * u32::from(a) + 255 * (255 - u32::from(a))) / 255) as u8;
    let flat = image::RgbImage::from_fn(picture.width(), picture.height(), |x, y| {
        let [r, g, b, a] = picture.get_pixel(x, y).0;
        image::Rgb([on_white(r, a), on_white(g, a), on_white(b, a)])
    });
    let mut jpeg = vec![];
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 85)
        .encode_image(&flat)
        .map_err(|e| e.to_string())?;
    Ok(jpeg)
}

#[cfg(feature = "snapshot")]
impl Hush {
    /// `--snapshot`'s Group Info: an admin's view of the Family group, with `sheet` over it.
    pub(super) fn demo_group(&mut self, sheet: Option<bool>, window: &mut Window, cx: &mut Context<Self>) {
        self.open_group_info("g:01".into(), window, cx);
        if let Some(g) = &mut self.group {
            g.info = Some(crate::demo::group_info());
            g.loaded = true;
        }
        self.names.extend(crate::demo::group_names());
        match sheet {
            Some(true) => self.edit_group_details(window, cx),
            Some(false) => {
                self.add_group_members(window, cx);
                let first = self.candidates().first().map(|(aci, _)| aci.clone());
                first.into_iter().for_each(|aci| self.toggle_new_member(aci, cx));
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{detail_changes, group_picture, sorted, timer_choices, PICTURE_SIDE};
    use sig_core::{GroupChange, GroupMember};

    #[test]
    fn saving_changes_only_what_differs_trimmed() {
        assert!(detail_changes(("Hikers", "Sundays"), (" Hikers ", "Sundays\n")).is_empty());
        assert_eq!(
            detail_changes(("Hikers", ""), ("Hikers", " Sundays at 9 ")),
            [GroupChange::Description {
                description: "Sundays at 9".into()
            }]
        );
        // a cleared description is sent empty
        assert_eq!(
            detail_changes(("Hikers", "Sundays"), ("Walkers", "")),
            [
                GroupChange::Title {
                    title: "Walkers".into()
                },
                GroupChange::Description {
                    description: String::new()
                }
            ]
        );
    }

    #[test]
    fn members_show_us_first_then_admins_then_by_name() {
        let member = |aci: &str, admin| GroupMember { aci: aci.into(), admin };
        let members = [
            member("z", false),
            member("b", true),
            member("me", false),
            member("a", false),
            member("c", true),
        ];
        let name = |aci: &str| match aci {
            "z" => "ana".to_string(),
            "a" => "Bogdan".into(),
            "b" => "Zoë".into(),
            "c" => "Carmen".into(),
            _ => "You".into(),
        };
        let order: Vec<&str> = sorted(&members, Some("me"), name)
            .iter()
            .map(|m| m.aci.as_str())
            .collect();
        assert_eq!(order, ["me", "c", "b", "z", "a"]);
    }

    #[test]
    fn the_timers_offered_include_the_one_set() {
        let usual = [0, 30, 300, 3_600, 28_800, 86_400, 604_800, 2_419_200];
        assert_eq!(timer_choices(0), usual);
        assert_eq!(timer_choices(3_600), usual);
        // two days, set on the phone
        assert_eq!(
            timer_choices(172_800),
            [0, 30, 300, 3_600, 28_800, 86_400, 172_800, 604_800, 2_419_200]
        );
    }

    #[test]
    fn a_group_picture_is_a_jpeg_at_most_1024_wide_on_white() {
        // a wide PNG, transparent but for a red right half
        let mut png = image::RgbaImage::new(2048, 1024);
        for (x, _, pixel) in png.enumerate_pixels_mut() {
            *pixel = if x >= 1024 {
                image::Rgba([255, 0, 0, 255])
            } else {
                image::Rgba([0, 0, 0, 0])
            };
        }
        let mut bytes = std::io::Cursor::new(vec![]);
        png.write_to(&mut bytes, image::ImageFormat::Png).unwrap();

        let jpeg = group_picture(bytes.get_ref()).expect("a PNG converts");
        assert_eq!(image::guess_format(&jpeg).unwrap(), image::ImageFormat::Jpeg);
        let sent = image::load_from_memory(&jpeg).unwrap().into_rgb8();
        assert_eq!((sent.width(), sent.height()), (PICTURE_SIDE, PICTURE_SIDE / 2));
        let [r, g, b] = sent.get_pixel(10, 10).0;
        assert!(r > 245 && g > 245 && b > 245, "transparency is white: {r} {g} {b}");
        let [r, g, b] = sent.get_pixel(1000, 10).0;
        assert!(r > 230 && g < 30 && b < 30, "the colours stay: {r} {g} {b}");

        assert!(group_picture(b"not a picture").is_err());
    }
}

#[cfg(all(test, feature = "snapshot"))]
mod window_tests {
    use gpui_kit::{gpui, AnyWindowHandle, AppContext as _, Entity, Focusable as _, TestAppContext, WindowOptions};

    use super::super::{navigate, Hush};
    use crate::demo::BOGDAN;
    use crate::Demo;

    fn open(screen: Demo, cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Hush>) {
        let opened = cx.update(|cx| {
            gpui_kit::init(cx);
            cx.bind_keys(crate::key_bindings());
            let (window, hush) = gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Hush::demo(screen, window, cx))
            })
            .expect("the window opens");
            navigate::shortcuts(window, hush.downgrade(), cx);
            (window, hush)
        });
        cx.run_until_parked();
        opened
    }

    #[gpui::test]
    fn the_panel_keeps_the_keys_until_esc_closes_its_sheet_then_it(cx: &mut TestAppContext) {
        let (window, hush) = open(Demo::AddMembers, cx);
        // whoever isn't a member yet: Bogdan, who asked to join, and not Ana
        let offered: Vec<String> =
            hush.read_with(cx, |app, _| app.candidates().into_iter().map(|(aci, _)| aci).collect());
        assert_eq!(offered, [BOGDAN]);

        // the chats behind wait
        cx.simulate_keystrokes(window, "alt-down secondary-2 secondary-shift-c");
        assert_eq!(
            hush.read_with(cx, |app, _| app.selected.clone()).as_deref(),
            Some("g:01")
        );

        cx.simulate_keystrokes(window, "escape");
        let (panel, sheet) = hush.read_with(cx, |app, _| {
            (
                app.group.is_some(),
                app.group.as_ref().is_some_and(|g| g.sheet.is_some()),
            )
        });
        assert!(panel && !sheet, "the sheet closes, the panel stays");

        cx.simulate_keystrokes(window, "escape");
        assert!(hush.read_with(cx, |app, _| app.group.is_none()));
        let composer = window
            .update(cx, |_, window, cx| {
                hush.read(cx).composer.focus_handle(cx).is_focused(window)
            })
            .unwrap();
        assert!(composer, "the keys go back to the chat");
        cx.simulate_keystrokes(window, "alt-down");
        assert_ne!(
            hush.read_with(cx, |app, _| app.selected.clone()).as_deref(),
            Some("g:01")
        );
    }
}
