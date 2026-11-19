#!/usr/bin/env bash
# Print the release notes for TAG: the "## Release notes" sections of all
# pull requests merged since the previous v* tag (by version order), PRs that
# say "None" left out. Needs GH_TOKEN and gh.
#
#   collect-release-notes.sh <tag> <owner/repo>
set -euo pipefail

TAG="$1"
REPO="$2"
HERE="$(cd "$(dirname "$0")" && pwd)"
SERVER="${GITHUB_SERVER_URL:-https://github.com}"

# Previous tag: the next lower v* tag in version order.
PREV="$(git ls-remote --tags --refs "$SERVER/$REPO.git" 'v*' 2>/dev/null \
  | sed 's#.*refs/tags/##' | { cat; echo "$TAG"; } | sort -uV \
  | awk -v t="$TAG" '$0 == t { print prev; exit } { prev = $0 }')"

PRS=""
if [ -n "$PREV" ]; then
  for sha in $(gh api "repos/$REPO/compare/$PREV...$TAG" --paginate --jq '.commits[].sha'); do
    PRS="$PRS$(gh api "repos/$REPO/commits/$sha/pulls" --jq '.[] | select(.merged_at != null) | .number')"$'\n'
  done
fi

NOTES=""
for pr in $(printf '%s' "$PRS" | sed '/^$/d' | sort -un); do
  section="$(gh pr view "$pr" --repo "$REPO" --json body --jq '.body // ""' | "$HERE/release-notes-section.sh")"
  case "$(printf '%s' "$section" | tr -d '[:space:]' | tr '[:upper:]' '[:lower:]')" in
    ''|none|none.|n/a|-) continue ;;
  esac
  NOTES="$NOTES$section"$'\n'
done

if [ -n "$NOTES" ]; then
  printf '%s' "$NOTES"
else
  echo "Maintenance release."
fi
if [ -n "$PREV" ]; then
  printf '\n**Full Changelog**: %s/%s/compare/%s...%s\n' "$SERVER" "$REPO" "$PREV" "$TAG"
fi
