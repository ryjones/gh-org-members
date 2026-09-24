# gh-org-members

Queries a GitHub enterprise or organization over GraphQL and emits a YAML file
listing every person, ordered by login, with the teams they belong to in each
organization.

Works against github.com and GitHub Enterprise Server.

Ships two binaries: `gh-org-members` fetches the export, and `gh-org-reports`
reads one back and reports the people who fall through the gaps in it — org
members on no team, and enterprise members in no org. `gh-org-members
--check-logins` also checks a file of hand-written logins against the casing
GitHub itself uses.

## Build

```sh
cargo build --release
```

## Use

```sh
export GITHUB_TOKEN=…            # needs read:org (plus read:enterprise for --enterprise)

# every org in an enterprise
gh-org-members --enterprise acme-inc -o people.yaml

# specific orgs, or a mix of both
gh-org-members --org acme --org acme-labs -o people.yaml
gh-org-members --enterprise acme-inc --org partner-org -o people.yaml

# GitHub Enterprise Server
gh-org-members --hostname ghe.example.com --enterprise acme-inc -o people.yaml
```

With no `-o`, the YAML goes to stdout and progress goes to stderr, so
`gh-org-members --org acme > people.yaml` works too.

### Options

| Flag | Meaning |
| --- | --- |
| `-e, --enterprise <SLUG>` | Enterprise slug; every org in it is queried |
| `--org <SLUG>` | Organization login; repeatable, combinable with `--enterprise` |
| `-o, --output <FILE>` | Write YAML to a file instead of stdout |
| `--hostname <HOST>` | GitHub Enterprise Server host, e.g. `ghe.example.com` |
| `--api-url <URL>` | Full GraphQL endpoint, if it is not `https://<host>/api/graphql` |
| `--concurrency <N>` | Organizations queried at once (default 3, max 16) |
| `--max-retries <N>` | Retries per request (default 5) |
| `--batch-size <N>` | Items per cursor fetch (default 100, max 100) |
| `--include-child-team-members` | Count members inherited from child teams as parent-team members |
| `--no-teams` | Org membership only; skip teams entirely |
| `--include-email` | Include each person's publicly visible email |
| `--check-logins <FILE>` | Check the logins in a file instead of exporting people (see below) |
| `--login-key <KEY>` | YAML key holding logins in that file; repeatable, replaces the default set |

The token is read from `GITHUB_TOKEN`, falling back to `GH_TOKEN`, and then to
`gh auth token` if the `gh` CLI is logged in. The CLI's own credential is worth
preferring: it carries the SSO authorizations and organization grants that a
hand-made PAT has to be given one organization at a time, and an export made
with a token that cannot see an organization leaves that organization out
without saying so (see **Whose view an export is**). The startup line names
which of the three the run used.

## Output

```yaml
source:
  api_url: https://api.github.com/graphql
  authenticated_as: alice
  token_scopes: read:org, repo
  enterprise: acme-inc
  include_child_team_members: false
  teams: true
  enterprise_members: true
organizations:
  - acme
  - acme-labs
totals:
  organizations: 2
  people: 87
  teams: 31
  enterprise_members_without_org: 1
people:
  - login: alice
    name: Alice Example
    enterprise_role: MEMBER
    organizations:
      - org: acme
        role: MEMBER
        teams:
          - slug: platform-maintainers
            name: platform-maintainers
            role: MEMBER
          - slug: release-managers
            name: release-managers
            role: MAINTAINER
      - org: acme-labs
        role: MEMBER
        teams: []
  - login: carol
    name: Carol Example
    enterprise_role: OWNER
    organizations: []
```

Ordering is deterministic: people by login (case-insensitive), each person's
organizations by org login, each organization's teams by team slug. A person in
several organizations appears once, with one entry per organization. Someone
who belongs to no team still appears, with `teams: []`, and with `--enterprise`
someone who belongs to no organization at all appears with `organizations: []`.

`role` is `ADMIN` or `MEMBER` at the org level and `MAINTAINER` or `MEMBER` at
the team level. `enterprise_role` is `OWNER` or `MEMBER`, and is present only
with `--enterprise`. `email` is only present with `--include-email`, and only
when the account exposes one publicly.

`authenticated_as` and `token_scopes` say whose view the export is. An export
lists only the organizations its token can see, so the same enterprise read with
two credentials can yield two different org lists; `token_scopes` is absent for
fine-grained PATs and App tokens, which do not report scopes at all.

`enterprise_members_without_org` counts the people on the enterprise list who
are in none of the organizations listed, and appears only when that list was
read. See **Whose view an export is** for why it is worth a second look.

The two `source` flags exist so a later reader can tell an empty list from an
unasked question: `teams: false` means the run passed `--no-teams`, and
`enterprise_members` is present only when the enterprise's own people list was
read.

If teams could not be read for some organization — a token without `read:org`
there — that org is listed under `organizations_without_team_data` and its
people appear with no team membership, rather than silently looking team-less.

## Whose view an export is

An organization the token cannot see is not an error. `enterprise.organizations`
simply does not return it, the export lists the organizations that came back,
and nothing in the file says one is missing. Its members are still on the
enterprise people list, so they land in the export as people who belong to the
enterprise and to no organization at all.

That is the tell. A genuinely unaffiliated account or two is ordinary; a cluster
of them usually means an organization went unseen. When they are at least five
people and at least 2% of the export, the run prints a warning and records it
under `notes`:

```yaml
notes:
  - "enterprise_members_without_org: 32 of 800 people (4.0%) are in the enterprise
    but in none of the organizations listed here. An organization the token cannot
    see is omitted from the enterprise listing without an error, and its members
    look exactly like this; check `organizations` against the enterprise's own
    list before reading them as unaffiliated."
```

The note is a prompt to check, not a verdict: compare `organizations` against
the enterprise's organization list in the web UI, and re-run with a credential
that reaches all of them. `gh auth token` is usually that credential.

## Checking logins

GitHub treats a login as case-insensitive: `RyJones` and `ryjones` reach the
same account, and both resolve over the API. A file that lists people by hand —
a CLOWarden org config, a roster, a list pasted out of an email — can therefore
carry the wrong casing forever without anything breaking and without anything
saying so.

`--check-logins` reads such a file, asks GitHub how it spells every name in it,
and reports the ones that do not match. It queries accounts rather than orgs, so
it needs no `--enterprise` or `--org`, and it conflicts with them: exporting
people and checking a file are separate jobs.

```sh
gh-org-members --check-logins config.yaml
gh-org-members --check-logins config.yaml -o case.yaml
gh-org-members --check-logins config.yaml --login-key maintainers --login-key members
printf 'RyJones\ndana\n' | gh-org-members --check-logins -
```

The input may be either:

- **YAML**, walked for the keys that hold logins — by default `login`, `logins`,
  `maintainer(s)`, `member(s)`, `user(s)` and `people`/`person`, as a scalar or a
  list of scalars. `owners` and `admins` are *not* in that set: they are ordinary
  team names, and a CLOWarden `teams: {owners: maintain}` block would otherwise
  offer up a permission where a login was expected. `--login-key` replaces the
  set, so `--login-key owners` asks for them. A `gh-org-members` export works as
  input too, since `login:` is one of the keys.
- **Plain text**, one login per line, with `#` comments, blank lines, `- `
  markers and trailing commas tolerated. This is the fallback for anything that
  is not a YAML mapping or sequence, which includes a bare list of logins.

```yaml
source:
  input: config.yaml
  api_url: https://api.github.com/graphql
  format: yaml
  login_keys:
    - login
    - maintainers
    - members
totals:
  names: 41
  occurrences: 58
  correct: 36
  case_mismatches: 2
  resolved_to_another_login: 1
  unknown: 1
  invalid: 1
login_case_mismatches:
  - given: RyJones
    actual: ryjones
    occurrences:
      - teams[0].maintainers[0]
      - teams[3].members[2]
  - given: Hyperledger
    actual: hyperledger
    kind: Organization
    occurrences:
      - teams[1].members[0]
resolved_to_another_login:
  - given: oldname
    actual: newname
    occurrences:
      - teams[2].members[1]
unknown:
  - given: notauser
    occurrences:
      - teams[2].members[4]
invalid:
  - given: someone@example.com
    occurrences:
      - teams[2].members[5]
```

A name spelled the way GitHub spells it is counted and not listed: the report is
what to fix. Each finding carries every place the name appeared, as a YAML path
(`teams[0].maintainers[0]`) or a line number (`line 12`), so it can be corrected
without searching for it.

The four kinds of finding are deliberately separate:

- **`login_case_mismatches`** — the same account, spelled differently. `kind`
  appears when the account is not a `User`, because an organization in a list of
  people is worth seeing even though casing was the question.
- **`resolved_to_another_login`** — GitHub answered with a login that differs by
  more than case, which is what a renamed account looks like. The new spelling is
  a fact; whether the entry should follow it is not, so it is not called a case
  fix.
- **`unknown`** — no account answers to that name. A typo, a deleted account, or
  a rename with no redirect left.
- **`invalid`** — not the shape of a login at all (an email address, a display
  name, a team slug with a slash). These are never looked up: no account could be
  named that, so asking would spend quota to be told what the shape already says.

Names are distinct by exact spelling, so one name written twice is a single
finding with two occurrences, while two casings of one account are two names.
Ordering is case-insensitive by name, as in the export.

Every name is looked up — around fifty per request, one point each — and its
outcome printed as it lands, so a long list says what it has done rather than
going quiet:

```
[37/41] RyJones -- wrong case: GitHub spells it ryjones
[38/41] notauser -- no such account
```

The exit status is non-zero when the check finds anything to fix, so it can gate
a config file in CI. A run that finds nothing exits zero and writes only the
`source` and `totals` blocks.

## Reports

`gh-org-reports` takes an export and answers two questions about it. It makes no
API calls, so it can be re-run over a captured file for free.

```sh
# both reports
gh-org-reports people.yaml -o gaps.yaml

# one of them; reads stdin with `-`
gh-org-reports people.yaml --report enterprise-members-without-org
gh-org-members --enterprise acme-inc | gh-org-reports -
```

| Flag | Meaning |
| --- | --- |
| `-o, --output <FILE>` | Write YAML to a file instead of stdout |
| `--report <NAME>` | `all` (default), `org-members-without-teams`, or `enterprise-members-without-org`; repeatable |
| `--exclude-admins` | Leave organization admins and enterprise owners out of `org-members-without-teams` |

```yaml
source:
  input: people.yaml
  api_url: https://api.github.com/graphql
  enterprise: acme-inc
  organizations: 2
  people: 87
totals:
  org_members_without_teams: 34
  enterprise_members_without_org: 1
org_members_without_teams:
  # alice holds a team in acme, so only acme-labs is reported against them
  - login: alice
    name: Alice Example
    organizations:
      - org: acme-labs
        role: MEMBER
  - login: bob
    name: Bob Example
    organizations:
      - org: acme
        role: MEMBER
      - org: acme-labs
        role: MEMBER
enterprise_members_without_org:
  - login: carol
    name: Carol Example
    enterprise_role: OWNER
```

**`org_members_without_teams`** lists each person against only the organizations
in which they hold no team, so someone on a team in one org and on none in
another is reported for the second alone. Organizations under
`organizations_without_team_data` are left out and echoed into the report's own
`organizations_without_team_data`: there, "no teams" and "teams unknown" are
indistinguishable, and reporting them would be a guess.

`--exclude-admins` narrows it to people whose access could only have come from a
team. An `ADMIN` is dropped for the organization they administer but still
reported for any other organization where they are a plain member, and an
enterprise `OWNER` is dropped everywhere, since owning the enterprise outranks
every org role. The flag reaches only this report — an owner in no organization
is the whole point of the other one — so `source.exclude_admins` appears only
when it actually filtered something, and a shortened list is never mistaken for
a complete one.

**`enterprise_members_without_org`** needs an export made with `--enterprise`.
An export built only from `--org` cannot answer it — everyone in it came from an
org listing — so the report is omitted and the reason recorded under `notes`
rather than being reported as nobody. Likewise, a `--no-teams` export cannot
answer the first report. The exit status is non-zero only when *every* requested
report is unanswerable.

## Behavior worth knowing

- **Both tools emit `yq .` formatting.** Block sequences are indented under the
  key that owns them. libyaml, which serde_yaml_ng emits through, writes them
  flush with the key instead — valid, but hard to follow at the depth these
  files reach. Scalars are still rendered by libyaml, so quoting is untouched
  and `yq .` over the output is a no-op.
- **Cursor pagination throughout.** Every connection advances by
  `after: <endCursor>`; there are no page numbers or offsets. Teams whose
  membership exceeds one batch are continued with a follow-up query, since a
  nested connection cannot be advanced in place.
- **Team membership is direct by default.** `--include-child-team-members`
  switches to GitHub's `ALL` semantics, where a parent team also reports the
  members of its child teams.
- **`email` costs a scope, so it is asked for only when wanted.** The `email`
  field needs `read:user` or `user:email`, and GitHub refuses the *entire*
  query when the token lacks them rather than omitting that one field. The
  queries therefore name `email` only under `--include-email`, so an ordinary
  export works with a `read:org` token.
- **Rate limits.** The client tracks the `x-ratelimit-*` headers and waits for
  the reset before spending the last of the budget, honors `Retry-After`,
  recognizes secondary rate limits and `RATE_LIMITED` responses on an otherwise
  successful request, and backs off exponentially on 5xx and timeouts. A
  hostname that does not resolve fails immediately instead of retrying.
- **Partial results beat no results.** One unreadable organization is reported
  on stderr and the run continues; the exit status is non-zero only if every
  organization fails. The enterprise people list is treated the same way: if it
  cannot be read, the org listings are still exported, and the export says so
  rather than implying nobody sits outside an org.
- **Login lookups are batched and degrade gracefully.** Around fifty logins go
  out per query as aliased `repositoryOwner` fields — `repositoryOwner` rather
  than `user` so an organization resolves too. A login nothing answers to may
  come back as a null field or as an error against that field, and in the second
  case GitHub may withhold the rest of the response; when that happens the batch
  is halved and retried, down to single logins. Only an error that names a
  missing account is read as "no such account": anything else fails the run,
  because passing off an unread name as absent would turn a broken check into a
  clean-looking one.
- **Enterprise owners are fetched separately.** They administer the enterprise
  rather than belong to it, so `enterprise.members` leaves them out. Reading
  them needs a token that owns the enterprise; without one they are skipped
  with a warning and the rest of the export is unaffected.

## Tests

```sh
cargo test
```

Unit tests cover report assembly (ordering, cross-org merging, case-insensitive
login identity, withheld fields), the two gap reports and what makes each of
them unanswerable, the login check (which keys a YAML input gives up and which
it does not, plain-text parsing, and how each answer from GitHub is classified),
YAML layout, and the backoff/reset arithmetic. They make no network calls.

`results/` holds YAML captured from real runs against a live enterprise, kept out
of git and kept around so the output shape can be inspected without re-spending
API quota. Each capture is named for the enterprise it came from:
`<slug>.batch3.yaml` is the same export fetched with `--batch-size 3` and is
identical to the default-batch export, which is how the cursor paths are
verified, and `<slug>.gaps.yaml` is `gh-org-reports` run over
`<slug>.enterprise.yaml`.

Every example in this README is invented. Real captures name real people, so
they stay in `results/`, which `.gitignore` covers.
