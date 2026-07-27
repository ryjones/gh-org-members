use anyhow::{Context, Result};
use serde_json::json;

use crate::client::GithubClient;
use crate::model::*;

const ENTERPRISE_ORGS: &str = r#"
query($slug: String!, $cursor: String, $batchSize: Int!) {
  enterprise(slug: $slug) {
    organizations(first: $batchSize, after: $cursor, orderBy: {field: LOGIN, direction: ASC}) {
      pageInfo { hasNextPage endCursor }
      nodes { login }
    }
  }
}
"#;

const ENTERPRISE_MEMBERS: &str = r#"
query($slug: String!, $cursor: String, $batchSize: Int!) {
  enterprise(slug: $slug) {
    members(first: $batchSize, after: $cursor, orderBy: {field: LOGIN, direction: ASC}) {
      pageInfo { hasNextPage endCursor }
      nodes {
        ... on EnterpriseUserAccount { login name user { login name email } }
        ... on User { login name email }
      }
    }
  }
}
"#;

const ENTERPRISE_ADMINS: &str = r#"
query($slug: String!, $cursor: String, $batchSize: Int!) {
  enterprise(slug: $slug) {
    ownerInfo {
      admins(first: $batchSize, after: $cursor, orderBy: {field: LOGIN, direction: ASC}) {
        pageInfo { hasNextPage endCursor }
        edges { role node { login name email } }
      }
    }
  }
}
"#;

const ORG_MEMBERS: &str = r#"
query($login: String!, $cursor: String, $batchSize: Int!) {
  organization(login: $login) {
    membersWithRole(first: $batchSize, after: $cursor) {
      pageInfo { hasNextPage endCursor }
      edges { role node { login name email } }
    }
  }
}
"#;

const ORG_TEAMS: &str = r#"
query($login: String!, $cursor: String, $membership: TeamMembershipType!, $batchSize: Int!, $teamBatchSize: Int!) {
  organization(login: $login) {
    teams(first: $teamBatchSize, after: $cursor, orderBy: {field: NAME, direction: ASC}) {
      pageInfo { hasNextPage endCursor }
      nodes {
        slug
        name
        members(first: $batchSize, membership: $membership) {
          pageInfo { hasNextPage endCursor }
          edges { role node { login name email } }
        }
      }
    }
  }
}
"#;

const TEAM_MEMBERS: &str = r#"
query($login: String!, $team: String!, $cursor: String, $membership: TeamMembershipType!, $batchSize: Int!) {
  organization(login: $login) {
    team(slug: $team) {
      members(first: $batchSize, after: $cursor, membership: $membership) {
        pageInfo { hasNextPage endCursor }
        edges { role node { login name email } }
      }
    }
  }
}
"#;

/// One person on the enterprise's own people list, independent of any org.
pub struct EnterpriseMember {
    pub actor: Actor,
    /// `OWNER` or `MEMBER`.
    pub role: String,
}

/// One person's membership in one team.
pub struct TeamMembership {
    pub slug: String,
    pub name: String,
    pub role: Option<String>,
}

/// Everything read out of a single organization.
pub struct OrgSnapshot {
    pub login: String,
    /// (actor, org role) for every org member visible to the token.
    pub members: Vec<(Actor, Option<String>)>,
    /// (member, team membership) pairs across all teams in the org.
    pub team_memberships: Vec<(Actor, TeamMembership)>,
    /// Teams seen, for the totals block.
    pub team_count: usize,
    /// Members discovered only via teams (also folded into `members` with no role).
    pub teams_readable: bool,
}

pub struct Collector<'a> {
    client: &'a GithubClient,
    membership: &'static str,
    skip_teams: bool,
    batch_size: u32,
}

impl<'a> Collector<'a> {
    pub fn new(
        client: &'a GithubClient,
        include_child_team_members: bool,
        skip_teams: bool,
        batch_size: u32,
    ) -> Self {
        Self {
            client,
            // IMMEDIATE = direct members only; ALL also returns members
            // inherited from child teams.
            membership: if include_child_team_members {
                "ALL"
            } else {
                "IMMEDIATE"
            },
            skip_teams,
            batch_size: batch_size.clamp(1, 100),
        }
    }

    /// Teams are fetched in smaller pages than members: each team node drags a
    /// nested member connection along, and the GraphQL cost model charges for
    /// the product.
    fn team_batch_size(&self) -> u32 {
        self.batch_size.div_ceil(5).clamp(1, 20)
    }

    /// Confirm the endpoint and token work before spending a long run on them,
    /// and prime the client's view of the rate-limit budget.
    pub async fn viewer_login(&self) -> Result<String> {
        let data: ViewerData = self
            .client
            .query("query { viewer { login } }", json!({}))
            .await
            .context("could not authenticate to the GraphQL endpoint")?;
        Ok(data.viewer.login)
    }

    /// All organizations in an enterprise, ordered by login.
    pub async fn enterprise_orgs(&self, slug: &str) -> Result<Vec<String>> {
        let mut cursor: Option<String> = None;
        let mut out = Vec::new();

        loop {
            let data: EnterpriseOrgsData = self
                .client
                .query(
                    ENTERPRISE_ORGS,
                    json!({ "slug": slug, "cursor": cursor, "batchSize": self.batch_size }),
                )
                .await
                .with_context(|| format!("listing organizations in enterprise `{slug}`"))?;

            let enterprise = data.enterprise.with_context(|| {
                format!("no enterprise named `{slug}` is visible to this token")
            })?;

            out.extend(
                enterprise
                    .organizations
                    .nodes
                    .into_iter()
                    .flatten()
                    .map(|n| n.login),
            );

            let page = enterprise.organizations.page_info;
            if !page.has_next_page {
                break;
            }
            cursor = page.end_cursor;
            if cursor.is_none() {
                break;
            }
        }

        Ok(out)
    }

    /// Everyone on the enterprise's people list, whether or not they belong to
    /// any of its organizations.
    ///
    /// This is what makes "in the enterprise but in no org" answerable: the org
    /// listings alone can only ever report people who are in an org.
    pub async fn enterprise_members(&self, slug: &str) -> Result<Vec<EnterpriseMember>> {
        let mut cursor: Option<String> = None;
        let mut out = Vec::new();

        loop {
            let data: EnterpriseMembersData = self
                .client
                .query(
                    ENTERPRISE_MEMBERS,
                    json!({ "slug": slug, "cursor": cursor, "batchSize": self.batch_size }),
                )
                .await
                .with_context(|| format!("listing members of enterprise `{slug}`"))?;

            let enterprise = data.enterprise.with_context(|| {
                format!("no enterprise named `{slug}` is visible to this token")
            })?;

            for node in enterprise.members.nodes.into_iter().flatten() {
                if let Some(actor) = enterprise_member_actor(node) {
                    out.push(EnterpriseMember {
                        actor,
                        role: "MEMBER".to_string(),
                    });
                }
            }

            let page = enterprise.members.page_info;
            if !page.has_next_page {
                break;
            }
            cursor = page.end_cursor;
            if cursor.is_none() {
                break;
            }
        }

        Ok(out)
    }

    /// The enterprise's owners. Readable only by an owner, and kept separate
    /// from `enterprise_members` because owners are not on the member list.
    pub async fn enterprise_admins(&self, slug: &str) -> Result<Vec<EnterpriseMember>> {
        let mut cursor: Option<String> = None;
        let mut out = Vec::new();

        loop {
            let data: EnterpriseAdminsData = self
                .client
                .query(
                    ENTERPRISE_ADMINS,
                    json!({ "slug": slug, "cursor": cursor, "batchSize": self.batch_size }),
                )
                .await
                .with_context(|| format!("listing owners of enterprise `{slug}`"))?;

            let owner_info = data
                .enterprise
                .and_then(|e| e.owner_info)
                .with_context(|| format!("this token is not an owner of enterprise `{slug}`"))?;

            for edge in owner_info.admins.edges.into_iter().flatten() {
                if let Some(actor) = edge.node {
                    out.push(EnterpriseMember {
                        actor,
                        role: edge.role.unwrap_or_else(|| "OWNER".to_string()),
                    });
                }
            }

            let page = owner_info.admins.page_info;
            if !page.has_next_page {
                break;
            }
            cursor = page.end_cursor;
            if cursor.is_none() {
                break;
            }
        }

        Ok(out)
    }

    /// Read members and team memberships for one organization.
    pub async fn org_snapshot(&self, login: &str) -> Result<OrgSnapshot> {
        let members = self.org_members(login).await?;
        let (team_memberships, team_count, teams_readable) = if self.skip_teams {
            (Vec::new(), 0, false)
        } else {
            match self.org_teams(login).await {
                Ok((memberships, count)) => (memberships, count, true),
                Err(err) => {
                    // Reading teams needs `read:org`; a token without it should
                    // still produce an org-member listing.
                    eprintln!("  warning: {login}: could not read teams: {err:#}");
                    (Vec::new(), 0, false)
                }
            }
        };

        Ok(OrgSnapshot {
            login: login.to_string(),
            members,
            team_memberships,
            team_count,
            teams_readable,
        })
    }

    async fn org_members(&self, login: &str) -> Result<Vec<(Actor, Option<String>)>> {
        let mut cursor: Option<String> = None;
        let mut out = Vec::new();

        loop {
            let data: OrgMembersData = self
                .client
                .query(
                    ORG_MEMBERS,
                    json!({ "login": login, "cursor": cursor, "batchSize": self.batch_size }),
                )
                .await
                .with_context(|| format!("listing members of `{login}`"))?;

            let org = data.organization.with_context(|| {
                format!("no organization named `{login}` is visible to this token")
            })?;

            let connection = org.members_with_role;
            for edge in connection.edges.into_iter().flatten() {
                if let Some(node) = edge.node {
                    out.push((node, edge.role));
                }
            }

            if !connection.page_info.has_next_page {
                break;
            }
            cursor = connection.page_info.end_cursor;
            if cursor.is_none() {
                break;
            }
        }

        Ok(out)
    }

    /// Returns (member, team membership) pairs plus the number of teams seen.
    async fn org_teams(&self, login: &str) -> Result<(Vec<(Actor, TeamMembership)>, usize)> {
        let mut cursor: Option<String> = None;
        let mut out: Vec<(Actor, TeamMembership)> = Vec::new();
        let mut team_count = 0usize;

        loop {
            let data: OrgTeamsData = self
                .client
                .query(
                    ORG_TEAMS,
                    json!({
                        "login": login,
                        "cursor": cursor,
                        "membership": self.membership,
                        "batchSize": self.batch_size,
                        "teamBatchSize": self.team_batch_size(),
                    }),
                )
                .await
                .with_context(|| format!("listing teams of `{login}`"))?;

            let org = data.organization.with_context(|| {
                format!("no organization named `{login}` is visible to this token")
            })?;

            let connection = org.teams;
            for team in connection.nodes.into_iter().flatten() {
                team_count += 1;
                let TeamNode {
                    slug,
                    name,
                    members,
                } = team;

                let mut page = members.page_info;
                collect_members(&mut out, &slug, &name, members.edges);

                // Teams with more than one page of members need a follow-up
                // query; the nested connection cannot be paged in place.
                while page.has_next_page {
                    let Some(next) = page.end_cursor else { break };
                    let data: TeamMembersData = self
                        .client
                        .query(
                            TEAM_MEMBERS,
                            json!({
                                "login": login,
                                "team": slug,
                                "cursor": next,
                                "membership": self.membership,
                                "batchSize": self.batch_size,
                            }),
                        )
                        .await
                        .with_context(|| format!("paging members of team `{login}/{slug}`"))?;

                    let Some(team) = data.organization.and_then(|o| o.team) else {
                        break;
                    };
                    page = team.members.page_info;
                    collect_members(&mut out, &slug, &name, team.members.edges);
                }
            }

            if !connection.page_info.has_next_page {
                break;
            }
            cursor = connection.page_info.end_cursor;
            if cursor.is_none() {
                break;
            }
        }

        Ok((out, team_count))
    }
}

/// Flatten one `enterprise.members` node into an `Actor`.
///
/// The two arms of the union carry the same person differently: an
/// `EnterpriseUserAccount` holds the enterprise-managed login and name and
/// hangs the profile (and its email) off `user`, while a `User` carries all
/// three directly. A node with neither login is skipped.
fn enterprise_member_actor(node: EnterpriseMemberNode) -> Option<Actor> {
    let EnterpriseMemberNode {
        login,
        name,
        email,
        user,
    } = node;
    let (user_login, user_name, user_email) = match user {
        Some(user) => (Some(user.login), user.name, user.email),
        None => (None, None, None),
    };
    Some(Actor {
        login: login.or(user_login)?,
        name: name.or(user_name),
        email: email.or(user_email),
    })
}

fn collect_members(
    out: &mut Vec<(Actor, TeamMembership)>,
    slug: &str,
    name: &str,
    edges: Vec<Option<MemberEdge>>,
) {
    for edge in edges.into_iter().flatten() {
        if let Some(node) = edge.node {
            out.push((
                node,
                TeamMembership {
                    slug: slug.to_string(),
                    name: name.to_string(),
                    role: edge.role,
                },
            ));
        }
    }
}
