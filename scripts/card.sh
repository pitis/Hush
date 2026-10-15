#!/usr/bin/env bash
# scripts/card.sh <issue number> <status>
# Moves the issue's card on the Semaphore board (https://github.com/users/pitis/projects/2).
# Projects need the keyring login, hence GITHUB_TOKEN= (an env token lacks project access).
set -euo pipefail
issue=$1 status=$2
PROJECT=2 OWNER=pitis PROJECT_ID=PVT_kwHOAkXkrM4BnMZS FIELD_ID=PVTSSF_lAHOAkXkrM4BnMZSzhlyH_U
option=$(GITHUB_TOKEN= gh project field-list $PROJECT --owner $OWNER --format json \
  --jq ".fields[] | select(.id==\"$FIELD_ID\") | .options[] | select(.name==\"$status\") | .id")
[ -n "$option" ] || { echo "✗ unknown status: $status" >&2; exit 2; }
item=$(GITHUB_TOKEN= gh project item-list $PROJECT --owner $OWNER --format json --limit 200 \
  --jq ".items[] | select(.content.number==$issue) | .id")
[ -n "$item" ] || { echo "✗ issue #$issue is not on the board" >&2; exit 2; }
GITHUB_TOKEN= gh project item-edit --project-id $PROJECT_ID --id "$item" --field-id $FIELD_ID \
  --single-select-option-id "$option" >/dev/null
echo "✓ #$issue → $status"
