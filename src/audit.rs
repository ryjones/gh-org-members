//! The login list `fetch-audit-history.zsh` reads, derived from an export.
//!
//! Asking GitHub who is in an enterprise is the expensive half of an audit-log
//! sweep, and an export already answers it. This turns a captured export into
//! the script's input without spending a point of quota, so the sweep can be
//! re-scoped — everyone, or one of the gap cohorts — as often as it needs to be.

use std::collections::HashSet;

use clap::ValueEnum;

use crate::model::Report;
use crate::reports;

/// Which people the list names.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum Cohort {
    /// Everyone in the export.
    #[default]
    All,
    /// People who belong to an organization but to none of its teams.
    OrgMembersWithoutTeams,
    /// People who belong to the enterprise but to none of its organizations.
    EnterpriseMembersWithoutOrg,
}

impl Cohort {
    /// What this cohort needs the export to have collected, and what to say
    /// when it did not.
    pub fn requires(self, report: &Report) -> Result<(), &'static str> {
        match self {
            Cohort::All => Ok(()),
            Cohort::OrgMembersWithoutTeams if !reports::team_membership_available(report) => {
                Err("this export was written with --no-teams, so it cannot say who is on no team")
            }
            Cohort::EnterpriseMembersWithoutOrg
                if !reports::enterprise_membership_available(report) =>
            {
                Err(
                    "this export has no enterprise people list, so everyone in it came from an \
                     organization and nobody can be outside one",
                )
            }
            _ => Ok(()),
        }
    }
}

/// Header and one row per person, ordered as the export orders people.
///
/// The login is first because that is the column the script reads; the rest is
/// context for whoever opens the file, and costs nothing to carry.
pub fn login_list(report: &Report, cohort: Cohort) -> String {
    let selected: Option<HashSet<String>> = match cohort {
        Cohort::All => None,
        Cohort::OrgMembersWithoutTeams => Some(
            reports::org_members_without_teams(report, false)
                .0
                .iter()
                .map(|person| person.login.to_lowercase())
                .collect(),
        ),
        Cohort::EnterpriseMembersWithoutOrg => Some(
            reports::enterprise_members_without_org(report)
                .iter()
                .map(|person| person.login.to_lowercase())
                .collect(),
        ),
    };

    let mut out = String::from(
        "\"login\",\"name\",\"enterprise_role\",\"orgs\",\"org_count\",\"team_count\"\n",
    );
    for person in &report.people {
        if let Some(selected) = &selected
            && !selected.contains(&person.login.to_lowercase())
        {
            continue;
        }
        let orgs: Vec<&str> = person
            .organizations
            .iter()
            .map(|org| org.org.as_str())
            .collect();
        let teams: usize = person.organizations.iter().map(|org| org.teams.len()).sum();
        out.push_str(&format!(
            "{},{},{},{},{},{}\n",
            quote(&person.login),
            quote(person.name.as_deref().unwrap_or_default()),
            quote(person.enterprise_role.as_deref().unwrap_or_default()),
            quote(&orgs.join("; ")),
            person.organizations.len(),
            teams
        ));
    }
    out
}

/// Quote as `@csv` does: every string field quoted, inner quotes doubled.
fn quote(field: &str) -> String {
    format!("\"{}\"", field.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Person, PersonOrg, PersonTeam, Source, Totals};

    fn person(login: &str, name: Option<&str>, orgs: Vec<(&str, usize)>) -> Person {
        Person {
            login: login.to_string(),
            name: name.map(str::to_string),
            email: None,
            enterprise_role: Some("MEMBER".to_string()),
            organizations: orgs
                .into_iter()
                .map(|(org, teams)| PersonOrg {
                    org: org.to_string(),
                    role: Some("MEMBER".to_string()),
                    teams: (0..teams)
                        .map(|n| PersonTeam {
                            slug: format!("team-{n}"),
                            name: Some(format!("team-{n}")),
                            role: Some("MEMBER".to_string()),
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    fn report(people: Vec<Person>, teams: bool, enterprise_members: Option<bool>) -> Report {
        Report {
            source: Source {
                api_url: "https://api.github.com/graphql".to_string(),
                authenticated_as: None,
                token_scopes: None,
                enterprise: Some("acme-inc".to_string()),
                include_child_team_members: false,
                teams,
                enterprise_members,
            },
            organizations: vec!["acme".to_string()],
            organizations_without_team_data: Vec::new(),
            totals: Totals {
                organizations: 1,
                people: people.len(),
                teams: 0,
                enterprise_members_without_org: None,
            },
            notes: Vec::new(),
            people,
        }
    }

    fn sample() -> Report {
        report(
            vec![
                person("alice", Some("Alice Example"), vec![("acme", 2)]),
                person("bob", None, vec![("acme", 0), ("acme-labs", 1)]),
                person("carol", Some("Carol Example"), vec![]),
                person("dan", Some("Dan, \"Danny\""), vec![("acme", 0)]),
            ],
            true,
            Some(true),
        )
    }

    fn logins(csv: &str) -> Vec<String> {
        csv.lines()
            .skip(1)
            .map(|line| {
                line.split(',')
                    .next()
                    .unwrap()
                    .trim_matches('"')
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn all_lists_everyone_in_export_order() {
        let csv = login_list(&sample(), Cohort::All);
        assert_eq!(logins(&csv), ["alice", "bob", "carol", "dan"]);
        assert!(csv.starts_with("\"login\",\"name\",\"enterprise_role\""));
    }

    #[test]
    fn a_name_with_a_comma_or_a_quote_stays_one_field() {
        let csv = login_list(&sample(), Cohort::All);
        let row = csv.lines().find(|l| l.starts_with("\"dan\"")).unwrap();
        assert!(row.contains("\"Dan, \"\"Danny\"\"\""));
        // The script reads column one; quoting must not shift it.
        assert_eq!(logins(&csv)[3], "dan");
    }

    #[test]
    fn cohorts_narrow_the_list() {
        // bob holds a team in acme-labs but none in acme, so he is reported;
        // this matches gh-org-reports, which reports per-org rather than per-person.
        let csv = login_list(&sample(), Cohort::OrgMembersWithoutTeams);
        assert_eq!(logins(&csv), ["bob", "dan"]);

        let csv = login_list(&sample(), Cohort::EnterpriseMembersWithoutOrg);
        assert_eq!(logins(&csv), ["carol"]);
    }

    #[test]
    fn counts_come_from_the_whole_person_not_the_cohort() {
        let csv = login_list(&sample(), Cohort::OrgMembersWithoutTeams);
        let row = csv.lines().find(|l| l.starts_with("\"bob\"")).unwrap();
        // Both orgs, and the team he does hold elsewhere.
        assert!(row.ends_with("\"acme; acme-labs\",2,1"));
    }

    #[test]
    fn a_cohort_the_export_cannot_answer_is_refused() {
        let no_teams = report(vec![person("alice", None, vec![("acme", 0)])], false, None);
        assert!(Cohort::OrgMembersWithoutTeams.requires(&no_teams).is_err());
        assert!(Cohort::All.requires(&no_teams).is_ok());

        let org_only = report(vec![person("alice", None, vec![("acme", 1)])], true, None);
        assert!(
            Cohort::EnterpriseMembersWithoutOrg
                .requires(&org_only)
                .is_err()
        );
    }
}
