# gh-org-members

Queries a GitHub enterprise or organization over GraphQL and emits a YAML file
listing every person, ordered by login, with the teams they belong to in each
organization.

Works against github.com and GitHub Enterprise Server.

Ships two binaries: `gh-org-members` fetches the export, and `gh-org-reports`
reads one back and reports the people who fall through the gaps in it — org
members on no team, and enterprise members in no org.

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

The token is read from `GITHUB_TOKEN`, falling back to `GH_TOKEN`.

## Output

```yaml
source:
  api_url: https://api.github.com/graphql
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

The two `source` flags exist so a later reader can tell an empty list from an
unasked question: `teams: false` means the run passed `--no-teams`, and
`enterprise_members` is present only when the enterprise's own people list was
read.

If teams could not be read for some organization — a token without `read:org`
there — that org is listed under `organizations_without_team_data` and its
people appear with no team membership, rather than silently looking team-less.

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
them unanswerable, YAML layout, and the backoff/reset arithmetic. They make no
network calls.

`results/` holds YAML captured from real runs against a live enterprise, kept out
of git and kept around so the output shape can be inspected without re-spending
API quota. Each capture is named for the enterprise it came from:
`<slug>.batch3.yaml` is the same export fetched with `--batch-size 3` and is
identical to the default-batch export, which is how the cursor paths are
verified, and `<slug>.gaps.yaml` is `gh-org-reports` run over
`<slug>.enterprise.yaml`.

Every example in this README is invented. Real captures name real people, so
they stay in `results/`, which `.gitignore` covers.
