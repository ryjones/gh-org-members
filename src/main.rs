use std::collections::BTreeMap;
use std::io::{Read as _, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::Parser;
use futures::stream::{self, StreamExt};
use serde::Serialize;

use gh_org_members::client::GithubClient;
use gh_org_members::collect::{Collector, EnterpriseMember, OrgSnapshot, TeamMembership};
use gh_org_members::logins::{self, CaseCheck, InputFormat, Resolution};
use gh_org_members::model::*;
use gh_org_members::yaml;

/// Export the people in a GitHub enterprise or organization, and the teams they
/// belong to, as YAML.
#[derive(Debug, Parser)]
#[command(name = "gh-org-members", version, about, long_about = None)]
struct Args {
    /// Enterprise slug; every organization in it is queried.
    #[arg(short, long, value_name = "SLUG")]
    enterprise: Option<String>,

    /// Organization login. Repeatable, and may be combined with --enterprise.
    #[arg(long, value_name = "SLUG")]
    org: Vec<String>,

    /// GraphQL endpoint. Defaults to github.com, or to the GitHub Enterprise
    /// Server endpoint derived from --hostname.
    #[arg(long, value_name = "URL")]
    api_url: Option<String>,

    /// GitHub Enterprise Server hostname, e.g. ghe.example.com.
    #[arg(long, value_name = "HOST", conflicts_with = "api_url")]
    hostname: Option<String>,

    /// Write YAML here instead of stdout.
    #[arg(short, long, value_name = "FILE")]
    output: Option<PathBuf>,

    /// Organizations to query at once.
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u16).range(1..=16))]
    concurrency: u16,

    /// Retries per request before giving up (rate limits, 5xx, timeouts).
    #[arg(long, default_value_t = 5)]
    max_retries: u32,

    /// Count members inherited from child teams as members of the parent team.
    #[arg(long)]
    include_child_team_members: bool,

    /// Skip teams entirely and list org membership only.
    #[arg(long)]
    no_teams: bool,

    /// Include each person's publicly visible email address.
    #[arg(long)]
    include_email: bool,

    /// Items requested per cursor fetch. Lower this if a large instance times
    /// out; pagination itself is always cursor-driven.
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..=100))]
    batch_size: u32,

    /// Check the GitHub logins in this file against the casing GitHub itself
    /// uses, instead of exporting people. `-` reads stdin.
    #[arg(
        long,
        value_name = "FILE",
        conflicts_with_all = [
            "enterprise",
            "org",
            "no_teams",
            "include_child_team_members",
            "include_email",
        ]
    )]
    check_logins: Option<PathBuf>,

    /// YAML key whose values hold logins in the checked file. Repeatable, and
    /// replaces the default set rather than adding to it.
    #[arg(long, value_name = "KEY", requires = "check_logins")]
    login_key: Vec<String>,
}

/// The YAML `gh-org-members --check-logins` writes.
#[derive(Debug, Serialize)]
struct LoginCheckOutput {
    source: LoginCheckSource,
    #[serde(flatten)]
    check: CaseCheck,
}

#[derive(Debug, Serialize)]
struct LoginCheckSource {
    /// The file the names came from.
    input: String,
    api_url: String,
    /// `yaml` when the file was walked for login-bearing keys, `text` when it
    /// was read a line at a time. It decides what an `occurrences` path means.
    format: InputFormat,
    /// The keys that were read. Absent for a text input, which has none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    login_keys: Vec<String>,
}

/// Accumulates one person across every org and team they appear in.
#[derive(Default)]
struct PersonAccum {
    login: String,
    name: Option<String>,
    email: Option<String>,
    enterprise_role: Option<String>,
    orgs: BTreeMap<String, OrgAccum>,
}

/// What was read at the enterprise level, as opposed to per organization.
#[derive(Default)]
struct EnterpriseInput {
    /// Everyone on the enterprise's people list, owners included. Empty when no
    /// enterprise was queried or when the list could not be read.
    people: Vec<EnterpriseMember>,
    /// `Some` once the people list has been attempted — `true` if it was read,
    /// `false` if it failed. `None` when no enterprise was queried. This is what
    /// tells a later reader whether an empty `organizations` list is meaningful.
    read: Option<bool>,
}

#[derive(Default)]
struct OrgAccum {
    role: Option<String>,
    teams: BTreeMap<String, PersonTeam>,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<ExitCode> {
    let args = Args::parse();

    if args.check_logins.is_none() && args.enterprise.is_none() && args.org.is_empty() {
        bail!("pass --enterprise <slug> and/or --org <slug>, or --check-logins <file>");
    }

    let token = std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GH_TOKEN"))
        .map_err(|_| anyhow::anyhow!("set GITHUB_TOKEN (or GH_TOKEN) to a token with read:org"))?;
    if token.trim().is_empty() {
        bail!("GITHUB_TOKEN is set but empty");
    }

    let api_url = match (&args.api_url, &args.hostname) {
        (Some(url), _) => url.clone(),
        (None, Some(host)) => {
            let host = host.trim_end_matches('/');
            if host.starts_with("http://") || host.starts_with("https://") {
                format!("{host}/api/graphql")
            } else {
                format!("https://{host}/api/graphql")
            }
        }
        (None, None) => "https://api.github.com/graphql".to_string(),
    };

    let client = GithubClient::new(&api_url, token.trim(), args.max_retries)?;
    let collector = Collector::new(
        &client,
        args.include_child_team_members,
        args.no_teams,
        args.batch_size,
    );

    let viewer = collector.viewer_login().await?;
    eprintln!("Authenticated as {viewer} at {api_url}");

    if let Some(path) = &args.check_logins {
        return check_logins(&args, &api_url, &collector, path).await;
    }

    // Resolve the org list.
    let mut orgs: Vec<String> = Vec::new();
    let mut enterprise = EnterpriseInput::default();
    if let Some(slug) = &args.enterprise {
        eprintln!("Listing organizations in enterprise `{slug}` …");
        orgs.extend(collector.enterprise_orgs(slug).await?);
        eprintln!("  found {} organization(s)", orgs.len());

        eprintln!("Listing people in enterprise `{slug}` …");
        match collector.enterprise_members(slug).await {
            Ok(people) => {
                eprintln!("  found {} person/people", people.len());
                enterprise.people = people;
                enterprise.read = Some(true);
            }
            Err(err) => {
                // Without this list, people who are in the enterprise but in no
                // org are invisible; the org listings still stand on their own.
                eprintln!("  warning: could not read enterprise members: {err:#}");
                enterprise.read = Some(false);
            }
        }
        // Owners are administrators of the enterprise, not members of it, so
        // they are absent from the list above and fetched separately.
        match collector.enterprise_admins(slug).await {
            Ok(admins) => {
                eprintln!("  found {} owner(s)", admins.len());
                enterprise.people.extend(admins);
            }
            Err(err) => {
                eprintln!("  warning: could not read enterprise owners: {err:#}");
            }
        }
    }
    orgs.extend(args.org.iter().cloned());
    orgs.sort_by_key(|o| o.to_lowercase());
    orgs.dedup_by_key(|o| o.to_lowercase());

    if orgs.is_empty() {
        bail!("no organizations to query");
    }

    // Query orgs concurrently; one failure should not sink the others.
    let total = orgs.len();
    let snapshots: Vec<Result<OrgSnapshot>> = stream::iter(orgs.iter().enumerate())
        .map(|(index, login)| {
            let collector = &collector;
            async move {
                eprintln!("[{}/{total}] {login}", index + 1);
                collector.org_snapshot(login).await
            }
        })
        .buffer_unordered(args.concurrency as usize)
        .collect()
        .await;

    let mut succeeded: Vec<OrgSnapshot> = Vec::new();
    let mut failures = 0usize;
    for snapshot in snapshots {
        match snapshot {
            Ok(snapshot) => succeeded.push(snapshot),
            Err(err) => {
                failures += 1;
                eprintln!("error: {err:#}");
            }
        }
    }
    if succeeded.is_empty() {
        bail!("every organization failed to query");
    }

    let report = build_report(&args, &api_url, succeeded, enterprise);

    let yaml = yaml::to_string(&report).context("failed to serialize YAML")?;
    match &args.output {
        Some(path) => {
            std::fs::write(path, &yaml)
                .with_context(|| format!("failed to write {}", path.display()))?;
            eprintln!("Wrote {} ({} people)", path.display(), report.people.len());
        }
        None => {
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(yaml.as_bytes())?;
        }
    }

    let rate = client.rate_state();
    if let (Some(remaining), Some(limit)) = (rate.remaining, rate.limit) {
        eprintln!("Rate limit: {remaining}/{limit} points remaining");
    }
    if failures > 0 {
        eprintln!("Warning: {failures} organization(s) failed; output is partial");
    }
    Ok(ExitCode::SUCCESS)
}

/// Read the logins in a file and report the ones GitHub spells differently.
///
/// Every name is looked up, and the outcome of each printed as it lands, so a
/// long list says what it has done rather than going quiet. The exit status is
/// non-zero when anything needs fixing, which is what makes this usable as a
/// gate over a config file.
async fn check_logins(
    args: &Args,
    api_url: &str,
    collector: &Collector<'_>,
    path: &Path,
) -> Result<ExitCode> {
    let text = read_input(path)?;
    let input = input_name(path);
    let keys: Vec<String> = if args.login_key.is_empty() {
        logins::DEFAULT_LOGIN_KEYS
            .iter()
            .map(|k| k.to_string())
            .collect()
    } else {
        args.login_key.clone()
    };

    let (names, format) = logins::parse_names(&text, &keys);
    if names.is_empty() {
        bail!("no logins found in {input}");
    }

    // A name that cannot be a login is reported without a lookup: no account
    // could be spelled that way, so asking would spend quota to learn nothing.
    let queryable: Vec<String> = names
        .iter()
        .map(|name| name.name.clone())
        .filter(|name| logins::is_plausible_login(name))
        .collect();
    let skipped = names.len() - queryable.len();
    eprintln!(
        "Checking {} name(s) from {input} ({} format){}",
        names.len(),
        format.label(),
        if skipped > 0 {
            format!("; {skipped} cannot be a login and will not be looked up")
        } else {
            String::new()
        }
    );

    let total = queryable.len();
    let mut resolutions: BTreeMap<String, Resolution> = BTreeMap::new();
    let mut done = 0usize;
    for chunk in queryable.chunks(collector.login_batch_size()) {
        for (name, resolution) in collector.resolve_logins(chunk).await? {
            done += 1;
            eprintln!("[{done}/{total}] {name} -- {}", outcome(&name, &resolution));
            resolutions.insert(name, resolution);
        }
    }

    let output = LoginCheckOutput {
        source: LoginCheckSource {
            input,
            api_url: api_url.to_string(),
            format,
            login_keys: match format {
                InputFormat::Yaml => keys,
                InputFormat::Text => Vec::new(),
            },
        },
        check: logins::check(&names, &resolutions),
    };

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

    let rate = collector.rate_state();
    if let (Some(remaining), Some(limit)) = (rate.remaining, rate.limit) {
        eprintln!("Rate limit: {remaining}/{limit} points remaining");
    }

    let findings = output.check.findings();
    if findings == 0 {
        eprintln!(
            "All {} name(s) are spelled the way GitHub spells them",
            output.check.totals.names
        );
        return Ok(ExitCode::SUCCESS);
    }
    let totals = &output.check.totals;
    eprintln!(
        "{findings} of {} name(s) need attention: {} wrong case, {} renamed, {} unknown, {} not a login",
        totals.names,
        totals.case_mismatches,
        totals.resolved_to_another_login,
        totals.unknown,
        totals.invalid
    );
    Ok(ExitCode::FAILURE)
}

/// One line's worth of verdict on a name, for the progress output.
fn outcome(given: &str, resolution: &Resolution) -> String {
    match resolution {
        Resolution::Found { login, kind } if login == given => match kind.as_deref() {
            Some(kind) if kind != "User" => {
                format!("ok, but that is a GitHub {}", kind.to_lowercase())
            }
            _ => "ok".to_string(),
        },
        Resolution::Found { login, .. } if login.eq_ignore_ascii_case(given) => {
            format!("wrong case: GitHub spells it {login}")
        }
        Resolution::Found { login, .. } => format!("resolves to {login}"),
        Resolution::Unknown => "no such account".to_string(),
    }
}

fn read_input(path: &Path) -> Result<String> {
    if path == Path::new("-") {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("failed to read stdin")?;
        return Ok(buf);
    }
    std::fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))
}

fn input_name(path: &Path) -> String {
    if path == Path::new("-") {
        "stdin".to_string()
    } else {
        path.display().to_string()
    }
}

fn build_report(
    args: &Args,
    api_url: &str,
    snapshots: Vec<OrgSnapshot>,
    enterprise: EnterpriseInput,
) -> Report {
    // Keyed by lowercased login: GitHub logins are unique case-insensitively,
    // and iterating a BTreeMap on that key yields the case-insensitive
    // ascending order the report promises.
    let mut people: BTreeMap<String, PersonAccum> = BTreeMap::new();
    let mut org_logins: Vec<String> = Vec::new();
    let mut unreadable_teams: Vec<String> = Vec::new();
    let mut team_total = 0usize;

    for snapshot in snapshots {
        org_logins.push(snapshot.login.clone());
        team_total += snapshot.team_count;
        if !snapshot.teams_readable && !args.no_teams {
            unreadable_teams.push(snapshot.login.clone());
        }
        let org = snapshot.login;

        for (actor, role) in snapshot.members {
            let entry = person_entry(&mut people, &actor);
            entry.orgs.entry(org.clone()).or_default().role = role;
        }

        for (actor, membership) in snapshot.team_memberships {
            let entry = person_entry(&mut people, &actor);
            // A team member the org listing did not return still gets an entry
            // here, with no org-level role.
            let org_entry = entry.orgs.entry(org.clone()).or_default();
            let TeamMembership { slug, name, role } = membership;
            org_entry.teams.insert(
                slug.clone(),
                PersonTeam {
                    slug,
                    name: Some(name),
                    role,
                },
            );
        }
    }

    // Last, so that someone on the enterprise list who is in no organization
    // still gets an entry — with `organizations: []`, which is exactly what the
    // "in the enterprise, in no org" report looks for.
    for member in enterprise.people {
        let entry = person_entry(&mut people, &member.actor);
        if member.role == "OWNER" || entry.enterprise_role.is_none() {
            entry.enterprise_role = Some(member.role);
        }
    }

    org_logins.sort_by_key(|o| o.to_lowercase());
    unreadable_teams.sort_by_key(|o| o.to_lowercase());

    let people: Vec<Person> = people
        .into_values()
        .map(|accum| Person {
            login: accum.login,
            name: accum.name,
            email: if args.include_email {
                accum.email
            } else {
                None
            },
            enterprise_role: accum.enterprise_role,
            organizations: accum
                .orgs
                .into_iter()
                .map(|(org, org_accum)| PersonOrg {
                    org,
                    role: org_accum.role,
                    teams: org_accum.teams.into_values().collect(),
                })
                .collect(),
        })
        .collect();

    Report {
        source: Source {
            api_url: api_url.to_string(),
            enterprise: args.enterprise.clone(),
            include_child_team_members: args.include_child_team_members,
            teams: !args.no_teams,
            enterprise_members: enterprise.read,
        },
        organizations: org_logins.clone(),
        organizations_without_team_data: unreadable_teams,
        totals: Totals {
            organizations: org_logins.len(),
            people: people.len(),
            teams: team_total,
        },
        people,
    }
}

/// Look up (or create) a person, filling in details the first time they are seen
/// with a non-empty value.
///
/// Keyed on the lowercased login so the same account seen in two orgs merges
/// into one entry, with the original casing kept for display.
fn person_entry<'a>(
    people: &'a mut BTreeMap<String, PersonAccum>,
    actor: &Actor,
) -> &'a mut PersonAccum {
    let entry = people
        .entry(actor.login.to_lowercase())
        .or_insert_with(|| PersonAccum {
            login: actor.login.clone(),
            ..Default::default()
        });
    if entry.name.is_none() {
        entry.name = actor.name.clone().filter(|s| !s.trim().is_empty());
    }
    if entry.email.is_none() {
        entry.email = actor.email.clone().filter(|s| !s.trim().is_empty());
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use gh_org_members::logins::GivenName;

    fn args(extra: &[&str]) -> Args {
        let mut argv = vec!["gh-org-members", "--org", "example"];
        argv.extend_from_slice(extra);
        Args::parse_from(argv)
    }

    fn actor(login: &str) -> Actor {
        Actor {
            login: login.to_string(),
            name: Some(format!("{login} display")),
            email: Some(format!("{login}@example.com")),
        }
    }

    /// An enterprise people list that was read successfully.
    fn enterprise(people: Vec<(&str, &str)>) -> EnterpriseInput {
        EnterpriseInput {
            people: people
                .into_iter()
                .map(|(login, role)| EnterpriseMember {
                    actor: actor(login),
                    role: role.to_string(),
                })
                .collect(),
            read: Some(true),
        }
    }

    fn team(slug: &str, role: &str) -> TeamMembership {
        TeamMembership {
            slug: slug.to_string(),
            name: slug.to_string(),
            role: Some(role.to_string()),
        }
    }

    fn snapshot(
        login: &str,
        members: Vec<(Actor, Option<String>)>,
        team_memberships: Vec<(Actor, TeamMembership)>,
    ) -> OrgSnapshot {
        let team_count = team_memberships.len();
        OrgSnapshot {
            login: login.to_string(),
            members,
            team_memberships,
            team_count,
            teams_readable: true,
        }
    }

    #[test]
    fn people_are_ordered_case_insensitively() {
        let snapshots = vec![snapshot(
            "acme",
            vec![
                (actor("zoe"), Some("MEMBER".into())),
                (actor("Adam"), Some("ADMIN".into())),
                (actor("bea"), Some("MEMBER".into())),
            ],
            vec![],
        )];
        let report = build_report(
            &args(&[]),
            "https://api.github.com/graphql",
            snapshots,
            EnterpriseInput::default(),
        );
        let logins: Vec<&str> = report.people.iter().map(|p| p.login.as_str()).collect();
        assert_eq!(logins, ["Adam", "bea", "zoe"]);
    }

    #[test]
    fn one_person_across_two_orgs_merges_into_one_entry() {
        let snapshots = vec![
            snapshot(
                "beta",
                vec![(actor("sam"), Some("MEMBER".into()))],
                vec![(actor("sam"), team("reviewers", "MEMBER"))],
            ),
            snapshot(
                "alpha",
                vec![(actor("sam"), Some("ADMIN".into()))],
                vec![
                    (actor("sam"), team("owners", "MAINTAINER")),
                    (actor("sam"), team("admins", "MEMBER")),
                ],
            ),
        ];
        let report = build_report(
            &args(&[]),
            "https://api.github.com/graphql",
            snapshots,
            EnterpriseInput::default(),
        );

        assert_eq!(report.people.len(), 1);
        assert_eq!(report.totals.people, 1);
        assert_eq!(report.organizations, ["alpha", "beta"]);

        let sam = &report.people[0];
        // Orgs ordered by login, teams ordered by slug.
        let orgs: Vec<&str> = sam.organizations.iter().map(|o| o.org.as_str()).collect();
        assert_eq!(orgs, ["alpha", "beta"]);
        assert_eq!(sam.organizations[0].role.as_deref(), Some("ADMIN"));
        let teams: Vec<&str> = sam.organizations[0]
            .teams
            .iter()
            .map(|t| t.slug.as_str())
            .collect();
        assert_eq!(teams, ["admins", "owners"]);
        assert_eq!(
            sam.organizations[0].teams[1].role.as_deref(),
            Some("MAINTAINER")
        );
    }

    #[test]
    fn the_same_login_in_different_casing_is_one_person() {
        let snapshots = vec![
            snapshot("alpha", vec![(actor("Sam"), Some("MEMBER".into()))], vec![]),
            snapshot("beta", vec![(actor("sam"), Some("MEMBER".into()))], vec![]),
        ];
        let report = build_report(
            &args(&[]),
            "https://api.github.com/graphql",
            snapshots,
            EnterpriseInput::default(),
        );
        assert_eq!(report.people.len(), 1);
        assert_eq!(report.people[0].organizations.len(), 2);
    }

    #[test]
    fn a_team_member_missing_from_the_org_listing_still_appears() {
        let snapshots = vec![snapshot(
            "acme",
            vec![],
            vec![(actor("ghost"), team("secret", "MEMBER"))],
        )];
        let report = build_report(
            &args(&[]),
            "https://api.github.com/graphql",
            snapshots,
            EnterpriseInput::default(),
        );
        assert_eq!(report.people.len(), 1);
        // No org-level role was readable, but the team membership is recorded.
        assert_eq!(report.people[0].organizations[0].role, None);
        assert_eq!(report.people[0].organizations[0].teams[0].slug, "secret");
    }

    #[test]
    fn email_is_withheld_unless_requested() {
        let snapshots = vec![snapshot(
            "acme",
            vec![(actor("sam"), Some("MEMBER".into()))],
            vec![],
        )];
        let without = build_report(
            &args(&[]),
            "https://api.github.com/graphql",
            snapshots,
            EnterpriseInput::default(),
        );
        assert_eq!(without.people[0].email, None);

        let snapshots = vec![snapshot(
            "acme",
            vec![(actor("sam"), Some("MEMBER".into()))],
            vec![],
        )];
        let with = build_report(
            &args(&["--include-email"]),
            "https://api.github.com/graphql",
            snapshots,
            EnterpriseInput::default(),
        );
        assert_eq!(with.people[0].email.as_deref(), Some("sam@example.com"));
    }

    #[test]
    fn blank_names_and_emails_are_omitted() {
        let blank = Actor {
            login: "sam".into(),
            name: Some("  ".into()),
            email: Some(String::new()),
        };
        let snapshots = vec![snapshot(
            "acme",
            vec![(blank, Some("MEMBER".into()))],
            vec![],
        )];
        let report = build_report(
            &args(&["--include-email"]),
            "https://api.github.com/graphql",
            snapshots,
            EnterpriseInput::default(),
        );
        assert_eq!(report.people[0].name, None);
        assert_eq!(report.people[0].email, None);
    }

    #[test]
    fn orgs_with_unreadable_teams_are_flagged() {
        let mut unreadable = snapshot("acme", vec![(actor("sam"), Some("MEMBER".into()))], vec![]);
        unreadable.teams_readable = false;
        let report = build_report(
            &args(&[]),
            "https://api.github.com/graphql",
            vec![unreadable],
            EnterpriseInput::default(),
        );
        assert_eq!(report.organizations_without_team_data, ["acme"]);
    }

    #[test]
    fn yaml_round_trips_to_the_documented_shape() {
        let snapshots = vec![snapshot(
            "acme",
            vec![(actor("sam"), Some("ADMIN".into()))],
            vec![(actor("sam"), team("owners", "MAINTAINER"))],
        )];
        let report = build_report(
            &args(&[]),
            "https://api.github.com/graphql",
            snapshots,
            EnterpriseInput::default(),
        );
        let yaml = serde_yaml_ng::to_string(&report).expect("serializes");
        let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&yaml).expect("parses");

        let person = &parsed["people"][0];
        assert_eq!(person["login"].as_str(), Some("sam"));
        assert_eq!(person["organizations"][0]["org"].as_str(), Some("acme"));
        assert_eq!(
            person["organizations"][0]["teams"][0]["slug"].as_str(),
            Some("owners")
        );
        // Withheld fields are absent, not null.
        assert!(person.get("email").is_none());
        assert!(parsed.get("organizations_without_team_data").is_none());
    }

    #[test]
    fn an_enterprise_member_in_no_org_still_appears() {
        let snapshots = vec![snapshot(
            "acme",
            vec![(actor("sam"), Some("MEMBER".into()))],
            vec![],
        )];
        let report = build_report(
            &args(&[]),
            "https://api.github.com/graphql",
            snapshots,
            enterprise(vec![("sam", "MEMBER"), ("dana", "MEMBER")]),
        );

        let dana = report
            .people
            .iter()
            .find(|p| p.login == "dana")
            .expect("listed");
        assert!(dana.organizations.is_empty());
        assert_eq!(dana.enterprise_role.as_deref(), Some("MEMBER"));
        assert_eq!(report.totals.people, 2);
        assert_eq!(report.source.enterprise_members, Some(true));
    }

    #[test]
    fn owning_the_enterprise_outranks_being_a_member_of_it() {
        let snapshots = vec![snapshot("acme", vec![], vec![])];
        // The owner list is fetched after the member list, but the two can also
        // arrive in the other order without changing the answer.
        let report = build_report(
            &args(&[]),
            "https://api.github.com/graphql",
            snapshots,
            enterprise(vec![("sam", "MEMBER"), ("sam", "OWNER")]),
        );
        assert_eq!(report.people[0].enterprise_role.as_deref(), Some("OWNER"));

        let snapshots = vec![snapshot("acme", vec![], vec![])];
        let report = build_report(
            &args(&[]),
            "https://api.github.com/graphql",
            snapshots,
            enterprise(vec![("sam", "OWNER"), ("sam", "MEMBER")]),
        );
        assert_eq!(report.people[0].enterprise_role.as_deref(), Some("OWNER"));
    }

    #[test]
    fn enterprise_membership_that_was_never_read_is_not_recorded_as_empty() {
        let snapshots = vec![snapshot(
            "acme",
            vec![(actor("sam"), Some("MEMBER".into()))],
            vec![],
        )];
        let report = build_report(
            &args(&[]),
            "https://api.github.com/graphql",
            snapshots,
            EnterpriseInput::default(),
        );
        assert_eq!(report.source.enterprise_members, None);
        assert_eq!(report.people[0].enterprise_role, None);
    }

    fn login_check(
        input: &str,
        format: InputFormat,
        keys: &[&str],
        names: &[GivenName],
        resolutions: &[(&str, Resolution)],
    ) -> LoginCheckOutput {
        let resolutions: BTreeMap<String, Resolution> = resolutions
            .iter()
            .map(|(given, resolution)| (given.to_string(), resolution.clone()))
            .collect();
        LoginCheckOutput {
            source: LoginCheckSource {
                input: input.to_string(),
                api_url: "https://api.github.com/graphql".to_string(),
                format,
                login_keys: match format {
                    InputFormat::Yaml => keys.iter().map(|k| k.to_string()).collect(),
                    InputFormat::Text => Vec::new(),
                },
            },
            check: logins::check(names, &resolutions),
        }
    }

    fn given(name: &str, occurrence: &str) -> GivenName {
        GivenName {
            name: name.to_string(),
            occurrences: vec![occurrence.to_string()],
        }
    }

    fn found(login: &str) -> Resolution {
        Resolution::Found {
            login: login.to_string(),
            kind: Some("User".to_string()),
        }
    }

    #[test]
    fn a_login_check_says_where_the_names_came_from_and_what_to_fix() {
        let output = login_check(
            "config.yaml",
            InputFormat::Yaml,
            &["maintainers", "members"],
            &[
                given("RyJones", "teams[0].maintainers[0]"),
                given("dana", "teams[0].members[0]"),
                given("notauser", "teams[1].members[0]"),
                given("sam@example.com", "teams[1].members[1]"),
            ],
            &[
                ("RyJones", found("ryjones")),
                ("dana", found("dana")),
                ("notauser", Resolution::Unknown),
            ],
        );
        assert_eq!(output.check.findings(), 3);

        let yaml = yaml::to_string(&output).expect("serializes");
        let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&yaml).expect("parses");

        assert_eq!(parsed["source"]["input"].as_str(), Some("config.yaml"));
        assert_eq!(parsed["source"]["format"].as_str(), Some("yaml"));
        assert_eq!(parsed["source"]["login_keys"][1].as_str(), Some("members"));
        assert_eq!(parsed["totals"]["names"].as_u64(), Some(4));
        assert_eq!(parsed["totals"]["correct"].as_u64(), Some(1));
        assert_eq!(
            parsed["login_case_mismatches"][0]["given"].as_str(),
            Some("RyJones")
        );
        assert_eq!(
            parsed["login_case_mismatches"][0]["actual"].as_str(),
            Some("ryjones")
        );
        assert_eq!(
            parsed["login_case_mismatches"][0]["occurrences"][0].as_str(),
            Some("teams[0].maintainers[0]")
        );
        assert_eq!(parsed["unknown"][0]["given"].as_str(), Some("notauser"));
        // The address was never looked up, so it is invalid rather than unknown.
        assert_eq!(
            parsed["invalid"][0]["given"].as_str(),
            Some("sam@example.com")
        );
        // A name spelled the way GitHub spells it is counted, not listed.
        assert!(yaml.find("dana").is_none());
    }

    #[test]
    fn a_text_input_records_no_keys_because_it_has_none() {
        let output = login_check(
            "logins.txt",
            InputFormat::Text,
            &[],
            &[given("dana", "line 1")],
            &[("dana", found("dana"))],
        );
        let yaml = yaml::to_string(&output).expect("serializes");
        let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&yaml).expect("parses");
        assert_eq!(parsed["source"]["format"].as_str(), Some("text"));
        assert!(parsed["source"].get("login_keys").is_none());
        // Nothing to fix, so only the totals are written.
        assert_eq!(output.check.findings(), 0);
        assert!(parsed.get("login_case_mismatches").is_none());
    }

    #[test]
    fn each_name_gets_a_progress_line_naming_its_outcome() {
        assert_eq!(outcome("dana", &found("dana")), "ok");
        assert_eq!(
            outcome("RyJones", &found("ryjones")),
            "wrong case: GitHub spells it ryjones"
        );
        assert_eq!(outcome("oldname", &found("newname")), "resolves to newname");
        assert_eq!(outcome("nope", &Resolution::Unknown), "no such account");
        assert_eq!(
            outcome(
                "hyperledger",
                &Resolution::Found {
                    login: "hyperledger".to_string(),
                    kind: Some("Organization".to_string()),
                }
            ),
            "ok, but that is a GitHub organization"
        );
    }

    #[test]
    fn checking_logins_does_not_ask_for_an_org_as_well() {
        // The two jobs are separate: a slug passed alongside the file would be
        // read as an export request and silently ignored.
        assert!(Args::try_parse_from(["gh-org-members", "--check-logins", "c.yaml"]).is_ok());
        assert!(
            Args::try_parse_from([
                "gh-org-members",
                "--check-logins",
                "c.yaml",
                "--org",
                "acme"
            ])
            .is_err()
        );
        assert!(Args::try_parse_from(["gh-org-members", "--login-key", "members"]).is_err());
    }

    #[test]
    fn skipping_teams_is_recorded_in_the_source_block() {
        let snapshots = vec![snapshot("acme", vec![], vec![])];
        let report = build_report(
            &args(&["--no-teams"]),
            "https://api.github.com/graphql",
            snapshots,
            EnterpriseInput::default(),
        );
        assert!(!report.source.teams);
    }
}
