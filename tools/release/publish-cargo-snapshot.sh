#!/usr/bin/env bash
set -euo pipefail

# Cargo's git source cache retains every ancestor needed by a pinned revision.
# Publish the same reviewed tree as a parentless commit so consumers fetch the
# source they need without inheriting this repository's fixture-heavy history.
# The full source SHA in the lightweight tag makes the mapping auditable.

usage() {
  echo "usage: $0 [--push] <source-revision> [remote]" >&2
}

snapshot_push=false
if [[ "${1:-}" == "--push" ]]; then
  snapshot_push=true
  shift
fi

if (( $# < 1 || $# > 2 )); then
  usage
  exit 2
fi

snapshot_source_input=$1
snapshot_remote=${2:-origin}

snapshot_source_commit=$(git rev-parse --verify "${snapshot_source_input}^{commit}")
snapshot_source_tree=$(git rev-parse --verify "${snapshot_source_commit}^{tree}")
snapshot_default_branch=$(
  git ls-remote --symref "$snapshot_remote" HEAD |
    awk '$1 == "ref:" { sub("refs/heads/", "", $2); print $2; exit }'
)

if [[ -z "$snapshot_default_branch" ]]; then
  echo "could not resolve the default branch for remote '$snapshot_remote'" >&2
  exit 1
fi

git fetch --quiet --no-tags "$snapshot_remote" "$snapshot_default_branch"
snapshot_default_tip=$(git rev-parse --verify FETCH_HEAD)

if ! git merge-base --is-ancestor "$snapshot_source_commit" "$snapshot_default_tip"; then
  echo "source commit $snapshot_source_commit is not on $snapshot_remote/$snapshot_default_branch" >&2
  exit 1
fi

snapshot_author_name=$(git show -s --format=%an "$snapshot_source_commit")
snapshot_author_email=$(git show -s --format=%ae "$snapshot_source_commit")
snapshot_source_date=$(git show -s --format=%aI "$snapshot_source_commit")
snapshot_ref="refs/tags/cargo-snapshot-${snapshot_source_commit}"
snapshot_message=$(printf \
  'Cargo source snapshot for Vize %s\n\nSource-Commit: %s\nSource-Tree: %s\n' \
  "$snapshot_source_commit" \
  "$snapshot_source_commit" \
  "$snapshot_source_tree")

snapshot_commit=$(
  printf '%s' "$snapshot_message" |
    GIT_AUTHOR_NAME="$snapshot_author_name" \
      GIT_AUTHOR_EMAIL="$snapshot_author_email" \
      GIT_AUTHOR_DATE="$snapshot_source_date" \
      GIT_COMMITTER_NAME="Vize Cargo Snapshot" \
      GIT_COMMITTER_EMAIL="nympharum@proton.me" \
      GIT_COMMITTER_DATE="$snapshot_source_date" \
      git commit-tree "$snapshot_source_tree"
)

snapshot_parent_count=$(git rev-list --parents -n 1 "$snapshot_commit" | awk '{ print NF - 1 }')
snapshot_actual_tree=$(git rev-parse --verify "${snapshot_commit}^{tree}")
if [[ "$snapshot_parent_count" != "0" || "$snapshot_actual_tree" != "$snapshot_source_tree" ]]; then
  echo "generated snapshot is not an exact orphan of source tree $snapshot_source_tree" >&2
  exit 1
fi

snapshot_remote_commit=$(
  git ls-remote --refs "$snapshot_remote" "$snapshot_ref" |
    awk 'NR == 1 { print $1 }'
)

if [[ -n "$snapshot_remote_commit" ]]; then
  if [[ "$snapshot_remote_commit" != "$snapshot_commit" ]]; then
    echo "$snapshot_ref already exists at unexpected commit $snapshot_remote_commit" >&2
    exit 1
  fi
  echo "$snapshot_ref already publishes $snapshot_commit"
  exit 0
fi

if [[ "$snapshot_push" != true ]]; then
  echo "dry run: would publish $snapshot_commit as $snapshot_ref to $snapshot_remote"
  echo "source tree: $snapshot_source_tree"
  exit 0
fi

git push "$snapshot_remote" "$snapshot_commit:$snapshot_ref"

snapshot_published_commit=$(
  git ls-remote --refs "$snapshot_remote" "$snapshot_ref" |
    awk 'NR == 1 { print $1 }'
)
if [[ "$snapshot_published_commit" != "$snapshot_commit" ]]; then
  echo "published ref verification failed for $snapshot_ref" >&2
  exit 1
fi

echo "published $snapshot_commit as $snapshot_ref"
echo "source tree: $snapshot_source_tree"
