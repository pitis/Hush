#!/usr/bin/env bash
# scripts/card.sh <issue number> <status>
# Moves the issue's card on the Hush board (https://github.com/users/pitis/projects/2).
# Projects need the keyring login: GH_TOKEN/GITHUB_TOKEN (env tokens without project access)
# would take precedence, so both are blanked.
set -euo pipefail
[ $# -eq 2 ] || { echo 'usage: scripts/card.sh <issue> "Todo|In progress|In review|Done|Needs decision"' >&2; exit 2; }
issue=$1 status=$2
[[ $issue =~ ^[0-9]+$ ]] || { echo "✗ issue must be a number (got: $issue)" >&2; exit 2; }
PROJECT=2 OWNER=pitis REPO=pitis/hush PROJECT_ID=PVT_kwHOAkXkrM4BnMZS FIELD_ID=PVTSSF_lAHOAkXkrM4BnMZSzhlyH_U
ghp() { GH_TOKEN= GITHUB_TOKEN= gh "$@"; }
option=$(ghp project field-list $PROJECT --owner $OWNER --format json |
  jq -r --arg f "$FIELD_ID" --arg s "$status" '.fields[] | select(.id==$f) | .options[] | select(.name==$s) | .id')
[ -n "$option" ] || { echo "✗ unknown status: $status" >&2; exit 2; }
items=$(ghp project item-list $PROJECT --owner $OWNER --format json --limit 1000 |
  jq -r --argjson n "$issue" --arg r "$REPO" '.items[] | select(.content.number==$n and (.content.repository|ascii_downcase)==($r|ascii_downcase)) | .id')
case $(grep -c . <<<"$items" || true) in
  0) echo "✗ issue #$issue is not on the board" >&2; exit 2 ;;
  1) ;;
  *) echo "✗ issue #$issue matches several board items" >&2; exit 2 ;;
esac
ghp project item-edit --project-id $PROJECT_ID --id "$items" --field-id $FIELD_ID --single-select-option-id "$option" >/dev/null
echo "✓ #$issue → $status"
