#!/usr/bin/env bash
# Merge guard of docs/claude-code-dependency.md. A change to a file that relies
# on Claude Code, one holding a marker comment before or after the change,
# must update that document too.
#
# Usage: scripts/claude-code-guard.sh <base> <head>
# Compares <head> with its merge base with <base>, in the current repository.
# Exits 0 when the document is not due, 1 when it is, 2 on an error.
set -euo pipefail

doc=docs/claude-code-dependency.md
marker='^[[:space:]]*// claude-code: [a-z0-9]+(-[a-z0-9]+)*[[:space:]]*$'

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <base> <head>" >&2
  exit 2
fi
base=$(git merge-base "$1" "$2") || exit 2
head=$(git rev-parse --verify "$2^{commit}") || exit 2

mapfile -t changed < <(git diff --no-renames --name-only "$base" "$head")
if [[ ${#changed[@]} -eq 0 ]]; then
  echo "No file changed."
  exit 0
fi
for file in "${changed[@]}"; do
  if [[ $file == "$doc" ]]; then
    echo "$doc is updated."
    exit 0
  fi
done

# Files holding a marker in either version, listed by git grep as
# <revision>:<path>. It exits 1 when nothing matches.
relying=""
for rev in "$base" "$head"; do
  status=0
  found=$(git --literal-pathspecs grep -l -E "$marker" "$rev" -- "${changed[@]}") || status=$?
  if [[ $status -gt 1 ]]; then
    exit 2
  fi
  while IFS= read -r line; do
    if [[ -n $line ]]; then
      relying+="${line#*:}"$'\n'
    fi
  done <<<"$found"
done
relying=$(sort -u <<<"$relying" | sed '/^$/d')
if [[ -z $relying ]]; then
  echo "No changed file relies on Claude Code."
  exit 0
fi

cat <<EOF
These changed files rely on Claude Code (they hold a claude-code marker):
$(sed 's/^/  /' <<<"$relying")
Update $doc to match: a contract added,
changed or gone, or where it lives in the code. When every contract stays as
it is, add the label claude-code-dependency-unchanged to the pull request
instead.
EOF
exit 1
