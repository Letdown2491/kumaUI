#!/usr/bin/env bash
# Cuts a release from CHANGELOG.md's Unreleased section.
#
# The bump is inferred from the subsections the bullets land under:
# any Added or Removed infers a minor bump (at 0.x that is also where
# breaking changes ride); only Changed and Fixed infer a patch. Kind
# decides, not amount. The script asks which apps move, bumps their
# Cargo.tomls, inserts the release heading with the version lines,
# commits, then builds and installs through build.sh. The order is the
# fix for the parent-stamp bug (v0.4.1 and v0.5.1 both shipped binaries
# stamped with the release commit's parent and needed a manual rebuild
# at the tag before kumaOS could pin): the stamp is HEAD at build time,
# so the commit must exist before the build runs. If the build fails
# after the commit, the recovery is manual and honest: fix the build,
# rerun `./scripts/build.sh build --release`, then tag by hand.
# kumaOS is atomic (no host toolchain), so cargo only runs in the
# container.
#
#   ./scripts/release.sh          # commit the release
#   ./scripts/release.sh --tag    # also tag vN
set -euo pipefail
cd "$(dirname "$0")/.."

APPS=(kuma-shell kuma-files kuma-term kuma-monitor)
DATE=$(date +%F)
TAG=0
[ "${1:-}" = "--tag" ] && TAG=1

if ! git diff --quiet || ! git diff --cached --quiet; then
  echo "release: the working tree is not clean; commit or stash first" >&2
  exit 1
fi

# the Unreleased body: from its heading to the next release heading
body=$(awk '/^## Unreleased$/{on=1; next} /^## /{on=0} on' CHANGELOG.md)

if grep -q '^### \(Added\|Removed\)' <<<"$body"; then bump=minor
elif grep -q '^### \(Changed\|Fixed\)' <<<"$body"; then bump=patch
else
  echo "release: nothing to release under ## Unreleased; write the bullets first" >&2
  exit 1
fi
count=$(grep -c '^- ' <<<"$body" || true)
echo "release: $count bullet(s) under Unreleased infer a $bump bump"

bump_version() { # old bump -> new
  local maj min pat
  IFS=. read -r maj min pat <<<"$1"
  case "$2" in
    major) echo "$((maj + 1)).0.0" ;;
    minor) if [ "$maj" = 0 ]; then echo "0.$((min + 1)).0"; else echo "$maj.$((min + 1)).0"; fi ;;
    patch) echo "$maj.$min.$((pat + 1))" ;;
  esac
}

declare -A bumped
train=""
for app in "${APPS[@]}"; do
  old=$(grep -m1 '^version = ' "$app/Cargo.toml" | cut -d'"' -f2)
  new=$(bump_version "$old" "$bump")
  read -rp "release: bump $app $old -> $new? [Y/n] " answer
  case "$answer" in n*|N*) continue ;; esac
  bumped[$app]=$new
  # the heading names the train's version: the highest number moved today
  train=$(printf '%s\n%s\n' "$train" "$new" | sort -V | tail -1)
  # the [package] version is the only line-start match: dependencies
  # carry their version keys inside braces or later tables
  sed -i "s/^version = \"$old\"/version = \"$new\"/" "$app/Cargo.toml"
done

if [ -z "$train" ]; then
  echo "release: no app bumped; nothing to do" >&2
  exit 1
fi

# the version lines name every app's version as of this release
lines=""
for app in "${APPS[@]}"; do
  v=$(grep -m1 '^version = ' "$app/Cargo.toml" | cut -d'"' -f2)
  lines+="$app $v"$'\n'
done

# the heading goes between ## Unreleased and the bullets; the blank
# line that closed Unreleased now separates the version lines from the
# previous release heading
export REL_HEADING="## v$train ($DATE)" REL_LINES="$lines"
awk '
  /^## Unreleased$/ {
    print; print ""; print ENVIRON["REL_HEADING"]; print "";
    printf "%s", ENVIRON["REL_LINES"]; next
  }
  { print }
' CHANGELOG.md > .CHANGELOG.tmp && mv .CHANGELOG.tmp CHANGELOG.md

# the lock refresh rides the container (no host toolchain) and cargo
# update -w touches only the workspace members' versions, so the
# release commit below is self-consistent and the post-commit build
# has nothing left to rewrite
./scripts/build.sh update -w

git add CHANGELOG.md Cargo.lock "${APPS[@]/%//Cargo.toml}"
git commit -m "release: v$train"

# build and install: HEAD is the release commit now, so the binaries'
# stamp is the sha the tag will name, and the tree built is the tree
# the tag names
./scripts/build.sh build --release

# the stamp assert reads the installed paths (what kumaOS's stamp gate
# reads), not target/release: a stale sibling from an earlier build
# surviving there is a per-binary failure mode. All five answer, not
# just the shell.
sha=$(git rev-parse --short HEAD)
for app in kuma-shell kuma-greeter kuma-files kuma-term kuma-monitor; do
  stamp=$(~/.local/bin/"$app" --version)
  case "$stamp" in
    *"g$sha"*) ;;
    *) echo "release: $app does not answer g$sha: $stamp" >&2; exit 1 ;;
  esac
done
# a lock the build still rewrote means the tagged tree would not be the
# tree that was built
git diff --exit-code Cargo.lock

[ "$TAG" = 1 ] && git tag "v$train"
echo "release: v$train committed"
