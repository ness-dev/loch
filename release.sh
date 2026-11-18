#!/usr/bin/env bash
# Cut a release. Bumps the version, verifies, tags, and pushes; CI builds every
# platform's binary and publishes the GitHub release from the tag.
#
#   ./release.sh 0.2.0
#   DRY_RUN=1 ./release.sh 0.2.0     rehearse without committing or pushing
set -euo pipefail

BRANCH=main
DRY_RUN=${DRY_RUN:-}

die() { printf 'release: %s\n' "$1" >&2; exit 1; }
run() { if [ -n "$DRY_RUN" ]; then printf '  would run: %s\n' "$*"; else "$@"; fi; }

[ $# -eq 1 ] || die "usage: ./release.sh <version>   (e.g. ./release.sh 0.2.0)"
version=${1#v}
echo "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.]+)?$' \
  || die "'$version' is not a version like 0.2.0"

cd "$(dirname "$0")"

tag="v$version"
[ "$(git rev-parse --abbrev-ref HEAD)" = "$BRANCH" ] || die "not on $BRANCH"
[ -z "$(git status --porcelain)" ] || die "working tree is dirty; commit or stash first"
git rev-parse -q --verify "refs/tags/$tag" >/dev/null && die "$tag already exists locally"

git fetch --quiet origin "$BRANCH" --tags
git ls-remote --exit-code --tags origin "$tag" >/dev/null 2>&1 && die "$tag already exists on origin"
[ -z "$(git rev-list "HEAD..origin/$BRANCH")" ] || die "$BRANCH is behind origin; pull first"

# The tag has to point at a commit containing the workflow that builds it: a
# tag-triggered run uses the workflow file as of the tagged commit.
echo "==> setting version to $version"
# The first `version = ` line is the package's; dependency tables come later.
sed -i.bak "1,/^version = \".*\"$/s/^version = \".*\"$/version = \"$version\"/" Cargo.toml
rm -f Cargo.toml.bak
grep -q "^version = \"$version\"$" Cargo.toml || die "version bump did not apply"

echo "==> verifying"
cargo fmt --check
cargo clippy -q --release --all-targets -- -D warnings
cargo test -q --release
[ "$(cargo run -q --release -- --version)" = "loch $version" ] || die "--version disagrees"

echo "==> tagging $tag"
# Cargo.toml may already hold this version — the first release, or a re-cut
# after a deleted tag. An empty commit fails under `set -e`, so tag what's there.
if [ -n "$(git status --porcelain)" ]; then
  run git add Cargo.toml Cargo.lock
  run git commit -m "release: $tag"
else
  echo "  Cargo.toml is already at $version; tagging the current commit"
fi
run git tag -a "$tag" -m "$tag"
run git push origin "$BRANCH"
run git push origin "$tag"

if [ -n "$DRY_RUN" ]; then
  git checkout -- Cargo.toml Cargo.lock
  echo "==> dry run: version restored, nothing pushed"
  exit 0
fi

slug=$(git remote get-url origin | sed -E 's#(git@github.com:|https://github.com/)##; s#\.git$##')
cat <<EOF

released $tag
  watch:   https://github.com/$slug/actions
  assets:  https://github.com/$slug/releases/tag/$tag
EOF
