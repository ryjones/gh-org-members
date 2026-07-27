use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// GraphQL response shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct PageInfo {
    #[serde(rename = "hasNextPage")]
    pub has_next_page: bool,
    #[serde(rename = "endCursor")]
    pub end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Actor {
    pub login: String,
    pub name: Option<String>,
    pub email: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MemberEdge {
    pub role: Option<String>,
    pub node: Option<Actor>,
}

#[derive(Debug, Deserialize)]
pub struct MemberConnection {
    #[serde(rename = "pageInfo")]
    pub page_info: PageInfo,
    #[serde(default)]
    pub edges: Vec<Option<MemberEdge>>,
}

#[derive(Debug, Deserialize)]
pub struct OrgNode {
    pub login: String,
}

#[derive(Debug, Deserialize)]
pub struct OrgConnection {
    #[serde(rename = "pageInfo")]
    pub page_info: PageInfo,
    #[serde(default)]
    pub nodes: Vec<Option<OrgNode>>,
}

#[derive(Debug, Deserialize)]
pub struct ViewerData {
    pub viewer: ViewerNode,
}

#[derive(Debug, Deserialize)]
pub struct ViewerNode {
    pub login: String,
}

#[derive(Debug, Deserialize)]
pub struct EnterpriseOrgsData {
    pub enterprise: Option<EnterpriseNode>,
}

#[derive(Debug, Deserialize)]
pub struct EnterpriseNode {
    pub organizations: OrgConnection,
}

#[derive(Debug, Deserialize)]
pub struct EnterpriseMembersData {
    pub enterprise: Option<EnterpriseMembersNode>,
}

#[derive(Debug, Deserialize)]
pub struct EnterpriseMembersNode {
    pub members: EnterpriseMemberConnection,
}

#[derive(Debug, Deserialize)]
pub struct EnterpriseMemberConnection {
    #[serde(rename = "pageInfo")]
    pub page_info: PageInfo,
    #[serde(default)]
    pub nodes: Vec<Option<EnterpriseMemberNode>>,
}

/// A node of `enterprise.members`, which is a union of `EnterpriseUserAccount`
/// and `User`. Every field is optional because which ones come back depends on
/// which arm matched.
#[derive(Debug, Deserialize)]
pub struct EnterpriseMemberNode {
    pub login: Option<String>,
    pub name: Option<String>,
    /// Only on the `User` arm.
    pub email: Option<String>,
    /// Only on the `EnterpriseUserAccount` arm, and null when the account has
    /// no user attached to it.
    pub user: Option<Actor>,
}

#[derive(Debug, Deserialize)]
pub struct EnterpriseAdminsData {
    pub enterprise: Option<EnterpriseAdminsNode>,
}

#[derive(Debug, Deserialize)]
pub struct EnterpriseAdminsNode {
    /// Null unless the token belongs to an owner of the enterprise.
    #[serde(rename = "ownerInfo")]
    pub owner_info: Option<EnterpriseOwnerInfoNode>,
}

#[derive(Debug, Deserialize)]
pub struct EnterpriseOwnerInfoNode {
    pub admins: MemberConnection,
}

#[derive(Debug, Deserialize)]
pub struct OrgMembersData {
    pub organization: Option<OrgMembersNode>,
}

#[derive(Debug, Deserialize)]
pub struct OrgMembersNode {
    #[serde(rename = "membersWithRole")]
    pub members_with_role: MemberConnection,
}

#[derive(Debug, Deserialize)]
pub struct TeamNode {
    pub slug: String,
    pub name: String,
    pub members: MemberConnection,
}

#[derive(Debug, Deserialize)]
pub struct TeamConnection {
    #[serde(rename = "pageInfo")]
    pub page_info: PageInfo,
    #[serde(default)]
    pub nodes: Vec<Option<TeamNode>>,
}

#[derive(Debug, Deserialize)]
pub struct OrgTeamsData {
    pub organization: Option<OrgTeamsNode>,
}

#[derive(Debug, Deserialize)]
pub struct OrgTeamsNode {
    pub teams: TeamConnection,
}

#[derive(Debug, Deserialize)]
pub struct TeamMembersData {
    pub organization: Option<TeamMembersOrgNode>,
}

#[derive(Debug, Deserialize)]
pub struct TeamMembersOrgNode {
    pub team: Option<TeamMembersNode>,
}

#[derive(Debug, Deserialize)]
pub struct TeamMembersNode {
    pub members: MemberConnection,
}

// ---------------------------------------------------------------------------
// YAML output shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct Report {
    pub source: Source,
    /// Org logins, ascending.
    pub organizations: Vec<String>,
    /// Orgs whose teams could not be read (token lacks `read:org` there), so
    /// their people appear with no team membership.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub organizations_without_team_data: Vec<String>,
    pub totals: Totals,
    /// People, ordered by login (case-insensitive, ascending).
    pub people: Vec<Person>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Source {
    pub api_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enterprise: Option<String>,
    /// True when team membership was expanded to include child-team members.
    pub include_child_team_members: bool,
    /// False when the run passed `--no-teams`, so an empty `teams` list means
    /// "not looked at" rather than "belongs to no team". Absent in exports
    /// written before this field existed, which always collected teams.
    #[serde(default = "yes")]
    pub teams: bool,
    /// True when the enterprise's own people list was read, so someone who
    /// belongs to the enterprise but to no organization still appears, with an
    /// empty `organizations` list. Absent when no enterprise was queried, or
    /// when the export predates enterprise-member collection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enterprise_members: Option<bool>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Totals {
    pub organizations: usize,
    pub people: usize,
    pub teams: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Person {
    pub login: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// `OWNER` or `MEMBER` at the enterprise level. Absent when the enterprise
    /// people list was not read, or when this person is not on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enterprise_role: Option<String>,
    /// Orgs this person belongs to, ordered by org login. Empty for someone who
    /// belongs to the enterprise but to none of its organizations.
    pub organizations: Vec<PersonOrg>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PersonOrg {
    pub org: String,
    /// `ADMIN` or `MEMBER`; absent when the person is visible only through a
    /// team (org-level role could not be read).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Teams within this org, ordered by team slug. Empty when the person is
    /// an org member with no team membership.
    pub teams: Vec<PersonTeam>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PersonTeam {
    pub slug: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `MEMBER` or `MAINTAINER`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}
