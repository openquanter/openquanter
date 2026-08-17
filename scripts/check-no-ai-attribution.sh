#!/usr/bin/env bash
# Verify that no commit in a range credits an AI assistant as an author.
#
# Usage:
#   scripts/check-no-ai-attribution.sh [<base>] [<head>]
#   scripts/check-no-ai-attribution.sh --self-test
#
# With no arguments the range is `origin/main..HEAD`, so contributors can
# run the same check locally that CI runs on a pull request.
#
# Why this is a check and not a convention: the tools that write these
# commits are configured to add the trailer by default, one working copy
# at a time. A rule that each session has to remember is a rule that
# holds until the next session, and the evidence that it does not hold is
# in this repository's own history. A check does not have to remember.
#
# What it matches, and what it must not
# -------------------------------------
# Only the *trailer* forms — a line that begins with `Co-authored-by:`
# naming an assistant, or the generated-with footer. Prose is left alone
# on purpose: `.claude/worktrees` is a real path in this repository's
# .gitignore, and "backtest reflection via Claude Code headless" is a
# commit subject describing an architecture. A pattern loose enough to
# catch those would make the check something people route around, and a
# check people route around is worse than none.
#
# Fix a commit that fails:
#   git commit --amend                 # last commit: delete the line
#   git rebase -i <base>               # earlier commits: reword each
#
# Or strip them from a whole branch in one pass:
#   git filter-branch -f --msg-filter \
#     "perl -0777 -pe 's/^Co-authored-by: (Claude|GPT|Copilot|Gemini).*(\n|\$)//gmi'" \
#     <base>..HEAD
#
# Note the `(\n|$)` in that filter rather than `\n`: the trailer is
# usually the last line of the message and the last line has no newline
# after it, so a pattern requiring one silently leaves it behind while
# reporting success.

set -uo pipefail

# Anchored at the start of a line, so only trailers match.
#
# Matched on the vendor's address or on a model designation, never on a
# bare given name. Claude is a name people have — Claude Monet had it —
# and a contributor called Claude Dupont writes a co-author line
# indistinguishable from an assistant's if you match the first word
# alone. Blocking that person is not a small cost: it is a stranger's
# first contribution refused by a machine that mistook their name for a
# robot, and the repository would never hear why they left.
PATTERNS=(
  # Definitive: no person sends mail from an assistant vendor's domain.
  '^Co-authored-by:.*<[^>]*@(anthropic|openai)\.com>'
  # A model designation, not the name on its own.
  '^Co-authored-by:[[:space:]]*Claude[[:space:]]+(Opus|Sonnet|Haiku|Fable|Code|[0-9])'
  # These are product names rather than given names, but still require a
  # separator after them so a surname beginning with one cannot match.
  '^Co-authored-by:[[:space:]]*(GPT|Codex|Copilot|Gemini|Cursor|Devin|Aider)[[:space:]—–-]'
  '^[[:space:]]*🤖[[:space:]]*Generated with'
)

self_test() {
  # A check that silently stopped matching is indistinguishable from a
  # clean history, so the patterns are exercised against samples that
  # must fail and samples that must pass.
  local must_match=(
    'Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>'
    'Co-authored-by: Claude Sonnet 4.6 <noreply@anthropic.com>'
    'Co-authored-by: Someone Else <noreply@anthropic.com>'
    '🤖 Generated with [Claude Code](https://claude.com/claude-code)'
    # No vendor address, so the model designation has to carry it.
    'Co-authored-by: Claude Haiku 4.5 <someone@example.com>'
    'Co-authored-by: GPT-5 <dev@example.com>'
  )
  # Real lines from this repository's history. Each names an assistant
  # and none is an attribution.
  local must_not_match=(
    'Also ignore .claude/worktrees, which is a peer session'"'"'s checkout'
    'feat(analysis): backtest reflection via Claude Code headless'
    'local does not run claude code; route LLM via ssh to the build host'
    'Co-authored-by: 0xdtee <312484298+0xdtee@users.noreply.github.com>'
    # People named Claude. The check must let them contribute.
    'Co-authored-by: Claude Dupont <claude.dupont@example.fr>'
    'Co-authored-by: Claude Monet <cmonet@giverny.example>'
    'Co-authored-by: Jean-Claude Martin <jc@example.fr>'
  )

  local failures=0 sample pattern hit
  for sample in "${must_match[@]}"; do
    hit=0
    for pattern in "${PATTERNS[@]}"; do
      printf '%s\n' "$sample" | grep -qEi -e "$pattern" && { hit=1; break; }
    done
    [ "$hit" -eq 1 ] || {
      echo "SELF-TEST FAIL: should have matched: $sample"
      failures=$((failures + 1))
    }
  done
  for sample in "${must_not_match[@]}"; do
    for pattern in "${PATTERNS[@]}"; do
      if printf '%s\n' "$sample" | grep -qEi -e "$pattern"; then
        echo "SELF-TEST FAIL: must not match: $sample"
        failures=$((failures + 1))
      fi
    done
  done

  if [ "$failures" -gt 0 ]; then
    echo "$failures self-test failure(s)"
    return 1
  fi
  echo "self-test: ${#must_match[@]} matched, ${#must_not_match[@]} correctly ignored"
  return 0
}

if [ "${1:-}" = "--self-test" ]; then
  self_test
  exit $?
fi

if ! self_test > /dev/null; then
  self_test
  echo "refusing to report a clean history from a broken check"
  exit 1
fi

base="${1:-}"
head="${2:-HEAD}"

if [ -z "$base" ]; then
  if git rev-parse --verify --quiet origin/main > /dev/null; then
    base="origin/main"
  else
    base="main"
  fi
fi

range="$base..$head"
commits="$(git rev-list --no-merges "$range")"

if [ -z "$commits" ]; then
  echo "attribution: no non-merge commits in $range — nothing to check."
  exit 0
fi

fail=0
count=0

for commit in $commits; do
  count=$((count + 1))
  subject="$(git show -s --format='%s' "$commit")"
  message="$(git show -s --format='%B' "$commit")"

  hits=""
  for pattern in "${PATTERNS[@]}"; do
    found="$(printf '%s\n' "$message" | grep -Ei -e "$pattern" || true)"
    [ -n "$found" ] && hits="${hits}${found}"$'\n'
  done

  if [ -z "$hits" ]; then
    echo "ok   ${commit:0:12}  $subject"
  else
    echo "FAIL ${commit:0:12}  $subject"
    printf '%s' "$hits" | sed 's/^/                 /'
    fail=$((fail + 1))
  fi
done

echo
if [ "$fail" -gt 0 ]; then
  echo "attribution: $fail of $count commit(s) in $range credit an assistant."
  echo
  echo "Commits in this repository are authored by people. Remove the"
  echo "trailer and push again:"
  echo "  git commit --amend           # last commit only"
  echo "  git rebase -i $base          # reword earlier commits"
  echo
  echo "See the header of this script for a filter that strips a whole"
  echo "branch in one pass, and for the newline trap in writing one."
  exit 1
fi

echo "attribution: all $count commit(s) in $range are attributed to people."
