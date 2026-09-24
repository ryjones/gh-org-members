# HOWTO: reconcile an enterprise's declared access against its real access

**BLUF —** This is the runbook for answering "who has access to what, who said
they should, and where do those two disagree" across a GitHub enterprise
managed by [CLOWarden](https://github.com/cncf/clowarden). It spans two
repositories — `gh-org-members` (people, teams, audit history, reconciliation)
and `gh-enterprise-repos` (repositories) — plus the CLOWarden config
repositories, which are mirrored locally and read as git history. Most of the
work costs no API calls. The single most important step is the first one:
**use a credential that can see every organization**, because a token that
cannot see one omits it in silence, and every number downstream is then quietly
wrong.

Worked end to end against `lf-decentralized-trust` on 2026-09-24; the figures
below are from that run and are there to show the shape of an answer, not as
expected values.

## What the two repositories do

| Repository | Binary / script | Answers |
| --- | --- | --- |
| `gh-org-members` | `gh-org-members` | Who is in the enterprise, in which orgs, on which teams |
| | `gh-org-reports` | Who falls in the gaps: org members on no team, enterprise members in no org |
| | `fetch-audit-history.zsh` | What the audit log says happened to a login (~180 days) |
| | `mirror-clowarden.zsh` | Local clones of every CLOWarden config repo |
| | `clowarden-history.py` | Membership history from config git history (years, not months) |
| | `clowarden-drift.py` | Where declared access and real access disagree |
| `gh-enterprise-repos` | `gh-enterprise-repos` | Every repository, by visibility and archived state |

The two checkouts are expected to sit side by side; the commands below assume
`gh-org-members` is the working directory and `../gh-enterprise-repos` is the
other.

## 0. Prerequisites

- `gh` (logged in), `yq`, `jq`, `python3`, and a Rust toolchain for the binaries.
- Scopes on whatever credential you use: `read:org` throughout,
  `read:enterprise` (or `admin:enterprise`) for enterprise listings,
  `read:audit_log` (or `admin:enterprise`) for the audit log, and `read:user`
  **only** if you want `--include-email`.
- Build once: `cargo build --release` in each repository.

### The credential trap, which is the whole ballgame

`enterprise.organizations` returns only the organizations the credential can
see. A classic PAT needs per-organization SSO authorization; a fine-grained PAT
is scoped to selected organizations. An organization it cannot see is **not an
error** — it is simply absent, and its people then appear in the export as
enterprise members who belong to no organization at all.

Both binaries resolve a token as `GITHUB_TOKEN` → `GH_TOKEN` → `gh auth token`,
and print which one they used. The CLI's own credential already carries the SSO
authorizations a PAT must be granted one organization at a time, so prefer it:

```sh
env -u GITHUB_TOKEN -u GH_TOKEN ./target/release/gh-org-members --enterprise acme-inc -o results/people.yaml
```

Setting `GITHUB_TOKEN=$GITHUB_TOKEN` on the command line **defeats the
fallback** — the environment wins by design. On the reference run, the PAT saw
53 organizations and the `gh` token saw 54; the missing one held 37 people and
14 teams.

## 1. Export the people

```sh
env -u GITHUB_TOKEN -u GH_TOKEN ./target/release/gh-org-members \
  --enterprise acme-inc -o results/people.yaml
```

Then **check the export before trusting it**:

```sh
yq '.source, .totals, .notes' results/people.yaml
```

- `authenticated_as` and `token_scopes` say whose view this is.
- `totals.organizations` — compare it against the enterprise's own organization
  list in the web UI. They must match.
- `totals.enterprise_members_without_org` — people in the enterprise and in no
  organization. A handful is ordinary. A cluster is the signature of an
  organization the credential could not see, and the export says so under
  `notes` when it is at least five people and at least 2% of the export.

On the reference run the bad export showed 32 of 803 (4.0%) and the good one
showed 1.

## 2. Find the gaps

```sh
./target/release/gh-org-reports results/people.yaml -o results/gaps.yaml
```

`org_members_without_teams` reports a person against only the organizations
where they hold no team, so someone on a team in one org and none in another is
reported for the second alone. `enterprise_members_without_org` needs an export
made with `--enterprise`.

Flat CSVs for a spreadsheet, straight from the export:

```sh
yq -o=json '.people' results/people.yaml | jq -r '
  ["login","name","enterprise_role","org","org_role","team_slug","team_name","team_role"],
  (.[] | . as $p
   | (if (($p.organizations // []) | length) == 0 then [null] else $p.organizations end)
   | .[] | . as $o
   | (if ($o == null) then [null] elif ((($o.teams // []) | length) == 0) then [null] else $o.teams end)
   | .[] | . as $t
   | [$p.login,$p.name,$p.enterprise_role,$o.org,$o.role,$t.slug,$t.name,$t.role])
  | map(. // "") | @csv' > people.csv
```

## 3. Export the repositories

From `../gh-enterprise-repos`. Four slices, so the parts add up: public,
private and internal partition the whole, and archived cuts across it.

```sh
cd ../gh-enterprise-repos
for spec in "all:include:repos" "private:include:repos-private" "internal:include:repos-internal"; do
  vis=${spec%%:*}; rest=${spec#*:}; arch=${rest%%:*}; name=${rest#*:}
  env -u GITHUB_TOKEN -u GH_TOKEN ./target/release/gh-enterprise-repos \
    --enterprise acme-inc --visibility $vis --archived $arch \
    -o reports/acme-inc-$name.yaml
done
env -u GITHUB_TOKEN -u GH_TOKEN ./target/release/gh-enterprise-repos \
  --enterprise acme-inc --visibility all --archived only \
  -o reports/acme-inc-repos-archived.yaml

for f in reports/*.yaml; do yq -o=json '.' "$f" > "${f%.yaml}.json"; done
```

Cross-check that the slices reconcile:

```sh
jq '[.repositories[].visibility] | group_by(.) | map({(.[0]): length}) | add' reports/acme-inc-repos.json
jq '[.repositories[] | select(.archived)] | length' reports/acme-inc-repos.json
```

`totals.organizations_without_repositories` is this tool's equivalent symptom:
an organization the credential can see but not see *into* answers with an empty
list rather than an error. Under a filter that is unremarkable, so it is only
raised as a `notes` entry when the run filtered nothing out.

## 4. Find what CLOWarden manages

Where it is installed, and how:

```sh
for org in $(jq -r '.organizations[]' reports/acme-inc-repos.json); do
  sel=$(gh api "/orgs/$org/installations?per_page=100" \
        --jq '.installations[] | select(.app_slug | test("clowarden";"i")) | .repository_selection' 2>/dev/null | head -1)
  printf '%s\t%s\n' "$org" "${sel:-NOT-INSTALLED}"
done
```

Expect `all`: CLOWarden acts on every repository in an org, and only *comments*
in the one repository holding its config. That config repo is almost always
`<org>/governance`, and the file has four known spellings — `config.yaml`,
`teams.yml`, `teams.yaml`, `access-control.yaml` — which all share one schema:

```yaml
teams:
  - name: some-team
    maintainers: [alice]
    members: [bob]
repositories:
  - name: some-repo
    teams: {some-team: maintain}
    collaborators: [carol]     # optional; also a login-bearing field
```

Find it per org by probing those names in `governance`, `.clowarden`,
`clowarden` and `.github`. Organizations with no config are out of scope for
reconciliation — there is no declared intent to compare against.

## 5. Mirror the configs

```sh
cd ../gh-org-members
./mirror-clowarden.zsh ../gh-enterprise-repos/reports/clowarden-watched-repos.csv
```

Clones into `mirror/<org>/<repo>` over HTTPS, eight at a time, and **fetches
instead of re-cloning** when a checkout already exists — so this is also the
refresh step. `mirror/` is gitignored.

## 6. Membership history from git

```sh
./clowarden-history.py --people results/people.yaml --out reports
```

Writes `reports/<login>/<login>.json` and `.csv` — one merged, chronological
history per login across **every** organization, so a login that moved from one
org to another reads as a move. The JSON adds a per-org block with first event,
last event and teams.

This is the deepest source available: on the reference run it reached back to
2024-03-18, against the audit log's ~180 days, and it names the human who merged
each change rather than `clowarden[bot]`.

Changes are `added`, `removed`, `role_changed` and `initial`, the last meaning
"present in the first revision of this file" — a baseline, not a join. Reports
are written for every login the configs ever named, including people no longer
in the export, which is where departures live.

## 7. Audit-log history, for the recent window

Only the audit log knows about actions taken outside CLOWarden. Build the login
list from the export — no API call — then sweep:

```sh
./target/release/gh-org-members --audit-logins results/people.yaml -o results/audit-logins.csv
./target/release/gh-org-members --audit-logins results/people.yaml \
  --audit-select org-members-without-teams -o results/audit-logins-noteam.csv

./fetch-audit-history.zsh results/audit-logins-noteam.csv results/audit-history
```

Two queries per login (`user:` for what was done to them, `actor:` for what they
did), merged and de-duplicated on `_document_id`; the `relation` column says
which. One CSV and one raw JSON per login.

Budget: the audit-log endpoint has its own rate limit (1,750/hour on the
reference run), separate from the GraphQL and core buckets, so this does not
compete with exports. Two requests per login plus pagination — a list of 800 is
about 1,600 requests, which is why `--audit-select` exists.

## 8. Reconcile

```sh
./clowarden-drift.py --people results/people.yaml --out reports
```

`reports/drift.csv` and `drift.json`, one record per disagreement, in four
kinds. What each means in practice:

| Kind | Reading | Usually |
| --- | --- | --- |
| `on_github_not_in_config` | Access nobody declared | **The one to investigate.** Pre-CLOWarden state never reconciled, or access granted outside it |
| `in_config_not_on_github` | Declared, never applied | Often one unpopulated team repeated across its members — count teams, not rows |
| `role_mismatch` | Role differs | Check the direction first; a single direction across many logins is a pattern, not decisions |
| `team_not_in_config` | The team itself is unmanaged | An org whose config declares only a staff team |

Read it by concentration, not by total. On the reference run: 231 findings,
but 50 of the 52 undeclared memberships were one organization, 21 of the 84
unapplied declarations were one team, and 54 of the 90 role mismatches were two
staff accounts — leaving perhaps three things to actually look at.

## Gotchas, all learned the hard way

- **`GITHUB_TOKEN=$GITHUB_TOKEN cmd` defeats the `gh` fallback.** Use `env -u`.
- **A PAT's blind spot is silent.** Check `totals.organizations` against the
  enterprise every time; `enterprise_members_without_org` is the tell.
- **`email` costs a scope.** GitHub refuses the whole query when the token
  lacks `read:user`, so `gh-org-members` names the field only under
  `--include-email`.
- **`git log --follow --reverse` does not compose.** Git answers a couple of
  commits instead of the file's history — 2 where one repo had 131. Ask for the
  path plainly and reverse in the caller.
- **A repo may have had several config files.** One migrated
  `access-control.yaml` → `config.yaml`; following only the current name lost
  the earlier era. Walk every config path the history contains.
- **Configs grant collaborators too**, not just team membership.
- **The audit log serves ~180 days.** An empty per-login file means the window
  is empty, not that nothing happened. Use the config git history for anything
  older.
- **Audit `user:` ≠ `actor:`.** Most people are only ever the target; querying
  both doubles the cost and, for an inactive cohort, returns nothing extra.
- **`integration_installation.repositories_added` under `actor:clowarden[bot]`
  is CLOWarden managing *other* apps' installations** (look at `integration`),
  not its own watch list.
- **Config history is intent, not state.** Live team membership can include
  teams the config never mentions; that is exactly what step 8 measures.

## Output layout

```
gh-org-members/
  results/            # exports and audit captures (gitignored)
  mirror/<org>/<repo> # CLOWarden config checkouts (gitignored)
  reports/
    <login>/<login>.{json,csv}   # per-login history, all orgs merged
    drift.{csv,json,md}          # reconciliation
gh-enterprise-repos/
  reports/            # repository exports, YAML + JSON (gitignored)
```

Everything under `results/`, `mirror/` and `reports/` names real people and
private repositories, and is gitignored in both repositories. Keep it that way.
