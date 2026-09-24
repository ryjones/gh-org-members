#!/usr/bin/env zsh
# Fetch enterprise audit-log org/team membership history for each login in a
# CSV (first column, header skipped) and write one CSV per login.
#
# Each login is queried twice: once as the target of an action (user:<login>)
# and once as the one who performed it (actor:<login>). The two result sets are
# merged and de-duplicated, and each row says which of the two it was.
#
#   ./fetch-audit-history.zsh [input.csv] [outdir]
#
# Needs a gh token with read:audit_log (or admin:enterprise) on the enterprise:
#   gh auth refresh -h github.com -s read:audit_log
set -euo pipefail

ENTERPRISE=${ENTERPRISE:-lf-decentralized-trust}
INPUT=${1:-enterprise-members-without-org.csv}
OUTDIR=${2:-results/audit-history}

mkdir -p "$OUTDIR"

# Membership-relevant events only: org.*, team.*, business.* (enterprise).
FILTER='^(org|team|business)\.'

to_csv='
  ["timestamp","action","relation","actor","org","team","user","permission"],
  ( map(select(.action | test($f)))
    | sort_by(.["@timestamp"] // .created_at)
    | .[]
    | . as $e
    | ($e.user // $e.user_login // "") as $target
    | [ ((($e["@timestamp"] // $e.created_at) // 0) / 1000 | todate),
        ($e.action // ""),
        (if $e.actor == $login and $target == $login then "self"
         elif $e.actor == $login then "actor"
         else "target" end),
        ($e.actor // ""), ($e.org // ""), ($e.team // ""),
        $target, ($e.permission // $e.role // "") ] )
  | @csv'

fetch() {  # fetch <phrase> -> JSON array on stdout
  gh api --paginate \
    -H "Accept: application/vnd.github+json" \
    "/enterprises/$ENTERPRISE/audit-log?phrase=$1&per_page=100&order=asc" \
    --jq '.' | jq -s 'add // []'
}

logins=("${(@f)$(tail -n +2 "$INPUT" | cut -d, -f1 | tr -d '"' | grep -v '^$')}")
print -r -- "${#logins} logins from $INPUT -> $OUTDIR/ (as target and as actor)"

total=0
for login in $logins; do
  raw="$OUTDIR/$login.json"
  csv="$OUTDIR/$login-audit-history.csv"
  # Two queries; an event where they acted on themselves comes back in both, so
  # de-duplicate on the audit entry's document id.
  jq -s 'add | unique_by(._document_id // tojson)' \
    =(fetch "user:$login") =(fetch "actor:$login") > "$raw"
  jq -r --arg f "$FILTER" --arg login "$login" "$to_csv" "$raw" > "$csv"
  events=$(( $(grep -c '^' "$csv") - 1 ))
  all=$(jq 'length' "$raw")
  as_actor=$(tail -n +2 "$csv" | cut -d, -f3 | grep -c '"\(actor\|self\)"' || true)
  total=$(( total + events ))
  print -r -- "$login -- $events membership events ($as_actor by them), $all audit entries -> $csv"
done
print -r -- "done: $total membership events across ${#logins} logins"
