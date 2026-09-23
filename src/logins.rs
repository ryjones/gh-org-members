//! Checking hand-written GitHub logins against the casing GitHub itself uses.
//!
//! GitHub treats a login as case-insensitive — `RyJones` and `ryjones` sign in
//! to the same account and both resolve over the API — so a config file that
//! lists people by hand can carry the wrong casing indefinitely without
//! anything breaking, and without anything saying so. The check here reads such
//! a file, asks GitHub for the canonical spelling of every name in it, and
//! reports the ones that do not match.
//!
//! Only the lookups cost API quota. Finding the names in the file and deciding
//! which answers are findings are pure functions, and tested as such.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_yaml_ng::Value as Yaml;

/// YAML keys whose values hold logins, used when no `--login-key` is given.
///
/// Both a scalar and a sequence of scalars are read, so this covers a
/// CLOWarden-style `maintainers: [a, b]` as well as the `login:` of a
/// `gh-org-members` export.
///
/// `owners` and `admins` are deliberately absent: they are ordinary team names,
/// and a CLOWarden `teams: {owners: maintain}` permission map would hand over a
/// permission where a login was expected. `--login-key owners` asks for them.
pub const DEFAULT_LOGIN_KEYS: &[&str] = &[
    "login",
    "logins",
    "maintainer",
    "maintainers",
    "member",
    "members",
    "people",
    "person",
    "user",
    "users",
];

/// How the names were found in the input.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InputFormat {
    /// A YAML mapping or sequence, walked for login-bearing keys.
    Yaml,
    /// One login per line.
    Text,
}

impl InputFormat {
    /// The word this format goes by in the output and on stderr.
    pub fn label(self) -> &'static str {
        match self {
            InputFormat::Yaml => "yaml",
            InputFormat::Text => "text",
        }
    }
}

/// One distinct spelling found in the input, with every place it appeared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GivenName {
    pub name: String,
    /// Where it was found: a YAML path such as `teams[0].maintainers[1]`, or
    /// `line 12` for a plain-text list. Ordered as the file is.
    pub occurrences: Vec<String>,
}

/// What GitHub says about one spelling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// GitHub knows the account; `login` is how GitHub spells it and `kind` is
    /// the GraphQL type name, `User` for a person and `Organization` for an org.
    Found { login: String, kind: Option<String> },
    /// No account answers to that login.
    Unknown,
}

/// A name whose spelling does not match GitHub's.
#[derive(Debug, Serialize)]
pub struct Mismatch {
    /// The spelling in the file.
    pub given: String,
    /// The spelling GitHub returned.
    pub actual: String,
    /// Present only when the account is not a `User` — an organization in a
    /// list of people is worth seeing even though the casing was the question.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub occurrences: Vec<String>,
}

/// A name GitHub could not resolve, or that cannot be a login at all.
#[derive(Debug, Serialize)]
pub struct Unresolved {
    pub given: String,
    pub occurrences: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct CaseTotals {
    /// Distinct spellings checked.
    pub names: usize,
    /// Places those spellings appeared, which is larger when a name is listed
    /// more than once.
    pub occurrences: usize,
    pub correct: usize,
    pub case_mismatches: usize,
    pub resolved_to_another_login: usize,
    pub unknown: usize,
    pub invalid: usize,
}

/// The verdict on one input file. Names that GitHub spells exactly as the file
/// does are counted and not listed: the point of the report is what to fix.
#[derive(Debug, Serialize)]
pub struct CaseCheck {
    pub totals: CaseTotals,
    /// Same account, different casing — the finding this check exists for.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub login_case_mismatches: Vec<Mismatch>,
    /// GitHub answered with a login that differs by more than case, which is
    /// what a renamed account looks like through a redirect.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub resolved_to_another_login: Vec<Mismatch>,
    /// No account by that name. A typo, a deleted account, or a rename with no
    /// redirect left.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unknown: Vec<Unresolved>,
    /// Not the shape of a GitHub login — an email address or a display name, for
    /// instance. Never looked up, because no account could have that name.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub invalid: Vec<Unresolved>,
}

impl CaseCheck {
    /// How many names need attention. Zero means the file agrees with GitHub.
    pub fn findings(&self) -> usize {
        self.login_case_mismatches.len()
            + self.resolved_to_another_login.len()
            + self.unknown.len()
            + self.invalid.len()
    }
}

/// Whether `name` could be a GitHub login: alphanumerics and single interior
/// hyphens, up to 39 characters. Anything else — an email address, a display
/// name, a team slug with a slash — cannot name an account, so looking it up
/// would spend quota to be told what its shape already says.
pub fn is_plausible_login(name: &str) -> bool {
    if name.is_empty() || name.len() > 39 {
        return false;
    }
    let bytes = name.as_bytes();
    if !bytes[0].is_ascii_alphanumeric() || !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
        return false;
    }
    let mut previous_hyphen = false;
    for &byte in bytes {
        match byte {
            b'-' if previous_hyphen => return false,
            b'-' => previous_hyphen = true,
            b if b.is_ascii_alphanumeric() => previous_hyphen = false,
            _ => return false,
        }
    }
    true
}

/// Find the names in `text`, ordered case-insensitively by name.
///
/// A document that parses as a YAML mapping or sequence is walked for the keys
/// in `keys`; anything else — including a bare list of logins, which YAML reads
/// as one multi-line scalar — is read a line at a time. The format that was
/// used comes back alongside, because it decides what the paths in
/// `occurrences` mean.
pub fn parse_names(text: &str, keys: &[String]) -> (Vec<GivenName>, InputFormat) {
    let parsed = serde_yaml_ng::from_str::<Yaml>(text).ok();
    let (found, format) = match &parsed {
        Some(value @ (Yaml::Mapping(_) | Yaml::Sequence(_))) => {
            let mut out = Vec::new();
            // A top-level sequence is the list itself, so its scalars count
            // without a key naming them; inside a mapping, only the keys asked
            // for do.
            let collect = matches!(value, Yaml::Sequence(_));
            walk(value, "", keys, collect, &mut out);
            (out, InputFormat::Yaml)
        }
        _ => (parse_lines(text), InputFormat::Text),
    };

    // Group by exact spelling: the same name written twice is one finding with
    // two occurrences, while two casings of one account are two names.
    let mut grouped: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for (name, path) in found {
        grouped
            .entry((name.to_lowercase(), name))
            .or_default()
            .push(path);
    }

    let names = grouped
        .into_iter()
        .map(|((_, name), occurrences)| GivenName { name, occurrences })
        .collect();
    (names, format)
}

/// One login per line, with `#` comments, blank lines, and the `- ` markers and
/// trailing commas of a hand-pasted list all tolerated.
fn parse_lines(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = match line.split_once('#') {
            Some((before, _)) => before,
            None => line,
        };
        let mut name = line.trim();
        name = name.strip_prefix("- ").unwrap_or(name).trim();
        name = name.strip_suffix(',').unwrap_or(name).trim();
        name = name.trim_matches(|c| c == '"' || c == '\'').trim();
        if name.is_empty() {
            continue;
        }
        out.push((name.to_string(), format!("line {}", index + 1)));
    }
    out
}

/// Walk a YAML document, collecting scalars that sit under a login-bearing key.
///
/// `collect` says whether the value being visited is already inside such a key.
/// It carries through sequences, so `members: [a, b]` and a list of lists both
/// work, but a mapping always decides afresh per key — otherwise a `members:`
/// list of records would swallow every field in them.
fn walk(value: &Yaml, path: &str, keys: &[String], collect: bool, out: &mut Vec<(String, String)>) {
    match value {
        Yaml::Mapping(map) => {
            for (key, child) in map {
                let Some(key) = key.as_str() else { continue };
                let child_path = if path.is_empty() {
                    key.to_string()
                } else {
                    format!("{path}.{key}")
                };
                let named = keys.iter().any(|k| k.eq_ignore_ascii_case(key));
                walk(child, &child_path, keys, named, out);
            }
        }
        Yaml::Sequence(seq) => {
            for (index, child) in seq.iter().enumerate() {
                walk(child, &format!("{path}[{index}]"), keys, collect, out);
            }
        }
        Yaml::Tagged(tagged) => walk(&tagged.value, path, keys, collect, out),
        Yaml::String(name) if collect => {
            let name = name.trim();
            if !name.is_empty() {
                out.push((name.to_string(), path.to_string()));
            }
        }
        _ => {}
    }
}

/// Decide what each name's answer means.
///
/// `resolutions` is keyed by the exact spelling from the file. A name with no
/// entry was never asked about — it failed [`is_plausible_login`] — and is
/// reported as invalid rather than as a missing account.
pub fn check(names: &[GivenName], resolutions: &BTreeMap<String, Resolution>) -> CaseCheck {
    let mut check = CaseCheck {
        totals: CaseTotals {
            names: names.len(),
            occurrences: names.iter().map(|n| n.occurrences.len()).sum(),
            correct: 0,
            case_mismatches: 0,
            resolved_to_another_login: 0,
            unknown: 0,
            invalid: 0,
        },
        login_case_mismatches: Vec::new(),
        resolved_to_another_login: Vec::new(),
        unknown: Vec::new(),
        invalid: Vec::new(),
    };

    for name in names {
        match resolutions.get(&name.name) {
            Some(Resolution::Found { login, .. }) if *login == name.name => {
                check.totals.correct += 1;
            }
            Some(Resolution::Found { login, kind }) => {
                let mismatch = Mismatch {
                    given: name.name.clone(),
                    actual: login.clone(),
                    kind: kind.clone().filter(|k| k != "User"),
                    occurrences: name.occurrences.clone(),
                };
                if login.eq_ignore_ascii_case(&name.name) {
                    check.totals.case_mismatches += 1;
                    check.login_case_mismatches.push(mismatch);
                } else {
                    check.totals.resolved_to_another_login += 1;
                    check.resolved_to_another_login.push(mismatch);
                }
            }
            Some(Resolution::Unknown) => {
                check.totals.unknown += 1;
                check.unknown.push(Unresolved {
                    given: name.name.clone(),
                    occurrences: name.occurrences.clone(),
                });
            }
            None => {
                check.totals.invalid += 1;
                check.invalid.push(Unresolved {
                    given: name.name.clone(),
                    occurrences: name.occurrences.clone(),
                });
            }
        }
    }

    check
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> Vec<String> {
        DEFAULT_LOGIN_KEYS.iter().map(|k| k.to_string()).collect()
    }

    fn names(text: &str) -> (Vec<String>, Vec<Vec<String>>, InputFormat) {
        let (names, format) = parse_names(text, &keys());
        (
            names.iter().map(|n| n.name.clone()).collect(),
            names.iter().map(|n| n.occurrences.clone()).collect(),
            format,
        )
    }

    fn found(login: &str) -> Resolution {
        Resolution::Found {
            login: login.to_string(),
            kind: Some("User".to_string()),
        }
    }

    fn resolutions(pairs: &[(&str, Resolution)]) -> BTreeMap<String, Resolution> {
        pairs
            .iter()
            .map(|(given, resolution)| (given.to_string(), resolution.clone()))
            .collect()
    }

    fn given(name: &str) -> GivenName {
        GivenName {
            name: name.to_string(),
            occurrences: vec!["line 1".to_string()],
        }
    }

    #[test]
    fn a_clowarden_config_yields_its_maintainers_and_members_and_nothing_else() {
        let (found, paths, format) = names(
            r#"
teams:
  - name: owners
    maintainers: [RyJones]
    members:
      - Sam
      - dana
repositories:
  - name: governance
    teams:
      owners: maintain
    visibility: public
"#,
        );
        assert_eq!(format, InputFormat::Yaml);
        // Team names, repo names, permissions and visibility are not logins —
        // including the `owners: maintain` under a repository's `teams`.
        assert_eq!(found, ["dana", "RyJones", "Sam"]);
        assert_eq!(paths[1], ["teams[0].maintainers[0]"]);
        assert_eq!(paths[2], ["teams[0].members[0]"]);
    }

    #[test]
    fn an_export_is_readable_too_because_login_is_a_login_bearing_key() {
        let (found, paths, _) = names(
            r#"
people:
  - login: sam
    name: Sam Example
    organizations:
      - org: acme
        role: MEMBER
"#,
        );
        // `name` holds a display name, not a login, and is not in the key set;
        // `org` and `role` are not either.
        assert_eq!(found, ["sam"]);
        assert_eq!(paths[0], ["people[0].login"]);
    }

    #[test]
    fn a_records_list_under_a_login_key_does_not_swallow_its_other_fields() {
        let (found, _, _) = names(
            r#"
members:
  - login: sam
    name: Sam Example
    title: staff
"#,
        );
        assert_eq!(found, ["sam"]);
    }

    #[test]
    fn a_bare_list_of_logins_is_read_a_line_at_a_time() {
        let (found, paths, format) = names(
            r#"
ryjones
# a comment line
Sam   # and a trailing one
- dana,
"quoted"
"#,
        );
        assert_eq!(format, InputFormat::Text);
        assert_eq!(found, ["dana", "quoted", "ryjones", "Sam"]);
        assert_eq!(paths[2], ["line 2"]);
        assert_eq!(paths[3], ["line 4"]);
    }

    #[test]
    fn a_yaml_sequence_of_logins_needs_no_key_to_name_it() {
        let (found, paths, format) = names("- RyJones\n- dana\n");
        assert_eq!(format, InputFormat::Yaml);
        assert_eq!(found, ["dana", "RyJones"]);
        assert_eq!(paths[1], ["[0]"]);
    }

    #[test]
    fn the_same_spelling_twice_is_one_name_with_two_occurrences() {
        let (found, paths, _) = names(
            r#"
teams:
  - name: a
    members: [sam]
  - name: b
    members: [sam, Sam]
"#,
        );
        // Two casings are two names; the repeated spelling is one. Names that
        // differ only in case keep a stable order among themselves.
        assert_eq!(found, ["Sam", "sam"]);
        assert_eq!(paths[0], ["teams[1].members[1]"]);
        assert_eq!(paths[1], ["teams[0].members[0]", "teams[1].members[0]"]);
    }

    #[test]
    fn only_the_requested_keys_are_read() {
        let keys = vec!["maintainers".to_string()];
        let (names, _) = parse_names("maintainers: [a]\nmembers: [b]\n", &keys);
        let found: Vec<&str> = names.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(found, ["a"]);
    }

    #[test]
    fn login_shapes_github_cannot_have_are_recognized_without_a_lookup() {
        assert!(is_plausible_login("ryjones"));
        assert!(is_plausible_login("RyJones"));
        assert!(is_plausible_login("adovale-IOB"));
        assert!(is_plausible_login("a"));
        assert!(!is_plausible_login(""));
        assert!(!is_plausible_login("-leading"));
        assert!(!is_plausible_login("trailing-"));
        assert!(!is_plausible_login("double--hyphen"));
        assert!(!is_plausible_login("someone@example.com"));
        assert!(!is_plausible_login("Sam Example"));
        assert!(!is_plausible_login("org/team"));
        assert!(!is_plausible_login(&"a".repeat(40)));
    }

    #[test]
    fn wrong_case_is_reported_and_right_case_is_only_counted() {
        let names = [given("RyJones"), given("dana")];
        let check = check(
            &names,
            &resolutions(&[("RyJones", found("ryjones")), ("dana", found("dana"))]),
        );

        assert_eq!(check.totals.names, 2);
        assert_eq!(check.totals.correct, 1);
        assert_eq!(check.totals.case_mismatches, 1);
        assert_eq!(check.login_case_mismatches.len(), 1);
        assert_eq!(check.login_case_mismatches[0].given, "RyJones");
        assert_eq!(check.login_case_mismatches[0].actual, "ryjones");
        // A user needs no `kind`; it is only there to flag the surprises.
        assert_eq!(check.login_case_mismatches[0].kind, None);
        assert_eq!(check.findings(), 1);
    }

    #[test]
    fn an_answer_that_differs_by_more_than_case_is_a_rename_not_a_casing_fix() {
        let check = check(
            &[given("oldname")],
            &resolutions(&[("oldname", found("newname"))]),
        );
        assert!(check.login_case_mismatches.is_empty());
        assert_eq!(check.totals.resolved_to_another_login, 1);
        assert_eq!(check.resolved_to_another_login[0].actual, "newname");
    }

    #[test]
    fn an_account_that_is_not_a_user_says_so() {
        let check = check(
            &[given("Hyperledger")],
            &resolutions(&[(
                "Hyperledger",
                Resolution::Found {
                    login: "hyperledger".to_string(),
                    kind: Some("Organization".to_string()),
                },
            )]),
        );
        assert_eq!(
            check.login_case_mismatches[0].kind.as_deref(),
            Some("Organization")
        );
    }

    #[test]
    fn a_name_github_does_not_know_and_one_it_was_never_asked_about_are_separate() {
        let names = [given("notauser"), given("someone@example.com")];
        let check = check(&names, &resolutions(&[("notauser", Resolution::Unknown)]));

        assert_eq!(check.unknown.len(), 1);
        assert_eq!(check.unknown[0].given, "notauser");
        assert_eq!(check.invalid.len(), 1);
        assert_eq!(check.invalid[0].given, "someone@example.com");
        assert_eq!(check.findings(), 2);
    }

    #[test]
    fn a_file_that_agrees_with_github_produces_only_totals() {
        let check = check(&[given("dana")], &resolutions(&[("dana", found("dana"))]));
        assert_eq!(check.findings(), 0);

        let yaml = crate::yaml::to_string(&check).expect("serializes");
        let parsed: Yaml = serde_yaml_ng::from_str(&yaml).expect("parses");
        assert_eq!(parsed["totals"]["correct"].as_u64(), Some(1));
        // Empty finding lists are absent, not empty.
        assert!(parsed.get("login_case_mismatches").is_none());
        assert!(parsed.get("unknown").is_none());
    }
}
