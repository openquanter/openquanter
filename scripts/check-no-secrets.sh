#!/usr/bin/env bash
# Scan the working tree for content that must never enter a public
# repository: credentials, private keys, and deployment details.
#
# Usage:
#   scripts/check-no-secrets.sh             # scan tracked files
#   scripts/check-no-secrets.sh --history   # scan every commit as well
#   scripts/check-no-secrets.sh --self-test # prove the patterns fire
#
# Note on regex dialect: `git grep -E` is POSIX ERE and does **not**
# support `\b`. A pattern containing it matches nothing and fails
# silently, which is worse than having no check at all. That is what
# --self-test exists to catch.
#
# Two extension points, both intentionally kept out of this repository:
#
#   .secretscan-local   extra regexes, one per line. Deployment-specific
#                       terms — host names, internal domains, machine
#                       aliases — belong here, NOT in this script. A
#                       public deny-list of your own host names is itself
#                       a disclosure.
#
#                       The same reasoning covers a private strategy's
#                       identifiers: its parameter names, class names and
#                       characteristic constants are among the things the
#                       public/private split exists to keep. Listing them
#                       in a public script to guard them would publish
#                       exactly what the guard is for. They go here.
#
#   .secretscan-allow   regexes for known false positives.
#
# Both files are git-ignored.
#
# Local patterns are read from two places, in this order:
#
#   $(git rev-parse --git-common-dir)/secretscan-local
#       Lives inside .git, so it is shared by every worktree of the clone
#       and cannot be committed even deliberately — nothing under .git is
#       a path git will add. Prefer this one.
#   .secretscan-local
#       At the repository root. Git-ignored, but per-worktree, and one
#       `git add -f` away from being published — which for a file whose
#       whole content is the list of things that must not be published is
#       a poor place to keep it.

set -uo pipefail

MODE="${1:-tracked}"
ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

# High-signal patterns only. Anything noisy here trains people to ignore
# the check, which is worse than not having it.
PATTERNS=(
  'BEGIN (RSA|OPENSSH|EC|DSA|PGP) PRIVATE KEY'
  'ghp_[A-Za-z0-9]{30,}'
  'gho_[A-Za-z0-9]{30,}'
  'github_pat_[A-Za-z0-9_]{30,}'
  'glpat-[A-Za-z0-9_-]{20,}'
  'AKIA[0-9A-Z]{16}'
  'xox[baprs]-[A-Za-z0-9-]{10,}'
  'cio[A-Za-z0-9]{30,}'
  '-----BEGIN CERTIFICATE-----'
  '(api[_-]?key|secret[_-]?key|access[_-]?token|auth[_-]?token|password|passwd)[[:space:]]*[=:][[:space:]]*["'"'"'][^"'"'"'{$][^"'"'"']{7,}'
  '(([0-9]{1,3})\.){3}[0-9]{1,3}:[0-9]{2,5}'
  '[a-z0-9_-]+@(([0-9]{1,3})\.){3}[0-9]{1,3}'
  'ssh[[:space:]]+-[ip][[:space:]]'
  '\.ssh/id_(rsa|ed25519|ecdsa)'
)

# Hygiene, not disclosure — and the difference decides where they apply.
#
# Where something runs is not a secret the way a key is. A cloud region
# or a home directory in source is a fact a reader cannot change and an
# attacker does not have to guess, so it should not be added; but one
# already in history needs no rotation, cannot be removed without
# rewriting published history, and would fail this check forever on
# every commit that ever contained it.
#
# So these run against the working tree only. The patterns above, which
# match things that must be rotated the moment they appear anywhere,
# still run against history — because for those, "it is only in an old
# commit" is not a mitigation.
TREE_ONLY_PATTERNS=(
  '"(ap|us|eu|na|sa)-[a-z]+(-[0-9])?"'
  '/home/(ubuntu|ec2-user|admin)/'
)

load_local_patterns() {
  local file="$1" count=0 line
  [ -f "$file" ] || return 0
  while IFS= read -r line; do
    [ -z "$line" ] && continue
    [[ "$line" == \#* ]] && continue
    PATTERNS+=("$line")
    count=$((count + 1))
  done < "$file"
  # The count, never the patterns. Echoing them would put the private
  # list into every CI log that runs this.
  [ "$count" -gt 0 ] && echo "loaded $count local pattern(s) from ${file##*/}"
  return 0
}

load_local_patterns "$(git rev-parse --git-common-dir)/secretscan-local"
load_local_patterns .secretscan-local

self_test() {
  # Each sample must be flagged by at least one pattern. A scanner that
  # silently stopped matching is indistinguishable from a clean repo,
  # so this runs in CI alongside the scan itself.
  local samples=(
    'aws_key = "AKIAIOSFODNN7EXAMPLE"'
    'password: "correct-horse-battery"'
    'api_key = "abcdefghijklmnop"'
    'token=ghp_0123456789abcdefghijklmnopqrstuvwxyz'
    'gitlab: glpat-0123456789abcdefghij'
    'host 203.0.113.44:22022'
    'deploy@198.51.100.7'
    '-----BEGIN OPENSSH PRIVATE KEY-----'
    'key at ~/.ssh/id_ed25519'
  )
  local tree_samples=(
    'region="ap-somewhere"'
    'BIN=/home/ubuntu/thing'
  )

  local failures=0
  for sample in "${samples[@]}"; do
    local matched=0
    for pattern in "${PATTERNS[@]}"; do
      if printf '%s\n' "$sample" | grep -qE -i -e "$pattern"; then
        matched=1
        break
      fi
    done
    if [ "$matched" -eq 0 ]; then
      echo "SELF-TEST FAIL: no pattern matches: $sample"
      failures=$((failures + 1))
    fi
  done

  for sample in "${tree_samples[@]}"; do
    local matched=0
    for pattern in "${TREE_ONLY_PATTERNS[@]}"; do
      if printf '%s\n' "$sample" | grep -qE -i -e "$pattern"; then
        matched=1
        break
      fi
    done
    if [ "$matched" -eq 0 ]; then
      echo "SELF-TEST FAIL: no tree-only pattern matches: $sample"
      failures=$((failures + 1))
    fi
  done

  # A loader that silently reads nothing is the same failure as a
  # pattern that silently matches nothing, and it fails in the same
  # direction: a clean report from a scanner that never loaded the list
  # of private terms is indistinguishable from a clean repository. The
  # probe runs in a subshell so it cannot leave anything in PATTERNS.
  local added
  added=$(
    probe="$(mktemp)"
    printf '# a comment\n\nZZ_LOADER_PROBE_[0-9]+\n' > "$probe"
    before=${#PATTERNS[@]}
    load_local_patterns "$probe" > /dev/null
    echo $(( ${#PATTERNS[@]} - before ))
    rm -f "$probe"
  )
  if [ "$added" != "1" ]; then
    echo "SELF-TEST FAIL: local pattern loader took $added of 1 pattern"
    failures=$((failures + 1))
  fi

  if [ "$failures" -gt 0 ]; then
    echo "$failures sample(s) were not detected"
    return 1
  fi
  echo "self-test: ${#samples[@]} + ${#tree_samples[@]} samples detected by ${#PATTERNS[@]} + ${#TREE_ONLY_PATTERNS[@]} patterns"
  return 0
}

allowed() {
  [ -f .secretscan-allow ] || return 1
  grep -qE -f .secretscan-allow <<< "$1"
}

scan_target() {
  local label="$1" hits=0
  shift
  local -a set=("${PATTERNS[@]}")
  if [ "$label" = "working tree" ]; then
    set+=("${TREE_ONLY_PATTERNS[@]}")
  fi
  for pattern in "${set[@]}"; do
    while IFS= read -r hit; do
      [ -z "$hit" ] && continue
      allowed "$hit" && continue
      echo "FAIL [$label] $hit"
      hits=$((hits + 1))
    done < <("$@" "$pattern" 2>/dev/null | grep -v 'scripts/check-no-secrets.sh' | sort -u | head -20)
  done
  return "$hits"
}

grep_tracked() {
  # -e matters: several patterns begin with a dash and would otherwise
  # be parsed as options.
  git grep -I -n -E -i -e "$1" -- . ':(exclude)scripts/check-no-secrets.sh'
}

grep_history() {
  git grep -I -n -E -i -e "$1" $(git rev-list --all) -- . 2>/dev/null \
    | grep -v 'scripts/check-no-secrets.sh'
}

if [ "$MODE" = "--self-test" ]; then
  self_test
  exit $?
fi

if ! self_test; then
  echo "refusing to report a clean scan from a broken scanner"
  exit 1
fi

total=0

scan_target "working tree" grep_tracked
total=$((total + $?))

if [ "$MODE" = "--history" ]; then
  scan_target "history" grep_history
  total=$((total + $?))
fi

echo
if [ "$total" -gt 0 ]; then
  cat <<'MSG'
Secrets or deployment details found.

If this is a false positive, add a regex to .secretscan-allow (git-ignored).
If it is real, do not just delete the line: a committed secret is a
disclosed secret. Rotate the credential first, then remove it.
MSG
  exit 1
fi

echo "no secrets or deployment details found"
