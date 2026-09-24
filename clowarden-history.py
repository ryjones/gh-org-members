#!/usr/bin/env python3
"""Team membership history for every login, from the CLOWarden config repos.

CLOWarden applies a YAML file in each org's governance repository, so that
file's git history *is* the org's membership history -- further back than the
audit log's 180 days, and without spending an API call. This walks every
revision of every mirrored config, diffs consecutive revisions, and writes one
report per login.

    ./clowarden-history.py [--mirror DIR] [--people FILE] [--out DIR] [--jobs N]

YAML is parsed by shelling out to `yq`, which is already a dependency of the
surrounding tooling; no Python YAML module is required.
"""

import argparse
import collections
import concurrent.futures
import csv
import json
import pathlib
import subprocess
import sys

CONFIG_NAMES = ("config.yaml", "config.yml", "teams.yml", "teams.yaml", "access-control.yaml")


def run(args, **kwargs):
    return subprocess.run(args, capture_output=True, text=True, **kwargs)


def config_repos(mirror):
    """Every mirrored checkout, with the config file it carries."""
    for git_dir in sorted(mirror.glob("*/*/.git")):
        repo = git_dir.parent
        for name in CONFIG_NAMES:
            if (repo / name).exists():
                yield repo, name
                break


def revisions(repo, config):
    """(sha, iso date, author, path) oldest first, following renames."""
    out = run(["git", "-C", str(repo), "log", "--follow", "--reverse",
               "--format=@@@%H\t%aI\t%an", "--name-only", "--", config]).stdout
    revs, current = [], None
    for line in out.splitlines():
        if line.startswith("@@@"):
            sha, date, author = line[3:].split("\t", 2)
            current = [sha, date, author]
        elif line.strip() and current:
            revs.append((*current, line.strip()))
            current = None
    return revs


def snapshot(repo, sha, path, cache):
    """{(team, login): role} as of one revision, or None if it cannot be read."""
    blob = run(["git", "-C", str(repo), "rev-parse", f"{sha}:{path}"]).stdout.strip()
    if not blob:
        return None
    if blob in cache:
        return cache[blob]

    text = run(["git", "-C", str(repo), "cat-file", "blob", blob]).stdout
    parsed = run(["yq", "-o=json", "-"], input=text)
    if parsed.returncode != 0:
        cache[blob] = None  # Unparseable revision: skipped, not guessed at.
        return None
    try:
        data = json.loads(parsed.stdout or "null")
    except json.JSONDecodeError:
        cache[blob] = None
        return None

    members = {}
    teams = (data or {}).get("teams") or []
    if isinstance(teams, dict):  # some configs key teams by name
        teams = [{"name": name, **(body or {})} for name, body in teams.items()]
    for team in teams:
        if not isinstance(team, dict):
            continue
        name = team.get("name")
        if not name:
            continue
        for role, key in (("MAINTAINER", "maintainers"), ("MEMBER", "members")):
            for login in team.get(key) or []:
                if isinstance(login, str) and login.strip():
                    members[(str(name), login.strip().lower())] = (role, login.strip())
    cache[blob] = members
    return members


def repo_events(repo, config, quiet=False):
    """Every add, removal and role change the config's history describes."""
    org, name = repo.parent.name, repo.name
    events, previous, cache = [], None, {}
    revs = revisions(repo, config)
    for sha, date, author, path in revs:
        current = snapshot(repo, sha, path, cache)
        if current is None:
            continue
        if previous is not None:
            for key, (role, login) in current.items():
                if key not in previous:
                    events.append(dict(timestamp=date, org=org, repo=name, team=key[0],
                                       login=login, change="added", role=role,
                                       previous_role="", commit=sha[:12], author=author))
                elif previous[key][0] != role:
                    events.append(dict(timestamp=date, org=org, repo=name, team=key[0],
                                       login=login, change="role_changed", role=role,
                                       previous_role=previous[key][0], commit=sha[:12],
                                       author=author))
            for key, (role, login) in previous.items():
                if key not in current:
                    events.append(dict(timestamp=date, org=org, repo=name, team=key[0],
                                       login=login, change="removed", role="",
                                       previous_role=role, commit=sha[:12], author=author))
        else:
            # The first revision is the baseline: everyone in it starts there.
            for (team, _), (role, login) in current.items():
                events.append(dict(timestamp=date, org=org, repo=name, team=team,
                                   login=login, change="initial", role=role,
                                   previous_role="", commit=sha[:12], author=author))
        previous = current
    if not quiet:
        print(f"{org}/{name} -- {len(revs)} revision(s) of {config}, {len(events)} event(s)",
              flush=True)
    return events


def export_logins(path):
    """Logins and names from a gh-org-members export, in its own order."""
    parsed = run(["yq", "-o=json", str(path)])
    if parsed.returncode != 0:
        sys.exit(f"cannot read {path}: {parsed.stderr.strip()}")
    data = json.loads(parsed.stdout)
    return [(p["login"], p.get("name") or "",
             [f"{o['org']}/{t['slug']}" for o in p.get("organizations") or []
              for t in o.get("teams") or []])
            for p in data.get("people") or []]


FIELDS = ["timestamp", "org", "repo", "team", "change", "role", "previous_role",
          "commit", "author"]


def write_reports(people, by_login, out, source):
    out.mkdir(parents=True, exist_ok=True)
    with_history = 0
    for login, name, current_teams in people:
        events = sorted(by_login.get(login.lower(), []), key=lambda e: e["timestamp"])
        if events:
            with_history += 1
        folder = out / login
        folder.mkdir(parents=True, exist_ok=True)

        with (folder / f"{login}.csv").open("w", newline="") as fh:
            writer = csv.DictWriter(fh, fieldnames=FIELDS, extrasaction="ignore",
                                    quoting=csv.QUOTE_NONNUMERIC)
            writer.writeheader()
            writer.writerows(events)

        teams = sorted({f"{e['org']}/{e['team']}" for e in events})
        (folder / f"{login}.json").write_text(json.dumps({
            "login": login,
            "name": name,
            "source": source,
            "totals": {
                "events": len(events),
                "teams_seen": len(teams),
                "first_event": events[0]["timestamp"] if events else None,
                "last_event": events[-1]["timestamp"] if events else None,
            },
            "teams_now": current_teams,
            "teams_seen": teams,
            "events": events,
        }, indent=2) + "\n")
    return with_history


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--mirror", default="mirror", type=pathlib.Path)
    ap.add_argument("--people", default="results/lf-decentralized-trust.gh.yaml",
                    type=pathlib.Path)
    ap.add_argument("--out", default="reports", type=pathlib.Path)
    ap.add_argument("--jobs", default=8, type=int)
    args = ap.parse_args()

    repos = list(config_repos(args.mirror))
    if not repos:
        sys.exit(f"no mirrored config repositories under {args.mirror}/ "
                 f"-- run ./mirror-clowarden.zsh first")
    print(f"{len(repos)} config repositories under {args.mirror}/", flush=True)

    events = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
        for result in pool.map(lambda pair: repo_events(*pair), repos):
            events.extend(result)

    by_login = collections.defaultdict(list)
    for event in events:
        by_login[event["login"].lower()].append(event)

    people = export_logins(args.people)
    source = {
        "mirror": str(args.mirror),
        "people": str(args.people),
        "repositories": len(repos),
        "method": "git history of the CLOWarden config in each governance repo; no API calls",
    }
    with_history = write_reports(people, by_login, args.out, source)

    outside = sorted(set(by_login) - {login.lower() for login, _, _ in people})
    print(f"\n{len(events)} event(s) across {len(by_login)} login(s)")
    print(f"wrote {len(people)} report(s) to {args.out}/<login>/, "
          f"{with_history} with history")
    if outside:
        print(f"{len(outside)} login(s) appear in config history but not in the export "
              f"(left without a report)")


if __name__ == "__main__":
    main()
