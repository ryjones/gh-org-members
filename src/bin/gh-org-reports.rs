//! Read a `gh-org-members` export and report the people who fall through the
//! gaps in it: org members on no team, and enterprise members in no org.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use serde::Serialize;

use gh_org_members::model::Report;
use gh_org_members::reports::{
    self, PersonWithoutOrg, PersonWithoutTeams, enterprise_membership_available,
    team_membership_available,
};
use gh_org_members::yaml;

/// Report on the people a gh-org-members export leaves in the gaps.
#[derive(Debug, Parser)]
#[command(name = "gh-org-reports", version, about, long_about = None)]
struct Args {
    /// YAML export written by gh-org-members. `-` reads stdin.
    #[arg(value_name = "FILE")]
    input: PathBuf,

    /// Write YAML here instead of stdout.
    #[arg(short, long, value_name = "FILE")]
    output: Option<PathBuf>,

    /// Which report to produce. Repeatable; defaults to both.
    #[arg(long, value_enum, default_value = "all", value_name = "NAME")]
    report: Vec<ReportKind>,

    /// Leave organization admins and enterprise owners out of
    /// org-members-without-teams. Their access comes from the role rather than
    /// from a team, so holding no team says nothing about them.
    #[arg(long)]
    exclude_admins: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum ReportKind {
    /// Both reports.
    All,
    /// People who belong to an organization but to none of its teams.
    OrgMembersWithoutTeams,
    /// People who belong to the enterprise but to none of its organizations.
    EnterpriseMembersWithoutOrg,
}

impl Args {
    fn wants(&self, kind: ReportKind) -> bool {
        self.report
            .iter()
            .any(|r| *r == kind || *r == ReportKind::All)
    }
}

#[derive(Debug, Serialize)]
struct Output {
    source: OutputSource,
    /// Orgs left out of `org_members_without_teams` because their teams could
    /// not be read, so "no teams" there would be a guess.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    organizations_without_team_data: Vec<String>,
    /// Reports that were asked for but could not be produced from this export.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    notes: Vec<String>,
    totals: OutputTotals,
    #[serde(skip_serializing_if = "Option::is_none")]
    org_members_without_teams: Option<Vec<PersonWithoutTeams>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    enterprise_members_without_org: Option<Vec<PersonWithoutOrg>>,
}

#[derive(Debug, Serialize)]
struct OutputSource {
    /// The export this was derived from.
    input: String,
    api_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    enterprise: Option<String>,
    organizations: usize,
    people: usize,
    /// Present only when `--exclude-admins` actually filtered a report, so a
    /// shortened list is never mistaken for a complete one.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    exclude_admins: bool,
}

#[derive(Debug, Serialize)]
struct OutputTotals {
    #[serde(skip_serializing_if = "Option::is_none")]
    org_members_without_teams: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    enterprise_members_without_org: Option<usize>,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();

    let export = read_export(&args.input)?;
    let (output, produced) = build_output(&args, &args.input, &export);

    let yaml = yaml::to_string(&output).context("failed to serialize YAML")?;
    match &args.output {
        Some(path) => {
            std::fs::write(path, &yaml)
                .with_context(|| format!("failed to write {}", path.display()))?;
            eprintln!("Wrote {}", path.display());
        }
        None => {
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(yaml.as_bytes())?;
        }
    }

    for note in &output.notes {
        eprintln!("warning: {note}");
    }
    if produced == 0 {
        bail!("no requested report could be produced from this export");
    }
    Ok(())
}

fn read_export(path: &Path) -> Result<Report> {
    let yaml = if path == Path::new("-") {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("failed to read stdin")?;
        buf
    } else {
        std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?
    };

    serde_yaml_ng::from_str(&yaml)
        .with_context(|| format!("{} is not a gh-org-members export", path.display()))
}

/// Returns the output and the number of requested reports that could be filled
/// in. A report the export cannot answer becomes a note instead of an empty
/// list, so "nobody" and "never looked" stay distinguishable.
fn build_output(args: &Args, input: &Path, export: &Report) -> (Output, usize) {
    let mut notes = Vec::new();
    let mut produced = 0usize;

    let mut skipped_orgs = Vec::new();
    let org_members_without_teams = if !args.wants(ReportKind::OrgMembersWithoutTeams) {
        None
    } else if !team_membership_available(export) {
        notes.push(
            "org_members_without_teams: this export was written with --no-teams, so it holds no \
             team membership to report on"
                .to_string(),
        );
        None
    } else {
        let (people, skipped) = reports::org_members_without_teams(export, args.exclude_admins);
        skipped_orgs = skipped;
        produced += 1;
        Some(people)
    };

    let enterprise_members_without_org = if !args.wants(ReportKind::EnterpriseMembersWithoutOrg) {
        None
    } else if !enterprise_membership_available(export) {
        notes.push(
            "enterprise_members_without_org: this export does not record the enterprise's own \
             people list, so everyone in it came from an organization; re-run gh-org-members \
             --enterprise <slug> to populate it"
                .to_string(),
        );
        None
    } else {
        produced += 1;
        Some(reports::enterprise_members_without_org(export))
    };

    let output = Output {
        source: OutputSource {
            input: input.display().to_string(),
            api_url: export.source.api_url.clone(),
            enterprise: export.source.enterprise.clone(),
            organizations: export.organizations.len(),
            people: export.people.len(),
            // Only the first report honors the flag, so saying it was applied
            // would be a lie if that report was not the one produced.
            exclude_admins: args.exclude_admins && org_members_without_teams.is_some(),
        },
        organizations_without_team_data: skipped_orgs,
        notes,
        totals: OutputTotals {
            org_members_without_teams: org_members_without_teams.as_ref().map(Vec::len),
            enterprise_members_without_org: enterprise_members_without_org.as_ref().map(Vec::len),
        },
        org_members_without_teams,
        enterprise_members_without_org,
    };
    (output, produced)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gh_org_members::model::{Person, PersonOrg, PersonTeam, Source, Totals};

    fn args(extra: &[&str]) -> Args {
        let mut argv = vec!["gh-org-reports", "export.yaml"];
        argv.extend_from_slice(extra);
        Args::parse_from(argv)
    }

    fn export(enterprise_members: Option<bool>, teams: bool) -> Report {
        Report {
            source: Source {
                api_url: "https://api.github.com/graphql".to_string(),
                enterprise: Some("acme-inc".to_string()),
                include_child_team_members: false,
                teams,
                enterprise_members,
            },
            organizations: vec!["acme".to_string()],
            organizations_without_team_data: Vec::new(),
            totals: Totals {
                organizations: 1,
                people: 2,
                teams: 1,
            },
            people: vec![
                Person {
                    login: "sam".to_string(),
                    name: None,
                    email: None,
                    enterprise_role: Some("MEMBER".to_string()),
                    organizations: vec![PersonOrg {
                        org: "acme".to_string(),
                        role: Some("MEMBER".to_string()),
                        teams: Vec::new(),
                    }],
                },
                Person {
                    login: "dana".to_string(),
                    name: None,
                    email: None,
                    enterprise_role: Some("OWNER".to_string()),
                    organizations: Vec::new(),
                },
            ],
        }
    }

    #[test]
    fn both_reports_are_produced_by_default() {
        let (output, produced) = build_output(
            &args(&[]),
            Path::new("export.yaml"),
            &export(Some(true), true),
        );
        assert_eq!(produced, 2);
        assert_eq!(output.totals.org_members_without_teams, Some(1));
        assert_eq!(output.totals.enterprise_members_without_org, Some(1));
        assert!(output.notes.is_empty());
    }

    #[test]
    fn asking_for_one_report_leaves_the_other_out_entirely() {
        let (output, produced) = build_output(
            &args(&["--report", "org-members-without-teams"]),
            Path::new("export.yaml"),
            &export(Some(true), true),
        );
        assert_eq!(produced, 1);
        assert!(output.org_members_without_teams.is_some());
        assert!(output.enterprise_members_without_org.is_none());
        // Not answerable and not asked for are different; neither gets a note.
        assert!(output.notes.is_empty());
    }

    #[test]
    fn a_report_the_export_cannot_answer_becomes_a_note_not_an_empty_list() {
        let (output, produced) =
            build_output(&args(&[]), Path::new("export.yaml"), &export(None, true));
        assert_eq!(produced, 1);
        assert!(output.enterprise_members_without_org.is_none());
        assert_eq!(output.notes.len(), 1);
        assert!(output.notes[0].starts_with("enterprise_members_without_org:"));
    }

    #[test]
    fn an_export_with_no_team_data_cannot_answer_the_team_report() {
        let (output, produced) = build_output(
            &args(&["--report", "org-members-without-teams"]),
            Path::new("export.yaml"),
            &export(Some(true), false),
        );
        assert_eq!(produced, 0);
        assert!(output.org_members_without_teams.is_none());
        assert_eq!(output.notes.len(), 1);
    }

    #[test]
    fn yaml_omits_the_reports_that_were_not_produced() {
        let (output, _) = build_output(&args(&[]), Path::new("export.yaml"), &export(None, true));
        let yaml = serde_yaml_ng::to_string(&output).expect("serializes");
        let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&yaml).expect("parses");

        assert!(parsed.get("enterprise_members_without_org").is_none());
        assert_eq!(
            parsed["org_members_without_teams"][0]["login"].as_str(),
            Some("sam")
        );
        assert_eq!(
            parsed["totals"]["org_members_without_teams"].as_u64(),
            Some(1)
        );
        assert!(
            parsed["totals"]
                .get("enterprise_members_without_org")
                .is_none()
        );
    }

    #[test]
    fn excluding_admins_is_recorded_only_when_it_applied() {
        let mut export = export(Some(true), true);
        export.people[0].organizations[0].role = Some("ADMIN".to_string());

        let (output, _) = build_output(&args(&["--exclude-admins"]), Path::new("e.yaml"), &export);
        assert_eq!(output.totals.org_members_without_teams, Some(0));
        assert!(output.source.exclude_admins);

        // The flag does not reach the enterprise report, so claiming it was
        // applied when only that report ran would misdescribe the output.
        let (output, _) = build_output(
            &args(&[
                "--exclude-admins",
                "--report",
                "enterprise-members-without-org",
            ]),
            Path::new("e.yaml"),
            &export,
        );
        assert!(!output.source.exclude_admins);
        assert_eq!(output.totals.enterprise_members_without_org, Some(1));
    }

    #[test]
    fn a_person_on_a_team_is_not_reported() {
        let mut export = export(Some(true), true);
        export.people[0].organizations[0].teams = vec![PersonTeam {
            slug: "owners".to_string(),
            name: None,
            role: Some("MEMBER".to_string()),
        }];
        let (output, _) = build_output(&args(&[]), Path::new("export.yaml"), &export);
        assert_eq!(output.totals.org_members_without_teams, Some(0));
    }
}
