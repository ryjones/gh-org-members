//! Two gap reports derived from a `gh-org-members` export.
//!
//! Both answer the same shape of question — who is inside one boundary but not
//! the next one in — and both are pure functions of an export, so they cost no
//! API quota and can be re-run over a captured file.

use serde::Serialize;

use crate::model::{Person, Report};

/// A person who belongs to at least one organization in which they hold no team
/// membership. Only those organizations are listed: someone on a team in one org
/// and on none in another appears here for the second org alone.
#[derive(Debug, Serialize)]
pub struct PersonWithoutTeams {
    pub login: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Ordered by org login, as in the export.
    pub organizations: Vec<OrgWithoutTeams>,
}

#[derive(Debug, Serialize)]
pub struct OrgWithoutTeams {
    pub org: String,
    /// `ADMIN` or `MEMBER`; absent when the org-level role could not be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

/// A person on the enterprise's people list who belongs to none of the
/// organizations in the export.
#[derive(Debug, Serialize)]
pub struct PersonWithoutOrg {
    pub login: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// `OWNER` or `MEMBER` at the enterprise level.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enterprise_role: Option<String>,
}

/// Org members who are on no team, ordered as the export orders people.
///
/// Organizations whose teams could not be read are skipped: there, "no teams"
/// and "teams unknown" look identical, and reporting them would be a guess.
/// Their logins are returned alongside so a caller can say what was left out.
///
/// With `exclude_admins`, someone who administers the thing they are being
/// reported against is left out — an enterprise `OWNER` entirely, and an
/// organization `ADMIN` for that organization. They have their access from the
/// role rather than from a team, so being on no team says nothing about them.
pub fn org_members_without_teams(
    report: &Report,
    exclude_admins: bool,
) -> (Vec<PersonWithoutTeams>, Vec<String>) {
    let skipped = &report.organizations_without_team_data;

    let people = report
        .people
        .iter()
        .filter_map(|person| {
            if exclude_admins && person.enterprise_role.as_deref() == Some("OWNER") {
                return None;
            }
            let organizations: Vec<OrgWithoutTeams> = person
                .organizations
                .iter()
                .filter(|org| org.teams.is_empty() && !skipped.contains(&org.org))
                .filter(|org| !(exclude_admins && org.role.as_deref() == Some("ADMIN")))
                .map(|org| OrgWithoutTeams {
                    org: org.org.clone(),
                    role: org.role.clone(),
                })
                .collect();
            if organizations.is_empty() {
                return None;
            }
            Some(PersonWithoutTeams {
                login: person.login.clone(),
                name: person.name.clone(),
                email: person.email.clone(),
                organizations,
            })
        })
        .collect();

    (people, skipped.clone())
}

/// Enterprise members who belong to none of the organizations in the export.
///
/// Only meaningful when the export recorded the enterprise's people list; see
/// [`enterprise_membership_available`].
pub fn enterprise_members_without_org(report: &Report) -> Vec<PersonWithoutOrg> {
    report
        .people
        .iter()
        .filter(|person| person.organizations.is_empty())
        .map(|person: &Person| PersonWithoutOrg {
            login: person.login.clone(),
            name: person.name.clone(),
            email: person.email.clone(),
            enterprise_role: person.enterprise_role.clone(),
        })
        .collect()
}

/// Whether the export read the enterprise's own people list. Without it every
/// person came from an org listing, so "belongs to no org" is empty by
/// construction rather than by fact.
pub fn enterprise_membership_available(report: &Report) -> bool {
    report.source.enterprise_members == Some(true)
}

/// Whether the export collected teams at all. A `--no-teams` run leaves every
/// `teams` list empty, which would otherwise read as "nobody is on a team".
pub fn team_membership_available(report: &Report) -> bool {
    report.source.teams
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Person, PersonOrg, PersonTeam, Source, Totals};

    fn person(login: &str, orgs: Vec<PersonOrg>) -> Person {
        Person {
            login: login.to_string(),
            name: Some(format!("{login} display")),
            email: None,
            enterprise_role: None,
            organizations: orgs,
        }
    }

    fn admin_of(login: &str) -> PersonOrg {
        PersonOrg {
            role: Some("ADMIN".to_string()),
            ..org(login, &[])
        }
    }

    fn org(login: &str, teams: &[&str]) -> PersonOrg {
        PersonOrg {
            org: login.to_string(),
            role: Some("MEMBER".to_string()),
            teams: teams
                .iter()
                .map(|slug| PersonTeam {
                    slug: slug.to_string(),
                    name: Some(slug.to_string()),
                    role: Some("MEMBER".to_string()),
                })
                .collect(),
        }
    }

    fn export(people: Vec<Person>) -> Report {
        Report {
            source: Source {
                api_url: "https://api.github.com/graphql".to_string(),
                enterprise: Some("acme-inc".to_string()),
                include_child_team_members: false,
                teams: true,
                enterprise_members: Some(true),
            },
            organizations: vec!["acme".to_string(), "acme-labs".to_string()],
            organizations_without_team_data: Vec::new(),
            totals: Totals {
                organizations: 2,
                people: people.len(),
                teams: 0,
            },
            people,
        }
    }

    #[test]
    fn only_the_orgs_where_someone_holds_no_team_are_reported() {
        let report = export(vec![
            person("sam", vec![org("acme", &["owners"]), org("acme-labs", &[])]),
            person("dana", vec![org("acme", &["reviewers"])]),
            person("kim", vec![org("acme", &[]), org("acme-labs", &[])]),
        ]);

        let (people, _) = org_members_without_teams(&report, false);
        let logins: Vec<&str> = people.iter().map(|p| p.login.as_str()).collect();
        assert_eq!(logins, ["sam", "kim"]);

        // sam is on a team in acme, so only acme-labs is listed against them.
        let sam_orgs: Vec<&str> = people[0]
            .organizations
            .iter()
            .map(|o| o.org.as_str())
            .collect();
        assert_eq!(sam_orgs, ["acme-labs"]);
        assert_eq!(people[0].organizations[0].role.as_deref(), Some("MEMBER"));
        assert_eq!(people[1].organizations.len(), 2);
    }

    #[test]
    fn orgs_whose_teams_could_not_be_read_are_left_out_not_guessed_at() {
        let mut report = export(vec![
            person("sam", vec![org("acme", &[]), org("acme-labs", &[])]),
            person("dana", vec![org("acme", &[])]),
        ]);
        report.organizations_without_team_data = vec!["acme".to_string()];

        let (people, skipped) = org_members_without_teams(&report, false);
        assert_eq!(skipped, ["acme"]);
        // dana is only in acme, so they drop out entirely rather than being
        // reported as team-less on unreadable data.
        let logins: Vec<&str> = people.iter().map(|p| p.login.as_str()).collect();
        assert_eq!(logins, ["sam"]);
        assert_eq!(people[0].organizations[0].org, "acme-labs");
    }

    #[test]
    fn someone_in_no_org_is_an_enterprise_finding_not_a_team_finding() {
        let mut alone = person("owner", Vec::new());
        alone.enterprise_role = Some("OWNER".to_string());
        let report = export(vec![alone, person("sam", vec![org("acme", &[])])]);

        let without_org = enterprise_members_without_org(&report);
        assert_eq!(without_org.len(), 1);
        assert_eq!(without_org[0].login, "owner");
        assert_eq!(without_org[0].enterprise_role.as_deref(), Some("OWNER"));

        // …and they are not also reported as an org member without teams.
        let (without_teams, _) = org_members_without_teams(&report, false);
        let logins: Vec<&str> = without_teams.iter().map(|p| p.login.as_str()).collect();
        assert_eq!(logins, ["sam"]);
    }

    #[test]
    fn excluding_admins_drops_the_org_they_administer_not_the_whole_person() {
        let report = export(vec![
            person("sam", vec![admin_of("acme"), org("acme-labs", &[])]),
            person("dana", vec![admin_of("acme")]),
        ]);

        let (people, _) = org_members_without_teams(&report, true);
        // sam still holds a plain membership in acme-labs; dana administers the
        // only org they are in, so nothing is left to report about them.
        let logins: Vec<&str> = people.iter().map(|p| p.login.as_str()).collect();
        assert_eq!(logins, ["sam"]);
        let orgs: Vec<&str> = people[0]
            .organizations
            .iter()
            .map(|o| o.org.as_str())
            .collect();
        assert_eq!(orgs, ["acme-labs"]);

        // Without the flag both are reported, admin role and all.
        let (people, _) = org_members_without_teams(&report, false);
        let logins: Vec<&str> = people.iter().map(|p| p.login.as_str()).collect();
        assert_eq!(logins, ["sam", "dana"]);
        assert_eq!(people[0].organizations[0].role.as_deref(), Some("ADMIN"));
    }

    #[test]
    fn excluding_admins_drops_an_enterprise_owner_from_every_org() {
        let mut owner = person("sam", vec![org("acme", &[]), org("acme-labs", &[])]);
        owner.enterprise_role = Some("OWNER".to_string());
        let mut member = person("dana", vec![org("acme", &[])]);
        member.enterprise_role = Some("MEMBER".to_string());
        let report = export(vec![owner, member]);

        let (people, _) = org_members_without_teams(&report, true);
        // Owning the enterprise outranks any org role, so sam goes entirely,
        // while an ordinary enterprise member is untouched.
        let logins: Vec<&str> = people.iter().map(|p| p.login.as_str()).collect();
        assert_eq!(logins, ["dana"]);
    }

    #[test]
    fn everyone_on_a_team_produces_an_empty_report() {
        let report = export(vec![person("sam", vec![org("acme", &["owners"])])]);
        let (people, _) = org_members_without_teams(&report, false);
        assert!(people.is_empty());
    }

    #[test]
    fn exports_that_looked_at_neither_boundary_are_recognizable_as_such() {
        let mut report = export(vec![person("sam", vec![org("acme", &[])])]);
        assert!(enterprise_membership_available(&report));
        assert!(team_membership_available(&report));

        report.source.enterprise_members = None;
        report.source.teams = false;
        assert!(!enterprise_membership_available(&report));
        assert!(!team_membership_available(&report));
    }

    #[test]
    fn an_export_written_before_these_fields_existed_still_parses() {
        let yaml = r#"
source:
  api_url: https://api.github.com/graphql
  enterprise: acme-inc
  include_child_team_members: false
organizations:
- acme
totals:
  organizations: 1
  people: 1
  teams: 0
people:
- login: sam
  name: Sam
  organizations:
  - org: acme
    role: MEMBER
    teams: []
"#;
        let report: Report = serde_yaml_ng::from_str(yaml).expect("parses");
        // Teams default to collected, because every such export collected them.
        assert!(team_membership_available(&report));
        // Enterprise membership does not, because none of them read it.
        assert!(!enterprise_membership_available(&report));
        let (people, _) = org_members_without_teams(&report, false);
        assert_eq!(people.len(), 1);
    }
}
