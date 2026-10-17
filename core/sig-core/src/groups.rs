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
    /// Asked to join through the link (ACIs), waiting for an admin.
    pub requesting: Vec<String>,
    /// Who the invite link lets in.
    pub link_access: LinkAccess,
    /// The invite link, for members while it's on. It holds the group's key: never logged.
    pub link: Option<String>,
    /// We are a member (not after leaving or being removed).
    pub member: bool,
    pub admin: bool,
    /// We may change the title, description, picture and timer.
    pub can_edit_details: bool,
    pub can_add_members: bool,
}

/// Who a group's invite link lets in.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkAccess {
    Off,
    /// Anyone with the link joins.
    Anyone,
    /// Anyone with the link asks; an admin lets them in.
    Approval,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct GroupMember {
    pub aci: String,
    pub admin: bool,
}

/// A group as its invite link shows it to someone outside.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct GroupPreview {
    /// Its chat (`g:…`): it holds the group's key, so it is never logged.
    pub conversation_id: String,
    pub title: String,
    pub description: String,
    pub member_count: u32,
    /// Joining asks an admin to let us in.
    pub approval: bool,
    /// We asked already and wait for an admin.
    pub requested: bool,
    /// We're in it already.
    pub member: bool,
}

/// What a link shows of its group (`info`, as the server answers whoever has the link); an
/// error if the link lets no one in any more.
pub fn preview(
    info: &presage::libsignal_service::groups_v2::GroupJoinInfo,
    conversation_id: String,
) -> Result<GroupPreview, String> {
    let approval = match info.add_from_invite_link {
        AccessRequired::Any => false,
        AccessRequired::Administrator => true,
        _ => return Err("this group link has been turned off".into()),
    };
    Ok(GroupPreview {
        conversation_id,
        title: info.title.clone(),
        description: info.description_text.clone().unwrap_or_default(),
        member_count: info.member_count,
        approval,
        requested: info.pending_admin_approval,
        member: false,
    })
}

/// A link's group we're in already, as we have it: its chat opens, whatever the link says.
pub fn member_preview(group: &Group, conversation_id: String) -> GroupPreview {
    GroupPreview {
        conversation_id,
        title: group.title.clone(),
        description: group.description.clone().unwrap_or_default(),
        member_count: group.members.len() as u32,
        approval: false,
        requested: false,
        member: true,
    }
}

/// How joining through a link went.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct GroupJoined {
    pub conversation_id: String,
    /// We asked; an admin lets us in.
    pub requested: bool,
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
    /// Joins a group we're invited to.
    AcceptInvitation,
    DeclineInvitation,
    /// Turns the invite link on (a new one if there was none) or off.
    LinkAccess {
        access: LinkAccess,
    },
    /// A new invite link: the old one stops working.
    ResetLink,
    /// Lets in someone who asked to join.
    ApproveRequest {
        aci: String,
    },
    DenyRequest {
        aci: String,
    },
}

/// What `me` (an ACI) sees of `group` (whose master key is `key`) and may do in it, as its
/// access control says.
pub fn info(group: &Group, key: [u8; 32], me: &str) -> GroupInfo {
    let mine = group.members.iter().find(|m| m.aci.service_id_string() == me);
    let member = mine.is_some();
    let admin = mine.is_some_and(|m| m.role == Role::Administrator);
    // a group without access control has Signal's defaults: members may do both, no link
    let (attributes, members, link) = group.access_control.as_ref().map_or(
        (
            AccessRequired::Member,
            AccessRequired::Member,
            AccessRequired::Unsatisfiable,
        ),
        |a| (a.attributes, a.members, a.add_from_invite_link),
    );
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
        requesting: group
            .requesting_members
            .iter()
            .map(|r| r.aci.service_id_string())
            .collect(),
        link_access: match link {
            AccessRequired::Any => LinkAccess::Anyone,
            AccessRequired::Administrator => LinkAccess::Approval,
            _ => LinkAccess::Off,
        },
        link: group.invite_link(key).filter(|_| member).map(|l| l.to_url()),
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
        GroupChange::AcceptInvitation => GroupEdit::AcceptInvitation,
        GroupChange::DeclineInvitation => GroupEdit::DeclineInvitation,
        GroupChange::LinkAccess { access } => GroupEdit::InviteLinkAccess(match access {
            LinkAccess::Off => AccessRequired::Unsatisfiable,
            LinkAccess::Anyone => AccessRequired::Any,
            LinkAccess::Approval => AccessRequired::Administrator,
        }),
        GroupChange::ResetLink => GroupEdit::ResetInviteLink,
        GroupChange::ApproveRequest { aci: a } => GroupEdit::ApproveRequest(aci(&a)?),
        GroupChange::DenyRequest { aci: a } => GroupEdit::DenyRequest(aci(&a)?),
    })
}

/// Our invitation to `group` (to our ACI or our phone number's PNI), if we haven't answered it.
pub fn invitation(group: &Group, aci: Uuid, pni: Uuid) -> Option<&presage::model::groups::PendingMember> {
    group.pending_members.iter().find(|p| match p.service_id_type {
        presage::model::ServiceIdType::AccountIdentity => p.uuid == aci,
        presage::model::ServiceIdType::PhoneNumberIdentity => p.uuid == pni,
    })
}

pub fn invited(group: &Group, aci: Uuid, pni: Uuid) -> bool {
    invitation(group, aci, pni).is_some()
}

/// Where we (our ACI, our number's PNI) stand in `group`: an invitation outranks a request.
pub fn standing(group: &Group, aci: Uuid, pni: Uuid) -> crate::projection::GroupStanding {
    use crate::projection::GroupStanding;
    if invited(group, aci, pni) {
        GroupStanding::Invited
    } else if group
        .requesting_members
        .iter()
        .any(|r| ServiceId::from(r.aci).raw_uuid() == aci)
    {
        GroupStanding::Requesting
    } else {
        GroupStanding::Other
    }
}

/// We're one of `group`'s members.
pub fn is_member(group: &Group, aci: Uuid) -> bool {
    group.members.iter().any(|m| ServiceId::from(m.aci).raw_uuid() == aci)
}

/// A server error's text without the invite link's password, which its requests carry
/// (`/v1/groups/join/<password>`, `?inviteLinkPassword=<password>`): it may be logged.
pub fn without_link(error: String) -> String {
    let found = ["groups/join/", "inviteLinkPassword="]
        .iter()
        .filter_map(|marker| error.find(marker).map(|i| i + marker.len()))
        .min();
    let Some(start) = found else {
        return error;
    };
    let end = error[start..]
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '=' | '%')))
        .map_or(error.len(), |i| start + i);
    format!("{}…{}", &error[..start], without_link(error[end..].to_string()))
}

/// Our answer to an invitation shows in `group`: accepted, we're a member; declined, we're
/// neither a member nor invited.
pub fn answered(group: &Group, aci: Uuid, pni: Uuid, accept: bool) -> bool {
    let member = is_member(group, aci);
    if accept {
        member
    } else {
        !member && !invited(group, aci, pni)
    }
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
    const KEY: [u8; 32] = [5; 32];

    /// As the panel reads it, for the group whose key is `KEY`.
    fn info(group: &Group, me: &str) -> GroupInfo {
        super::info(group, KEY, me)
    }

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

    #[test]
    fn members_see_the_link_while_its_on_and_admins_the_requests() {
        use presage::model::groups::RequestingMember;
        let mut g = group(
            vec![member(ME, Role::Administrator)],
            Some((AccessRequired::Member, AccessRequired::Member)),
        );
        let i = info(&g, ME);
        assert_eq!((i.link_access, i.link.as_deref()), (LinkAccess::Off, None));
        g.invite_link_password = vec![1; 16];
        g.access_control.as_mut().unwrap().add_from_invite_link = AccessRequired::Administrator;
        g.requesting_members = vec![RequestingMember {
            aci: super::aci(ALICE).unwrap(),
            profile_key: ProfileKey::create([0; 32]),
            timestamp: 0,
        }];
        let i = info(&g, ME);
        assert_eq!(i.link_access, LinkAccess::Approval);
        let link = i.link.expect("on");
        let parsed = presage::libsignal_service::groups_v2::GroupInviteLink::parse(&link).unwrap();
        assert_eq!((parsed.master_key, parsed.password), (KEY, vec![1; 16]));
        assert_eq!(i.requesting, vec![ALICE.to_string()]);
        g.access_control.as_mut().unwrap().add_from_invite_link = AccessRequired::Any;
        assert_eq!(info(&g, ME).link_access, LinkAccess::Anyone);
        assert_eq!(info(&g, CAROL).link, None, "only members get the link");
        g.access_control.as_mut().unwrap().add_from_invite_link = AccessRequired::Unsatisfiable;
        let off = info(&g, ME);
        assert_eq!(
            (off.link_access, off.link),
            (LinkAccess::Off, None),
            "turned off, the password kept: no link"
        );
    }

    #[test]
    fn an_invitation_is_to_our_account_or_our_number() {
        let g = group(vec![member(ALICE, Role::Administrator)], None);
        let carol = Uuid::parse_str(CAROL).unwrap();
        let other = Uuid::parse_str(ALICE).unwrap();
        assert!(invited(&g, carol, other), "by account");
        assert!(invited(&g, other, carol), "by number");
        assert!(!invited(&g, other, other));
        assert_eq!(
            invitation(&g, carol, other).map(|p| p.added_by_aci),
            Some(super::aci(ALICE).unwrap()),
            "who invited us"
        );
    }

    #[test]
    fn an_answer_shows_in_the_group() {
        let carol = Uuid::parse_str(CAROL).unwrap();
        let nobody = Uuid::nil();
        let invited_still = group(vec![member(ALICE, Role::Administrator)], None);
        assert!(!answered(&invited_still, carol, nobody, true));
        assert!(!answered(&invited_still, carol, nobody, false));
        let mut joined = group(
            vec![member(ALICE, Role::Administrator), member(CAROL, Role::Default)],
            None,
        );
        joined.pending_members.clear();
        assert!(answered(&joined, carol, nobody, true));
        assert!(!answered(&joined, carol, nobody, false));
        joined.members.pop();
        assert!(
            answered(&joined, carol, nobody, false),
            "declined: neither in nor invited"
        );
    }

    #[test]
    fn invitation_and_link_changes_become_presages_edits() {
        let same =
            |change, edit: GroupEdit| assert_eq!(format!("{:?}", super::edit(change).unwrap()), format!("{edit:?}"));
        same(GroupChange::AcceptInvitation, GroupEdit::AcceptInvitation);
        same(GroupChange::DeclineInvitation, GroupEdit::DeclineInvitation);
        same(
            GroupChange::LinkAccess {
                access: LinkAccess::Off,
            },
            GroupEdit::InviteLinkAccess(AccessRequired::Unsatisfiable),
        );
        same(
            GroupChange::LinkAccess {
                access: LinkAccess::Anyone,
            },
            GroupEdit::InviteLinkAccess(AccessRequired::Any),
        );
        same(
            GroupChange::LinkAccess {
                access: LinkAccess::Approval,
            },
            GroupEdit::InviteLinkAccess(AccessRequired::Administrator),
        );
        same(GroupChange::ResetLink, GroupEdit::ResetInviteLink);
        same(
            GroupChange::ApproveRequest { aci: ALICE.into() },
            GroupEdit::ApproveRequest(super::aci(ALICE).unwrap()),
        );
        same(
            GroupChange::DenyRequest { aci: ALICE.into() },
            GroupEdit::DenyRequest(super::aci(ALICE).unwrap()),
        );
        assert!(edit(GroupChange::ApproveRequest { aci: "x".into() }).is_err());
    }

    #[test]
    fn a_link_shows_its_group_and_whether_joining_asks_an_admin() {
        use presage::libsignal_service::groups_v2::GroupJoinInfo;
        let mut info = GroupJoinInfo {
            title: "Hikers".into(),
            description_text: None,
            avatar: String::new(),
            member_count: 7,
            add_from_invite_link: AccessRequired::Any,
            version: 3,
            pending_admin_approval: false,
        };
        let open = preview(&info, "g:x".into()).unwrap();
        assert_eq!(
            open,
            GroupPreview {
                conversation_id: "g:x".into(),
                title: "Hikers".into(),
                description: String::new(),
                member_count: 7,
                approval: false,
                requested: false,
                member: false,
            }
        );
        info.add_from_invite_link = AccessRequired::Administrator;
        info.pending_admin_approval = true;
        let asked = preview(&info, "g:x".into()).unwrap();
        assert!(asked.approval && asked.requested);
        info.add_from_invite_link = AccessRequired::Unsatisfiable;
        assert!(preview(&info, "g:x".into()).is_err(), "turned off");
        let ours = member_preview(&group(vec![member(ME, Role::Default)], None), "g:x".into());
        assert!(ours.member && ours.member_count == 1 && ours.title == "Hikers");
    }

    #[test]
    fn where_we_stand_invited_asking_or_neither() {
        use crate::projection::GroupStanding;
        use presage::model::groups::RequestingMember;
        let carol = Uuid::parse_str(CAROL).unwrap();
        let me = Uuid::parse_str(ME).unwrap();
        let mut g = group(vec![member(ALICE, Role::Administrator)], None);
        assert_eq!(standing(&g, carol, Uuid::nil()), GroupStanding::Invited);
        g.requesting_members = vec![RequestingMember {
            aci: super::aci(ME).unwrap(),
            profile_key: ProfileKey::create([0; 32]),
            timestamp: 0,
        }];
        assert_eq!(standing(&g, me, Uuid::nil()), GroupStanding::Requesting);
        g.requesting_members[0].aci = super::aci(CAROL).unwrap();
        assert_eq!(standing(&g, carol, Uuid::nil()), GroupStanding::Invited, "invited wins");
        g.requesting_members.clear();
        g.members.push(member(ME, Role::Default));
        assert_eq!(standing(&g, me, Uuid::nil()), GroupStanding::Other, "let in");
        assert!(is_member(&g, me));
    }

    #[test]
    fn a_link_password_never_leaves_in_an_error() {
        assert_eq!(
            without_link("error sending request for url (https://x/v1/groups/join/Ab-c_9%3D): timed out".into()),
            "error sending request for url (https://x/v1/groups/join/…): timed out"
        );
        assert_eq!(without_link("refused".into()), "refused");
        assert_eq!(without_link("groups/join/abc".into()), "groups/join/…");
        assert_eq!(
            without_link("PATCH https://x/v1/groups/?inviteLinkPassword=Zm9v%3D&a=1 and /groups/join/b".into()),
            "PATCH https://x/v1/groups/?inviteLinkPassword=…&a=1 and /groups/join/…"
        );
    }
}
