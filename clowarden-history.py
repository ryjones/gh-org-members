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
import re
import subprocess
import sys

CONFIG_NAMES = ("config.yaml", "config.yml", "teams.yml", "teams.yaml", "access-control.yaml")


def run(args, **kwargs):
    return subprocess.run(args, capture_output=True, text=True, **kwargs)


CONFIG_RE = re.compile(r"^(config|teams|access-control)\.ya?ml$", re.I)


def config_repos(mirror):
    """Every mirrored checkout, with every config path in its history.

    A repo that migrated -- hyperledger-labs went from access-control.yaml to
    config.yaml in 2024 -- has history under both names, and following only the
    current one loses everything before the move.
    """
    for git_dir in sorted(mirror.glob("*/*/.git")):
        repo = git_dir.parent
        seen = run(["git", "-C", str(repo), "log", "--all", "--name-only",
                    "--format="]).stdout.splitlines()
        paths = sorted({line.strip() for line in seen if CONFIG_RE.match(line.strip())})
        for path in paths:
            yield repo, path


def revisions(repo, config):
    """(sha, iso date, author, path) oldest first, for one config path.

    Deliberately no `--follow`: it does not compose with `--reverse` (git
    quietly returns a couple of commits instead of the file's history), and it
    is not needed here because every config path a repo has ever had is walked
    separately, so a rename is covered by both names rather than by git
    guessing at one.
    """
    out = run(["git", "-C", str(repo), "log", "--format=%H\t%aI\t%an",
               "--", config]).stdout
    revs = []
    for line in out.splitlines():
        if not line.strip():
            continue
        sha, date, author = line.split("\t", 2)
        revs.append((sha, date, author, config))
    revs.reverse()  # oldest first, so each revision diffs against the one before
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
    data = data or {}

    # Per-repository external collaborators: a list of logins, or a map of
    # login to permission.
    for entry in data.get("repositories") or []:
        if not isinstance(entry, dict):
            continue
        repo_name = entry.get("name")
        collaborators = entry.get("collaborators")
        if not repo_name or not collaborators:
            continue
        pairs = (collaborators.items() if isinstance(collaborators, dict)
                 else [(login, "COLLABORATOR") for login in collaborators])
        for login, permission in pairs:
            if isinstance(login, str) and login.strip():
                members[("collaborator", str(repo_name), login.strip().lower())] = (
                    str(permission).upper(), login.strip())

    teams = data.get("teams") or []
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
                    members[("team", str(name), login.strip().lower())] = (role, login.strip())
    cache[blob] = members
    return members


def repo_events(repo, config, quiet=False):
    """Every add, removal and role change the config's history describes."""
    org, name = repo.parent.name, repo.name

    def event(date, key, change, role, previous_role, login, sha, author):
        kind, target = key[0], key[1]
        return dict(timestamp=date, org=org, repo=name, kind=kind, target=target,
                    login=login, change=change, role=role, previous_role=previous_role,
                    commit=sha[:12], author=author, source_file=config)
    events, previous, cache = [], None, {}
    revs = revisions(repo, config)
    for sha, date, author, path in revs:
        current = snapshot(repo, sha, path, cache)
        if current is None:
            continue
        if previous is not None:
            for key, (role, login) in current.items():
                if key not in previous:
                    events.append(event(date, key, "added", role, "", login, sha, author))
                elif previous[key][0] != role:
                    events.append(event(date, key, "role_changed", role,
                                        previous[key][0], login, sha, author))
            for key, (role, login) in previous.items():
                if key not in current:
                    events.append(event(date, key, "removed", "", role, login, sha, author))
        else:
            # The first revision of this file is a baseline, not a join: a repo
            # that migrated config files starts its new file with everyone in it.
            for key, (role, login) in current.items():
                events.append(event(date, key, "initial", role, "", login, sha, author))
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


FIELDS = ["timestamp", "org", "repo", "kind", "target", "change", "role",
          "previous_role", "commit", "author", "source_file"]


def write_reports(people, by_login, out, source):
    """One report per login, merged across every org's config."""
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

        teams = sorted({f"{e['org']}/{e['target']}" for e in events if e["kind"] == "team"})
        repos = sorted({f"{e['org']}/{e['target']}" for e in events
                        if e["kind"] == "collaborator"})

        # One block per org, so a login that moved from one org to another reads
        # as a move rather than as two unrelated piles of events.
        orgs = {}
        for e in events:
            block = orgs.setdefault(e["org"], {
                "first_event": e["timestamp"], "last_event": e["timestamp"],
                "events": 0, "teams": set(),
            })
            block["events"] += 1
            block["last_event"] = e["timestamp"]
            if e["kind"] == "team":
                block["teams"].add(e["target"])
        timeline = [
            {"org": org, **{k: v for k, v in block.items() if k != "teams"},
             "teams": sorted(block["teams"])}
            for org, block in sorted(orgs.items(), key=lambda kv: kv[1]["first_event"])
        ]

        (folder / f"{login}.json").write_text(json.dumps({
            "login": login,
            "name": name,
            "source": source,
            "totals": {
                "events": len(events),
                "organizations": len(orgs),
                "teams_seen": len(teams),
                "repositories_as_collaborator": len(repos),
                "first_event": events[0]["timestamp"] if events else None,
                "last_event": events[-1]["timestamp"] if events else None,
            },
            "organizations": timeline,
            "teams_now": current_teams,
            "teams_seen": teams,
            "repositories_as_collaborator": repos,
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
    # Everyone the configs ever named gets a report, not only the people an
    # export happens to hold now: a login that left is exactly the history
    # worth keeping.
    known = {login.lower() for login, _, _ in people}
    for login in sorted(by_login):
        if login not in known:
            people.append((by_login[login][0]["login"], "", []))
    source = {
        "mirror": str(args.mirror),
        "people": str(args.people),
        "repositories": len(repos),
        "method": "git history of the CLOWarden config in each governance repo; no API calls",
    }
    with_history = write_reports(people, by_login, args.out, source)

    print(f"\n{len(events)} event(s) across {len(by_login)} login(s)")
    print(f"wrote {len(people)} report(s) to {args.out}/<login>/, "
          f"{with_history} with history")
    print(f"{len(people) - len(known)} of them are logins the export no longer holds")


if __name__ == "__main__":
    main()
