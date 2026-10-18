#!/usr/bin/env bash
# secret-scan: refuse to push if the tree contains something that looks like a credential.
# Usage: ./secret-scan.sh [path]            (default: repo root). Exit 1 on a hit.
#        ./secret-scan.sh --stdin           scan content on stdin (used by pre-push)
#        ./secret-scan.sh --names           scan newline-separated file names on stdin
# Tree mode checks tracked file names in a Git working tree, or the names of
# all files on disk when the path is not in one, then scans file contents.
# Exit 0 = clean, 1 = hit, 2 = scanner error (e.g. a grep failure). Callers must
# treat anything other than 0 or 1 as an error and refuse.
# Vendored from https://github.com/5omeOtherGuy/secret-scan (secret-scan.sh at 1a9b5f7); update by copying, not by editing here.
set -u
MODE=tree
case "${1:-}" in
  --stdin) MODE=stdin; shift;;
  --names) MODE=names; shift;;
  -h|--help) sed -n '2,9p' "$0"; exit 0;;
esac
PAT='(sk-([A-Za-z0-9]{20,}|(ant|proj|or|svcacct|admin)-[A-Za-z0-9_-]{20,})|\bsk-[A-Za-z0-9_-]{20,}|gho_[A-Za-z0-9]{20,}|ghp_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|xox[baprs]-[A-Za-z0-9-]{10,}|\bxai-[A-Za-z0-9]{40,}|\bhf_[A-Za-z0-9]{34,}|\bAIza[0-9A-Za-z_-]{35}|\bgsk_[A-Za-z0-9]{40,}|\b[rs]k_live_[A-Za-z0-9]{24,}|AKIA[0-9A-Z]{16}|-----BEGIN [A-Z ]*PRIVATE KEY-----|"access_token"\s*:\s*"[^"]{20,}|"refresh_token"\s*:\s*"[^"]{20,}|Bearer [A-Za-z0-9._-]{30,}|eyJ[A-Za-z0-9_-]{30,}\.[A-Za-z0-9_-]{30,})'
FILES='(^|/)(auth\.json|\.credentials\.json|credentials\.json|.*\.key|.*\.pem|.*\.token|models\.json)$'

# grep returns 0 on a match (hit), 1 on no match (clean), >=2 on error. Never
# swallow the error: map 0->1, 1->0, anything else->2.
scan_grep() { # label grep-args...
  local label=$1; shift
  local rc
  grep "$@"
  rc=$?
  case $rc in
    0) echo "secret-scan: $label above" >&2; return 1;;
    1) return 0;;
    *) echo "secret-scan: scanner error: grep failed (exit $rc) while scanning $label" >&2; return 2;;
  esac
}

if [ "$MODE" = stdin ]; then
  scan_grep "credential-like content" -InE -- "$PAT" -
  rc=$?
  [ $rc -eq 0 ] && echo "secret-scan: clean"
  exit $rc
fi

if [ "$MODE" = names ]; then
  # LC_ALL=C: a name that is not valid UTF-8 still matches '.*'.
  LC_ALL=C scan_grep "forbidden file name" -E -- "$FILES" -
  rc=$?
  [ $rc -eq 0 ] && echo "secret-scan: clean"
  exit $rc
fi

ROOT=${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}
[ -e "$ROOT" ] || { echo "secret-scan: scanner error: no such file or directory: $ROOT" >&2; exit 2; }
# File names: tracked files inside a Git working tree; otherwise (no Git
# repository, or git not installed) every file found on disk, excluding .git.
# A failing listing is a scanner error, never a silent skip. Git names are
# listed NUL-delimited so that Git does not quote them (non-ASCII, quotes, tabs,
# newlines), then put one per line; a name with a newline still ends in its own
# last line, which is what the pattern checks.
if [ "$(git -C "$ROOT" rev-parse --is-inside-work-tree 2>/dev/null)" = true ]; then
  names=$(set -o pipefail; git -C "$ROOT" ls-files -z | tr '\0' '\n') || {
    echo "secret-scan: scanner error: git ls-files failed in $ROOT" >&2; exit 2; }
elif [ -d "$ROOT" ]; then
  echo "secret-scan: $ROOT is not a Git working tree; checking the names of all files on disk" >&2
  names=$(cd "$ROOT" && find . -name .git -prune -o ! -type d -print) || {
    echo "secret-scan: scanner error: listing files under $ROOT failed" >&2; exit 2; }
else
  names=$ROOT
fi
printf '%s\n' "$names" | LC_ALL=C scan_grep "forbidden file name" -E -- "$FILES" -
rc=$?
if [ $rc -eq 0 ]; then
  grep -rInE --exclude-dir=.git --exclude=secret-scan.sh -- "$PAT" "$ROOT"
  grc=$?
  if [ "$grc" -eq 0 ]; then
    echo "secret-scan: credential-like content above" >&2; rc=1
  elif [ "$grc" -gt 1 ]; then
    echo "secret-scan: scanner error: grep failed (exit $grc) while scanning content" >&2; rc=2
  fi
fi
[ $rc -eq 0 ] && echo "secret-scan: clean"
exit $rc
