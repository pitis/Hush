//! Group management: what a group's info panel shows, and the changes the app can ask for.

use presage::libsignal_service::groups_v2::{AccessRequired, Role};
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::{Aci, ServiceId};
use presage::manager::GroupEdit;
use presage::model::groups::Group;

/// A group as its info panel shows it, beyond what its chat has (title, picture, timer).
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct GroupInfo {
    pub description: String,
    pub members: Vec<GroupMember>,
    /// Invited and not yet joined: ACIs, or `PNI:<uuid>` for an invited phone number.
    pub invited: Vec<String>,
    /// We are a member (not after leaving or being removed).
    pub member: bool,
    pub admin: bool,
    /// We may change the title, description, picture and timer.
    pub can_edit_details: bool,
    pub can_add_members: bool,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct GroupMember {
    pub aci: String,
    pub admin: bool,
}

/// A change to a group the app asks for.
#[derive(uniffi::Enum, Clone, Debug, PartialEq)]
pub enum GroupChange {
    Title {
        title: String,
    },
    /// Empty clears it.
    Description {
        description: String,
    },
    /// JPEG or PNG; empty clears it.
    Avatar {
        image: Vec<u8>,
    },
    /// Seconds; 0 turns disappearing messages off.
    Timer {
        seconds: u32,
    },
    /// Members we have a profile key for are added, the others invited.
    AddMembers {
        acis: Vec<String>,
    },
    RemoveMember {
        aci: String,
    },
    SetAdmin {
        aci: String,
        admin: bool,
    },
    Leave,
}

/// What `me` (an ACI) sees of `group` and may do in it, as its access control says.
pub fn info(group: &Group, me: &str) -> GroupInfo {
    let mine = group.members.iter().find(|m| m.aci.service_id_string() == me);
    let member = mine.is_some();
    let admin = mine.is_some_and(|m| m.role == Role::Administrator);
    // a group without access control has Signal's defaults: members may do both
    let (attributes, members) = group
        .access_control
        .as_ref()
        .map_or((AccessRequired::Member, AccessRequired::Member), |a| {
            (a.attributes, a.members)
        });
    let allowed = |required: AccessRequired| match required {
        AccessRequired::Any | AccessRequired::Member => member,
        AccessRequired::Administrator => admin,
        AccessRequired::Unknown | AccessRequired::Unsatisfiable => false,
    };
    GroupInfo {
        description: group.description.clone().unwrap_or_default(),
        members: group
            .members
            .iter()
            .map(|m| GroupMember {
                aci: m.aci.service_id_string(),
                admin: m.role == Role::Administrator,
            })
            .collect(),
        invited: group
            .pending_members
            .iter()
            .map(|p| match p.service_id_type {
                presage::model::ServiceIdType::PhoneNumberIdentity => format!("PNI:{}", p.uuid),
                presage::model::ServiceIdType::AccountIdentity => p.uuid.to_string(),
            })
            .collect(),
        member,
        admin,
        can_edit_details: allowed(attributes),
        can_add_members: allowed(members),
    }
}

/// presage's edit for a change; an error names what's wrong with it.
pub fn edit(change: GroupChange) -> Result<GroupEdit, String> {
    Ok(match change {
        GroupChange::Title { title } => {
            let title = title.trim().to_string();
            if title.is_empty() {
                return Err("a group needs a name".into());
            }
            GroupEdit::Title(title)
        }
        GroupChange::Description { description } => GroupEdit::Description(description.trim().to_string()),
        GroupChange::Avatar { image } => GroupEdit::Avatar(image),
        GroupChange::Timer { seconds } => GroupEdit::Timer(seconds),
        GroupChange::AddMembers { acis } => {
            if acis.is_empty() {
                return Err("no one to add".into());
            }
            GroupEdit::AddMembers(
                acis.iter()
                    .map(|a| aci(a).map(ServiceId::from))
                    .collect::<Result<_, _>>()?,
            )
        }
        GroupChange::RemoveMember { aci: a } => GroupEdit::RemoveMember(aci(&a)?),
        GroupChange::SetAdmin { aci: a, admin } => GroupEdit::SetAdmin(aci(&a)?, admin),
        GroupChange::Leave => GroupEdit::Leave,
    })
}

/// A new group's name (trimmed) and members; an error names what's wrong with them.
pub fn new_group(title: &str, acis: &[String]) -> Result<(String, Vec<Aci>), String> {
    let title = title.trim().to_string();
    if title.is_empty() {
        return Err("a group needs a name".into());
    }
    Ok((title, acis.iter().map(|a| aci(a)).collect::<Result<_, _>>()?))
}

fn aci(s: &str) -> Result<Aci, String> {
    Uuid::parse_str(s)
        .map(Aci::from)
        .map_err(|_| "not an account id".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::libsignal_service::prelude::ProfileKey;
    use presage::model::groups::AccessControl;
    use presage::model::groups::{Member, PendingMember};

    const ME: &str = "11111111-1111-1111-1111-111111111111";
    const ALICE: &str = "22222222-2222-2222-2222-222222222222";
    const CAROL: &str = "33333333-3333-3333-3333-333333333333";

    fn member(aci: &str, role: Role) -> Member {
        Member {
            aci: super::aci(aci).unwrap(),
            role,
            profile_key: ProfileKey::create([0; 32]),
            joined_at_revision: 0,
            label: None,
            label_emoji: None,
        }
    }

    fn group(members: Vec<Member>, access: Option<(AccessRequired, AccessRequired)>) -> Group {
        Group {
            title: "Hikers".into(),
            avatar: String::new(),
            disappearing_messages_timer: None,
            access_control: access.map(|(attributes, members)| AccessControl {
                attributes,
                members,
                add_from_invite_link: AccessRequired::Unsatisfiable,
                member_label: AccessRequired::Member,
            }),
            revision: 1,
            members,
            pending_members: vec![
                PendingMember {
                    uuid: Uuid::parse_str(CAROL).unwrap(),
                    service_id_type: presage::model::ServiceIdType::AccountIdentity,
                    role: Role::Default,
                    added_by_aci: super::aci(ALICE).unwrap(),
                    timestamp: 0,
                },
                PendingMember {
                    uuid: Uuid::parse_str(CAROL).unwrap(),
                    service_id_type: presage::model::ServiceIdType::PhoneNumberIdentity,
                    role: Role::Default,
                    added_by_aci: super::aci(ALICE).unwrap(),
                    timestamp: 0,
                },
            ],
            requesting_members: vec![],
            invite_link_password: vec![],
            description: Some("Sundays".into()),
        }
    }

    #[test]
    fn a_group_shows_its_members_admins_and_invitations() {
        let g = group(
            vec![member(ME, Role::Default), member(ALICE, Role::Administrator)],
            None,
        );
        let i = info(&g, ME);
        assert_eq!(i.description, "Sundays");
        assert_eq!(
            i.members,
            vec![
                GroupMember {
                    aci: ME.into(),
                    admin: false
                },
                GroupMember {
                    aci: ALICE.into(),
                    admin: true
                }
            ]
        );
        assert_eq!(i.invited, vec![CAROL.to_string(), format!("PNI:{CAROL}")]);
        assert!(i.member && !i.admin);
        assert!(i.can_edit_details && i.can_add_members, "Signal's defaults let members");
    }

    #[test]
    fn what_we_may_change_follows_the_groups_access_control() {
        let admins_only = Some((AccessRequired::Administrator, AccessRequired::Administrator));
        let as_member = info(
            &group(
                vec![member(ME, Role::Default), member(ALICE, Role::Administrator)],
                admins_only,
            ),
            ME,
        );
        assert!(!as_member.can_edit_details && !as_member.can_add_members);
        let as_admin = info(&group(vec![member(ME, Role::Administrator)], admins_only), ME);
        assert!(as_admin.admin && as_admin.can_edit_details && as_admin.can_add_members);
        let mixed = info(
            &group(
                vec![member(ME, Role::Default)],
                Some((AccessRequired::Member, AccessRequired::Administrator)),
            ),
            ME,
        );
        assert!(mixed.can_edit_details && !mixed.can_add_members);
    }

    #[test]
    fn anyone_means_members_here_and_no_one_means_no_one() {
        let me = vec![member(ME, Role::Administrator)];
        let any = info(&group(me.clone(), Some((AccessRequired::Any, AccessRequired::Any))), ME);
        assert!(any.can_edit_details && any.can_add_members);
        for none in [AccessRequired::Unsatisfiable, AccessRequired::Unknown] {
            let i = info(&group(me.clone(), Some((none, none))), ME);
            assert!(!i.can_edit_details && !i.can_add_members, "{none:?}, even for an admin");
        }
    }

    #[test]
    fn after_leaving_nothing_can_be_changed() {
        let left = info(&group(vec![member(ALICE, Role::Administrator)], None), ME);
        assert!(!left.member && !left.admin && !left.can_edit_details && !left.can_add_members);
    }

    #[test]
    fn changes_become_presages_edits() {
        assert_eq!(
            format!(
                "{:?}",
                edit(GroupChange::Title {
                    title: "  Climbers ".into()
                })
                .unwrap()
            ),
            format!("{:?}", GroupEdit::Title("Climbers".into()))
        );
        assert!(edit(GroupChange::Title { title: "  ".into() }).is_err());
        let same =
            |change, edit: GroupEdit| assert_eq!(format!("{:?}", super::edit(change).unwrap()), format!("{edit:?}"));
        same(
            GroupChange::Description {
                description: " Sundays\n".into(),
            },
            GroupEdit::Description("Sundays".into()),
        );
        same(
            GroupChange::Description { description: "".into() },
            GroupEdit::Description("".into()),
        );
        same(GroupChange::Timer { seconds: 3600 }, GroupEdit::Timer(3600));
        same(GroupChange::Avatar { image: vec![1, 2] }, GroupEdit::Avatar(vec![1, 2]));
        same(GroupChange::Leave, GroupEdit::Leave);
        assert!(edit(GroupChange::AddMembers { acis: vec![] }).is_err());
        assert!(edit(GroupChange::RemoveMember { aci: "nope".into() }).is_err());
        assert_eq!(
            format!(
                "{:?}",
                edit(GroupChange::SetAdmin {
                    aci: ALICE.into(),
                    admin: true
                })
                .unwrap()
            ),
            format!("{:?}", GroupEdit::SetAdmin(super::aci(ALICE).unwrap(), true))
        );
        assert_eq!(
            format!(
                "{:?}",
                edit(GroupChange::AddMembers {
                    acis: vec![ALICE.into()]
                })
                .unwrap()
            ),
            format!("{:?}", GroupEdit::AddMembers(vec![super::aci(ALICE).unwrap().into()]))
        );
    }

    #[test]
    fn a_new_group_needs_a_name_and_account_ids() {
        let (title, members) = new_group(" Climbers\n", &[ALICE.into()]).unwrap();
        assert_eq!(title, "Climbers");
        assert_eq!(members, vec![super::aci(ALICE).unwrap()]);
        assert!(new_group("Just me", &[]).is_ok(), "members can be added later");
        assert!(new_group("  ", &[ALICE.into()]).is_err());
        assert!(new_group("Climbers", &["PNI:x".into()]).is_err());
    }
}
