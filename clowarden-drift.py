#!/usr/bin/env python3
"""Reconcile the CLOWarden configs against what GitHub actually shows.

CLOWarden's config is a statement of intent; an export is the state. Where the
two disagree, someone holds access nobody declared, or a declaration never took
effect. This compares the current revision of every mirrored config against an
export and reports each disagreement, so the ones worth looking into are named
rather than counted.

    ./clowarden-drift.py [--mirror DIR] [--people FILE] [--out DIR]

No API calls: both sides are files already on disk.
"""

import argparse
import collections
import csv
import json
import pathlib
import re
import subprocess
import sys

CONFIG_NAMES = ("config.yaml", "config.yml", "teams.yml", "teams.yaml", "access-control.yaml")


def run(args, **kwargs):
    return subprocess.run(args, capture_output=True, text=True, **kwargs)


def slug(name):
    """GitHub's team slug, near enough: lowercase, runs of punctuation to one dash."""
    return re.sub(r"-+", "-", re.sub(r"[^a-z0-9]+", "-", str(name).lower())).strip("-")


def read_yaml(path):
    parsed = run(["yq", "-o=json", str(path)])
    if parsed.returncode != 0:
        return None
    try:
        return json.loads(parsed.stdout or "null")
    except json.JSONDecodeError:
        return None


def config_state(mirror):
    """{(org, team_slug): {login_lower: (role, login)}} from the current configs."""
    state, orgs = {}, []
    for git_dir in sorted(mirror.glob("*/*/.git")):
        repo = git_dir.parent
        config = next((repo / n for n in CONFIG_NAMES if (repo / n).exists()), None)
        if config is None:
            continue
        data = read_yaml(config)
        if data is None:
            print(f"warning: cannot parse {config}", file=sys.stderr)
            continue
        org = repo.parent.name
        orgs.append(org)
        for team in data.get("teams") or []:
            if not isinstance(team, dict) or not team.get("name"):
                continue
            key = (org.lower(), slug(team["name"]))
            members = state.setdefault(key, {})
            for role, field in (("MAINTAINER", "maintainers"), ("MEMBER", "members")):
                for login in team.get(field) or []:
                    if isinstance(login, str) and login.strip():
                        members[login.strip().lower()] = (role, login.strip())
    return state, orgs


def github_state(people_file):
    """{(org, team_slug): {login_lower: (role, login)}} from an export."""
    data = read_yaml(people_file)
    if data is None:
        sys.exit(f"cannot read {people_file}")
    state = {}
    for person in data.get("people") or []:
        login = person["login"]
        for org in person.get("organizations") or []:
            for team in org.get("teams") or []:
                key = (org["org"].lower(), slug(team["slug"]))
                state.setdefault(key, {})[login.lower()] = (
                    team.get("role") or "MEMBER", login)
    return state, data


def reconcile(config, github, managed_orgs):
    """Every disagreement, as one record each."""
    managed = {org.lower() for org in managed_orgs}
    rows = []
    for key in sorted(set(config) | set(github)):
        org, team = key
        if org not in managed:
            continue  # no config to disagree with
        declared, actual = config.get(key), github.get(key)
        team_in_config = key in config

        for login_lower, (role, login) in (actual or {}).items():
            if not team_in_config:
                kind = "team_not_in_config"
            elif login_lower not in declared:
                kind = "on_github_not_in_config"
            elif declared[login_lower][0] != role:
                kind = "role_mismatch"
            else:
                continue
            rows.append(dict(org=org, team=team, login=login, kind=kind,
                             github_role=role,
                             config_role=(declared or {}).get(login_lower, ("", ""))[0]))

        for login_lower, (role, login) in (declared or {}).items():
            if not actual or login_lower not in actual:
                rows.append(dict(org=org, team=team, login=login,
                                 kind="in_config_not_on_github",
                                 github_role="", config_role=role))
    rows.sort(key=lambda r: (r["org"].lower(), r["team"], r["login"].lower()))
    return rows


FIELDS = ["login", "org", "team", "kind", "github_role", "config_role"]


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--mirror", default="mirror", type=pathlib.Path)
    ap.add_argument("--people", default="results/lf-decentralized-trust.gh.yaml",
                    type=pathlib.Path)
    ap.add_argument("--out", default="reports", type=pathlib.Path)
    args = ap.parse_args()

    config, managed_orgs = config_state(args.mirror)
    github, export = github_state(args.people)
    print(f"{len(managed_orgs)} organizations with a config, "
          f"{len(config)} teams declared, {len(github)} teams on GitHub", flush=True)

    rows = reconcile(config, github, managed_orgs)
    by_kind = collections.Counter(r["kind"] for r in rows)
    by_login = collections.Counter(r["login"] for r in rows)
    by_org = collections.Counter(r["org"] for r in rows)
    unmanaged = sorted({org for org in export.get("organizations") or []
                        if org.lower() not in {o.lower() for o in managed_orgs}})

    args.out.mkdir(parents=True, exist_ok=True)
    with (args.out / "drift.csv").open("w", newline="") as fh:
        writer = csv.DictWriter(fh, fieldnames=FIELDS, extrasaction="ignore",
                                quoting=csv.QUOTE_NONNUMERIC)
        writer.writeheader()
        writer.writerows(rows)

    (args.out / "drift.json").write_text(json.dumps({
        "source": {
            "mirror": str(args.mirror),
            "people": str(args.people),
            "authenticated_as": (export.get("source") or {}).get("authenticated_as"),
            "organizations_with_config": len(managed_orgs),
            "organizations_without_config": unmanaged,
            "method": "current config revision vs export; no API calls",
        },
        "totals": {
            "findings": len(rows),
            "logins": len(by_login),
            **{kind: count for kind, count in sorted(by_kind.items())},
        },
        "by_org": dict(by_org.most_common()),
        "by_login": dict(by_login.most_common()),
        "findings": rows,
    }, indent=2) + "\n")

    for kind, count in by_kind.most_common():
        print(f"{kind}: {count}")
    print(f"wrote {args.out}/drift.csv and {args.out}/drift.json "
          f"({len(rows)} findings across {len(by_login)} logins)")


if __name__ == "__main__":
    main()
