#!/usr/bin/env zsh
# Clone (or fetch, when already present) the CLOWarden config repositories into
# mirror/<org>/<repo>.
#
#   ./mirror-clowarden.zsh [watched-repos.csv] [mirror-dir] [jobs]
#
# The CSV is gh-enterprise-repos' clowarden-watched-repos.csv: its second column
# holds `<org>/<repo>`. Existing checkouts are fetched rather than re-cloned, so
# the script is safe to re-run.
set -uo pipefail

INPUT=${1:-../gh-enterprise-repos/reports/clowarden-watched-repos.csv}
MIRROR=${2:-mirror}
JOBS=${3:-8}

[[ -r $INPUT ]] || { print -ru2 -- "cannot read $INPUT"; exit 1 }
mkdir -p "$MIRROR"

repos=("${(@f)$(tail -n +2 "$INPUT" | cut -d, -f2 | tr -d '"' | grep -v '^$' | sort -u)}")
print -r -- "${#repos} repositories -> $MIRROR/"

# One line per repository as it lands, from a subshell per job, so a long run
# says what it is doing rather than going quiet.
mirror_one() {
  local slug=$1 mirror=$2
  local dir="$mirror/$slug"
  if [[ -d $dir/.git ]]; then
    if out=$(git -C "$dir" fetch --all --prune --quiet 2>&1); then
      print -r -- "$slug -- fetched"
    else
      print -r -- "$slug -- FETCH FAILED: ${out//$'\n'/ }"
    fi
  else
    mkdir -p "${dir:h}"
    if out=$(git clone --quiet "https://github.com/$slug.git" "$dir" 2>&1); then
      print -r -- "$slug -- cloned"
    else
      print -r -- "$slug -- CLONE FAILED: ${out//$'\n'/ }"
    fi
  fi
}

for slug in $repos; do
  mirror_one "$slug" "$MIRROR" &
  while (( $(jobs -r | wc -l) >= JOBS )); do wait -n; done
done
wait

cloned=$(find "$MIRROR" -maxdepth 3 -name .git -type d | wc -l | tr -d ' ')
print -r -- "done: $cloned checkout(s) under $MIRROR/"
