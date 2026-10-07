#!/usr/bin/env bash
# Merge guard of the guide, plugins/orchestrator/. A change to any other file
# must update the guide too, or the pull request's description must hold a
# line "Guide: unchanged, <reason>" saying why the guide stays true.
#
# Usage: scripts/guide-guard.sh <base> <head> <description file>
# Compares <head> with its merge base with <base>, in the current repository.
# Exits 0 when the guide is not due, 1 when it is, 2 on an error.
set -euo pipefail

guide=plugins/orchestrator/
form='Guide: unchanged, <why the guide stays true>'

if [[ $# -ne 3 ]]; then
  echo "usage: $0 <base> <head> <description file>" >&2
  exit 2
fi
base=$(git merge-base "$1" "$2") || exit 2
head=$(git rev-parse --verify "$2^{commit}") || exit 2
# A description edited on GitHub has CRLF line endings.
body=$(tr -d '\r' <"$3") || exit 2

changed=$(git -c core.quotePath=false diff --no-renames --name-only "$base" "$head") || exit 2
if [[ -z $changed ]]; then
  echo "No file changed."
  exit 0
fi
if grep -q -- "^$guide" <<<"$changed"; then
  echo "The guide is updated."
  exit 0
fi

statement=$(grep -m1 -E '^[[:space:]]*Guide: unchanged,[[:space:]]*[^[:space:]]' <<<"$body" || true)
if [[ -n $statement ]]; then
  reason=${statement#*Guide: unchanged,}
  echo "The guide stays unchanged: ${reason#"${reason%%[![:space:]]*}"}"
  exit 0
fi

attempt=$(grep -m1 -E '^[[:space:]]*Guide: unchanged' <<<"$body" || true)
if [[ -n $attempt ]]; then
  cat <<EOF
The description says the guide is unchanged without giving a reason:
  $attempt
Write the line as
  $form
Editing the description runs this check again.
EOF
  exit 1
fi

cat <<EOF
This pull request changes files outside the guide ($guide):
$(sed 's/^/  /' <<<"$changed")
The guide must describe the code it ships with. Either update it under
$guide, or, once you have checked that it needs no change,
add this line to the pull request's description:
  $form
Editing the description runs this check again.
EOF
exit 1
