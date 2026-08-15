#!/usr/bin/env bash
# Verify that every non-merge commit in a range carries a Developer
# Certificate of Origin sign-off matching its author or committer.
#
# Usage:
#   scripts/check-dco.sh [<base>] [<head>]
#
# With no arguments the range is `origin/main..HEAD`, so contributors can
# run the same check locally that CI runs on a pull request:
#
#   scripts/check-dco.sh
#
# Fix a missing sign-off with:
#   git commit --amend -s          # last commit
#   git rebase --signoff <base>    # a whole branch

set -euo pipefail

base="${1:-}"
head="${2:-HEAD}"

if [ -z "$base" ]; then
  if git rev-parse --verify --quiet origin/main >/dev/null; then
    base="origin/main"
  else
    base="main"
  fi
fi

range="$base..$head"
commits="$(git rev-list --no-merges "$range")"

if [ -z "$commits" ]; then
  echo "DCO: no non-merge commits in $range — nothing to check."
  exit 0
fi

fail=0
count=0

for commit in $commits; do
  count=$((count + 1))
  subject="$(git show -s --format='%s' "$commit")"
  author_email="$(git show -s --format='%ae' "$commit" | tr '[:upper:]' '[:lower:]')"
  committer_email="$(git show -s --format='%ce' "$commit" | tr '[:upper:]' '[:lower:]')"

  # Collect the e-mail address of every Signed-off-by trailer.
  signoff_emails="$(
    git show -s --format='%B' "$commit" \
      | grep -i '^[[:space:]]*Signed-off-by:' \
      | sed -n 's/.*<\(.*\)>.*/\1/p' \
      | tr '[:upper:]' '[:lower:]' || true
  )"

  if [ -z "$signoff_emails" ]; then
    echo "FAIL ${commit:0:12}  missing Signed-off-by  — $subject"
    fail=$((fail + 1))
    continue
  fi

  matched=0
  for email in $signoff_emails; do
    if [ "$email" = "$author_email" ] || [ "$email" = "$committer_email" ]; then
      matched=1
      break
    fi
  done

  if [ "$matched" -eq 1 ]; then
    echo "ok   ${commit:0:12}  $subject"
  else
    echo "FAIL ${commit:0:12}  sign-off does not match author <$author_email> — $subject"
    echo "                 sign-off found: $(echo "$signoff_emails" | tr '\n' ' ')"
    fail=$((fail + 1))
  fi
done

echo
if [ "$fail" -gt 0 ]; then
  echo "DCO: $fail of $count commit(s) in $range are not signed off."
  echo
  echo "Every commit must certify the Developer Certificate of Origin."
  echo "Add the sign-off and push again:"
  echo "  git commit --amend -s        # last commit only"
  echo "  git rebase --signoff $base   # every commit on this branch"
  exit 1
fi

echo "DCO: all $count commit(s) in $range are signed off."
