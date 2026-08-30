use crate::{
    auth::AuthenticatedUser,
    registry::types::{VersionStatus, Visibility},
};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum AccessRole {
    Reader,
    Publisher,
    OrgAdmin,
    ServerAdmin,
}

impl AccessRole {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Reader => "reader",
            Self::Publisher => "publisher",
            Self::OrgAdmin => "org_admin",
            Self::ServerAdmin => "server_admin",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PermissionDenied {
    pub(crate) required: AccessRole,
    pub(crate) actual: Option<AccessRole>,
}

pub(crate) fn require_role(
    user: &AuthenticatedUser,
    org: &str,
    minimum: AccessRole,
) -> Result<AccessRole, PermissionDenied> {
    match role_for(user, org) {
        Some(role) if role >= minimum => Ok(role),
        other => Err(PermissionDenied {
            required: minimum,
            actual: other,
        }),
    }
}

pub(crate) fn role_for(user: &AuthenticatedUser, org: &str) -> Option<AccessRole> {
    if user.is_server_admin {
        return Some(AccessRole::ServerAdmin);
    }
    user.orgs
        .iter()
        .find(|membership| membership.slug == org)
        .and_then(|membership| match membership.role.as_str() {
            "org_admin" => Some(AccessRole::OrgAdmin),
            "publisher" => Some(AccessRole::Publisher),
            "reader" => Some(AccessRole::Reader),
            _ => None,
        })
}

pub(crate) fn can_read_visibility(
    user: &AuthenticatedUser,
    role: AccessRole,
    visibility: Visibility,
    owner_user_id: Option<&str>,
    team_member: bool,
) -> bool {
    match visibility {
        Visibility::Org => role >= AccessRole::Reader,
        Visibility::Private => {
            role >= AccessRole::OrgAdmin || owner_user_id == Some(user.id.as_str())
        }
        Visibility::Team => {
            role >= AccessRole::OrgAdmin || team_member || owner_user_id == Some(user.id.as_str())
        }
    }
}

pub(crate) fn can_publish_visibility(
    role: AccessRole,
    visibility: Visibility,
    team_role: Option<&str>,
) -> bool {
    match visibility {
        Visibility::Private | Visibility::Org => role >= AccessRole::Publisher,
        Visibility::Team => {
            role >= AccessRole::OrgAdmin
                || (role >= AccessRole::Publisher && team_role.is_some())
                || is_team_admin_role(team_role)
        }
    }
}

pub(crate) fn can_read_version(
    user: &AuthenticatedUser,
    role: AccessRole,
    visibility: Visibility,
    owner_user_id: Option<&str>,
    team_role: Option<&str>,
    status: VersionStatus,
) -> bool {
    if !can_read_visibility(user, role, visibility, owner_user_id, team_role.is_some()) {
        return false;
    }
    match status {
        VersionStatus::Approved => true,
        VersionStatus::Candidate | VersionStatus::Rejected => {
            role >= AccessRole::Publisher
                || (visibility == Visibility::Team && is_team_admin_role(team_role))
        }
    }
}

pub(crate) fn is_team_admin_role(team_role: Option<&str>) -> bool {
    matches!(team_role, Some("team_admin" | "lead"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::OrgMembership;

    fn user(id: &str, is_server_admin: bool, orgs: Vec<OrgMembership>) -> AuthenticatedUser {
        AuthenticatedUser {
            id: id.to_string(),
            principal_id: format!("prn_{id}"),
            email: format!("{id}@example.com"),
            name: None,
            is_server_admin,
            orgs,
        }
    }

    fn membership(slug: &str, role: &str) -> OrgMembership {
        OrgMembership {
            slug: slug.to_string(),
            name: slug.to_string(),
            role: role.to_string(),
        }
    }

    #[test]
    fn role_for_resolves_server_admin_before_org_membership() {
        let user = user("admin", true, Vec::new());

        assert_eq!(role_for(&user, "demo"), Some(AccessRole::ServerAdmin));
        assert_eq!(
            require_role(&user, "demo", AccessRole::OrgAdmin),
            Ok(AccessRole::ServerAdmin)
        );
    }

    #[test]
    fn role_for_resolves_known_org_roles() {
        let org_admin = user("admin", false, vec![membership("demo", "org_admin")]);
        let publisher = user("publisher", false, vec![membership("demo", "publisher")]);
        let reader = user("reader", false, vec![membership("demo", "reader")]);

        assert_eq!(role_for(&org_admin, "demo"), Some(AccessRole::OrgAdmin));
        assert_eq!(role_for(&publisher, "demo"), Some(AccessRole::Publisher));
        assert_eq!(role_for(&reader, "demo"), Some(AccessRole::Reader));
    }

    #[test]
    fn role_for_ignores_unknown_org_role() {
        let user = user("unknown-role", false, vec![membership("demo", "writer")]);

        assert_eq!(role_for(&user, "demo"), None);
        assert_eq!(
            require_role(&user, "demo", AccessRole::Reader),
            Err(PermissionDenied {
                required: AccessRole::Reader,
                actual: None,
            })
        );
    }

    #[test]
    fn require_role_rejects_roles_below_minimum() {
        let user = user("reader", false, vec![membership("demo", "reader")]);

        assert_eq!(
            require_role(&user, "demo", AccessRole::Publisher),
            Err(PermissionDenied {
                required: AccessRole::Publisher,
                actual: Some(AccessRole::Reader),
            })
        );
    }

    #[test]
    fn private_visibility_allows_owner_and_admins() {
        let owner = user("owner", false, vec![membership("demo", "reader")]);
        let reader = user("reader", false, vec![membership("demo", "reader")]);
        let org_admin = user("org-admin", false, vec![membership("demo", "org_admin")]);
        let server_admin = user("server-admin", true, Vec::new());

        assert!(can_read_visibility(
            &owner,
            AccessRole::Reader,
            Visibility::Private,
            Some("owner"),
            false
        ));
        assert!(!can_read_visibility(
            &reader,
            AccessRole::Reader,
            Visibility::Private,
            Some("owner"),
            false
        ));
        assert!(can_read_visibility(
            &org_admin,
            AccessRole::OrgAdmin,
            Visibility::Private,
            Some("owner"),
            false
        ));
        assert!(can_read_visibility(
            &server_admin,
            AccessRole::ServerAdmin,
            Visibility::Private,
            Some("owner"),
            false
        ));
    }

    #[test]
    fn team_visibility_allows_team_members_and_admins() {
        let owner = user("owner", false, vec![membership("demo", "reader")]);
        let member = user("member", false, vec![membership("demo", "reader")]);
        let reader = user("reader", false, vec![membership("demo", "reader")]);
        let org_admin = user("org-admin", false, vec![membership("demo", "org_admin")]);

        assert!(can_read_visibility(
            &owner,
            AccessRole::Reader,
            Visibility::Team,
            Some("owner"),
            false
        ));
        assert!(can_read_visibility(
            &member,
            AccessRole::Reader,
            Visibility::Team,
            None,
            true
        ));
        assert!(!can_read_visibility(
            &reader,
            AccessRole::Reader,
            Visibility::Team,
            None,
            false
        ));
        assert!(can_read_visibility(
            &org_admin,
            AccessRole::OrgAdmin,
            Visibility::Team,
            None,
            false
        ));
    }

    #[test]
    fn team_publish_allows_team_admins_legacy_leads_and_team_publishers() {
        assert!(can_publish_visibility(
            AccessRole::Reader,
            Visibility::Team,
            Some("team_admin")
        ));
        assert!(can_publish_visibility(
            AccessRole::Reader,
            Visibility::Team,
            Some("lead")
        ));
        assert!(can_publish_visibility(
            AccessRole::Publisher,
            Visibility::Team,
            Some("member")
        ));
        assert!(!can_publish_visibility(
            AccessRole::Reader,
            Visibility::Team,
            Some("member")
        ));
        assert!(!can_publish_visibility(
            AccessRole::Publisher,
            Visibility::Team,
            None
        ));
    }

    #[test]
    fn reader_can_only_read_approved_versions() {
        let reader = user("reader", false, vec![membership("demo", "reader")]);

        assert!(can_read_version(
            &reader,
            AccessRole::Reader,
            Visibility::Org,
            None,
            None,
            VersionStatus::Approved
        ));
        assert!(!can_read_version(
            &reader,
            AccessRole::Reader,
            Visibility::Org,
            None,
            None,
            VersionStatus::Candidate
        ));
        assert!(!can_read_version(
            &reader,
            AccessRole::Reader,
            Visibility::Org,
            None,
            None,
            VersionStatus::Rejected
        ));
    }

    #[test]
    fn publisher_can_read_candidate_versions() {
        let publisher = user("publisher", false, vec![membership("demo", "publisher")]);

        assert!(can_read_version(
            &publisher,
            AccessRole::Publisher,
            Visibility::Org,
            None,
            None,
            VersionStatus::Candidate
        ));
    }

    #[test]
    fn team_admin_can_read_candidate_versions() {
        let team_admin = user("team-admin", false, vec![membership("demo", "reader")]);

        assert!(can_read_version(
            &team_admin,
            AccessRole::Reader,
            Visibility::Team,
            None,
            Some("team_admin"),
            VersionStatus::Candidate
        ));
        assert!(can_read_version(
            &team_admin,
            AccessRole::Reader,
            Visibility::Team,
            None,
            Some("lead"),
            VersionStatus::Candidate
        ));
        assert!(!can_read_version(
            &team_admin,
            AccessRole::Reader,
            Visibility::Team,
            None,
            Some("member"),
            VersionStatus::Candidate
        ));
    }
}
