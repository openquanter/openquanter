#!/usr/bin/env bash
# Scan the working tree for content that must never enter a public
# repository: credentials, private keys, and deployment details.
#
# Usage:
#   scripts/check-no-secrets.sh             # scan tracked files
#   scripts/check-no-secrets.sh --history   # scan every commit as well
#   scripts/check-no-secrets.sh --self-test # prove the patterns fire
#
# Note on regex dialect: patterns are PCRE (`git grep -P`), not POSIX
# ERE. ERE has no literal prefilter for a case-insensitive alternation,
# and one such pattern cost more than every other pattern together when
# run over the whole history. A pattern the engine reads differently
# than its author meant matches nothing and fails silently, which is
# worse than having no check at all; that is what --self-test exists to
# catch, and it runs the samples through the same engine as the scan.
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
# Both files are git-ignored, and the scan fails outright if either is
# tracked: a committed allow-list is a way to switch the check off from
# inside the change it is checking, and a committed local list is the
# disclosure the file exists to prevent.
#
# Both are read from two places, in this order:
#
#   $(git rev-parse --git-common-dir)/secretscan-local (and -allow)
#       Lives inside .git, so it is shared by every worktree of the clone
#       and cannot be committed even deliberately — nothing under .git is
#       a path git will add. Prefer this one.
#   .secretscan-local (and .secretscan-allow)
#       At the repository root. Git-ignored, but per-worktree, and one
#       `git add -f` away from being published — which for a file whose
#       whole content is the list of things that must not be published is
#       a poor place to keep it.
#
# The scan fails closed. Binary-looking files are read as text, so a
# committed .gitattributes cannot hide a file by calling it binary; the
# script excludes itself by path, never by what a matching line says;
# and a git grep that errors out stops the scan instead of reading as
# "no match".

set -uo pipefail

MODE="${1:-tracked}"
ROOT="$(git rev-parse --show-toplevel)" || exit 2
cd "$ROOT" || exit 2

# High-signal patterns only. Anything noisy here trains people to ignore
# the check, which is worse than not having it.
PATTERNS=(
  'BEGIN ([A-Z0-9]+ )*PRIVATE KEY'
  'ghp_[A-Za-z0-9]{30,}'
  'gho_[A-Za-z0-9]{30,}'
  'github_pat_[A-Za-z0-9_]{30,}'
  'glpat-[A-Za-z0-9_-]{20,}'
  'AKIA[0-9A-Z]{16}'
  'xox[baprs]-[A-Za-z0-9-]{10,}'
  'cio[A-Za-z0-9]{30,}'
  '-----BEGIN CERTIFICATE-----'
  '(api[_-]?key|secret[_-]?key|access[_-]?token|auth[_-]?token|password|passwd|passphrase)[[:space:]]*[=:][[:space:]]*["'"'"'][^"'"'"'{$][^"'"'"']{7,}'
  # An .env line: no quotes, no spaces, a value that is not a reference.
  '(^|[^a-z0-9_])[a-z0-9_]*(key|secret|token|password|passwd|passphrase)[a-z0-9_]*=[^[:space:]$"'"'"'{<(]{8,}'
  # A raw 32-byte key — the shape of an EVM wallet or an Ed25519 seed —
  # quoted on a line that names it as a key.
  '(key|secret|priv|seed|wallet)[^"'"'"']{0,40}["'"'"'](0x)?[0-9a-f]{64}["'"'"']'
  '(^|[^a-z0-9])sk-(proj-|ant-|live-)?[a-z0-9_-]{24,}'
  'hooks\.slack\.com/services/'
  'discord(app)?\.com/api/webhooks/'
  '(([0-9]{1,3})\.){3}[0-9]{1,3}:[0-9]{2,5}'
  '[a-z0-9_-]+@(([0-9]{1,3})\.){3}[0-9]{1,3}'
  'ssh[[:space:]]+-[ip][[:space:]]'
  '\.ssh/id_(rsa|ed25519|ecdsa)'
)

# Keys that are public on purpose: the test vectors venues publish in
# their own SDKs, which our signing tests must reproduce exactly. Fixed
# strings, reviewed like any other line of this script — unlike the
# allow-list, which is local and invisible to review.
PUBLIC_TEST_VECTORS=(
  '0123456789012345678901234567890123456789012345678901234567890123'
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

ALLOW=()

# Appends each non-comment line of a file to the named array. By eval,
# not a nameref: the bash macOS ships (3.2) has no `local -n`.
load_list() {
  local into="$1" file="$2" count=0 line
  [ -f "$file" ] || return 0
  while IFS= read -r line; do
    [ -z "$line" ] && continue
    [[ "$line" == \#* ]] && continue
    eval "$into+=(\"\$line\")"
    count=$((count + 1))
  done < "$file"
  # The count, never the patterns. Echoing them would put the private
  # list into every CI log that runs this.
  [ "$count" -gt 0 ] && echo "loaded $count line(s) from ${file##*/}"
  return 0
}

GIT_DIR_COMMON="$(git rev-parse --git-common-dir)"
load_list PATTERNS "$GIT_DIR_COMMON/secretscan-local"
load_list PATTERNS .secretscan-local
load_list ALLOW "$GIT_DIR_COMMON/secretscan-allow"
load_list ALLOW .secretscan-allow

# Run this script against another repository without its self-test, so
# the self-test can prove the whole scan — not just the patterns — on a
# repository built to break it.
nested_scan() {
  (cd "$1" && OQ_SECRETSCAN_NESTED=1 bash "$ROOT/scripts/check-no-secrets.sh" > /dev/null 2>&1)
}

# Whether any of the patterns matches the file, asked of the engine the
# scan itself uses.
sample_matches() {
  local file="$1" pattern
  shift
  local -a exprs=()
  for pattern in "$@"; do
    exprs+=(-e "$pattern")
  done
  # --no-index only reads files below the current directory.
  (cd "${file%/*}" && git grep --no-index -q -P -i "${exprs[@]}" -- "${file##*/}")
}

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
    '-----BEGIN PRIVATE KEY-----'
    '-----BEGIN ENCRYPTED PRIVATE KEY-----'
    'key at ~/.ssh/id_ed25519'
    'OQ_VENUE_SECRET=Zx81kQ0pLmN2vB7c'
    'export API_PASSPHRASE=hunter2hunter2'
    'passphrase: "s3cr3t-phrase"'
    'let wallet_key = "0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318";'
    'OPENAI=sk-proj-abcdefghijklmnopqrstuvwxyz012345'
    'https://hooks.slack.com/services/T000/B000/XXXX'
  )
  local tree_samples=(
    'region="ap-somewhere"'
    'BIN=/home/ubuntu/thing'
  )

  local failures=0 probe
  probe="$(mktemp)"
  for sample in "${samples[@]}"; do
    printf '%s\n' "$sample" > "$probe"
    if ! sample_matches "$probe" "${PATTERNS[@]}"; then
      echo "SELF-TEST FAIL: no pattern matches: $sample"
      failures=$((failures + 1))
    fi
  done

  for sample in "${tree_samples[@]}"; do
    printf '%s\n' "$sample" > "$probe"
    if ! sample_matches "$probe" "${TREE_ONLY_PATTERNS[@]}"; then
      echo "SELF-TEST FAIL: no tree-only pattern matches: $sample"
      failures=$((failures + 1))
    fi
  done
  rm -f "$probe"

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
    load_list PATTERNS "$probe" > /dev/null
    echo $(( ${#PATTERNS[@]} - before ))
    rm -f "$probe"
  )
  if [ "$added" != "1" ]; then
    echo "SELF-TEST FAIL: local pattern loader took $added of 1 pattern"
    failures=$((failures + 1))
  fi

  # The ways around the scan the patterns cannot see. Each repository
  # below hides a key one way; the scan must still fail on it.
  if [ -z "${OQ_SECRETSCAN_NESTED:-}" ]; then
    local key='token=ghp_0123456789abcdefghijklmnopqrstuvwxyz'
    # name|file|content — a list, not an associative array, for bash 3.2.
    local evasions=(
      "a line that names this script|leak.txt|$key # scripts/check-no-secrets.sh"
      "a file .gitattributes calls binary|leak.dat|$key"
      "a committed allow-list|leak.txt|$key"
      "a copy of this script under another name|scripts/check-no-secrets.sh.bak|$key"
      "256 leaks, which an exit status would count as none|leak.txt|$key"
    )
    local entry name repo file body rc
    for entry in "${evasions[@]}"; do
      name="${entry%%|*}"
      file="${entry#*|}"; file="${file%%|*}"
      body="${entry#*|*|}"
      repo="$(mktemp -d)"
      git -C "$repo" init -q
      mkdir -p "$repo/scripts" "$(dirname "$repo/$file")"
      cp "$ROOT/scripts/check-no-secrets.sh" "$repo/scripts/"
      printf '%s\n' "$body" > "$repo/$file"
      case "$name" in
        *binary*) printf '*.dat binary\n' > "$repo/.gitattributes" ;;
        *allow-list*) printf '.*\n' > "$repo/.secretscan-allow" ;;
        256*) for i in $(seq 1 256); do printf '%s%d\n' "$body" "$i"; done > "$repo/$file" ;;
      esac
      git -C "$repo" add -f . > /dev/null
      # 1 is "found something"; 0 missed it, and anything else is the
      # scan failing for another reason, which proves nothing.
      nested_scan "$repo"
      rc=$?
      if [ "$rc" -ne 1 ]; then
        echo "SELF-TEST FAIL: scan of $name exited $rc, not 1"
        failures=$((failures + 1))
      fi
      rm -rf "$repo"
    done

    # The control: the same kind of repository with nothing in it must
    # pass, or every result above could be the harness failing.
    repo="$(mktemp -d)"
    git -C "$repo" init -q
    mkdir -p "$repo/scripts"
    cp "$ROOT/scripts/check-no-secrets.sh" "$repo/scripts/"
    printf 'nothing to see\n' > "$repo/clean.txt"
    git -C "$repo" add -f . > /dev/null
    nested_scan "$repo"
    rc=$?
    if [ "$rc" -ne 0 ]; then
      echo "SELF-TEST FAIL: scan of a clean repository exited $rc, not 0"
      failures=$((failures + 1))
    fi
    rm -rf "$repo"
  fi

  if [ "$failures" -gt 0 ]; then
    echo "$failures sample(s) were not detected"
    return 1
  fi
  echo "self-test: ${#samples[@]} + ${#tree_samples[@]} samples detected by ${#PATTERNS[@]} + ${#TREE_ONLY_PATTERNS[@]} patterns"
  return 0
}

allowed() {
  local hit="$1" entry
  for entry in "${PUBLIC_TEST_VECTORS[@]}"; do
    [[ "$hit" == *"$entry"* ]] && return 0
  done
  [ "${#ALLOW[@]}" -gt 0 ] || return 1
  local pattern
  for pattern in "${ALLOW[@]}"; do
    grep -qE -e "$pattern" <<< "$hit" && return 0
  done
  return 1
}

# The script is excluded by pathspec — the path git reports, not text on
# the matching line, which the change under scan controls.
SELF=':(exclude,top)scripts/check-no-secrets.sh'

# git grep exits 0 on a match, 1 on none, and anything else when it
# could not look. The last must stop the scan: read as "no match" it is
# a clean report from a scan that never ran.
run_grep() {
  local out="$1" rc
  shift
  git grep --text -n -P -i "$@" >> "$out" 2> "$out.err"
  rc=$?
  if [ "$rc" -gt 1 ]; then
    echo "git grep failed (exit $rc):"
    cat "$out.err"
    return 2
  fi
  return 0
}

grep_tracked() {
  run_grep "$1" "${EXPRS[@]}" -- . "$SELF"
}

grep_history() {
  # In batches: every commit on one command line stops fitting once the
  # history is long enough, and that failure is an error, not a clean scan.
  local revs=() rev
  while IFS= read -r rev; do
    revs+=("$rev")
    if [ "${#revs[@]}" -ge 200 ]; then
      run_grep "$1" "${EXPRS[@]}" "${revs[@]}" -- . "$SELF" || return 2
      revs=()
    fi
  done < <(git rev-list --all)
  if [ "${#revs[@]}" -gt 0 ]; then
    run_grep "$1" "${EXPRS[@]}" "${revs[@]}" -- . "$SELF" || return 2
  fi
  return 0
}

EXPRS=()

# A counter, not an exit status: a status wraps at 256, and 256 hits
# would read as none.
HITS=0

scan_target() {
  local label="$1" grep_fn="$2" out pattern hit shown=0
  local -a set=("${PATTERNS[@]}")
  if [ "$label" = "working tree" ]; then
    set+=("${TREE_ONLY_PATTERNS[@]}")
  fi
  # One pass with every pattern, not one pass per pattern: history is
  # read once. -e matters: several patterns begin with a dash and would
  # otherwise be parsed as options.
  EXPRS=()
  for pattern in "${set[@]}"; do
    EXPRS+=(-e "$pattern")
  done
  out="$(mktemp)"
  "$grep_fn" "$out" || { rm -f "$out" "$out.err"; exit 2; }
  while IFS= read -r hit; do
    [ -z "$hit" ] && continue
    allowed "$hit" && continue
    HITS=$((HITS + 1))
    shown=$((shown + 1))
    [ "$shown" -le 50 ] && echo "FAIL [$label] $hit"
  done < <(sort -u "$out")
  [ "$shown" -gt 50 ] && echo "... and $((shown - 50)) more in the $label"
  rm -f "$out" "$out.err"
}

# Files that switch the scan off, or publish what it guards, if they are
# ever committed.
refuse_tracked_config() {
  local path found=0
  for path in .secretscan-allow .secretscan-local; do
    if [ -n "$(git ls-files -- "$path")" ]; then
      echo "FAIL $path is tracked; it must stay local (see the header of this script)"
      found=1
    fi
  done
  return "$found"
}

if [ "$MODE" = "--self-test" ]; then
  self_test
  exit $?
fi

if ! self_test; then
  echo "refusing to report a clean scan from a broken scanner"
  exit 1
fi

refuse_tracked_config || HITS=$((HITS + 1))

scan_target "working tree" grep_tracked

if [ "$MODE" = "--history" ]; then
  scan_target "history" grep_history
fi

echo
if [ "$HITS" -gt 0 ]; then
  cat <<'MSG'
Secrets or deployment details found.

If this is a false positive, add a regex to .secretscan-allow (git-ignored).
If it is real, do not just delete the line: a committed secret is a
disclosed secret. Rotate the credential first, then remove it.
MSG
  exit 1
fi

echo "no secrets or deployment details found"
