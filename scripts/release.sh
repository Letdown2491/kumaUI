#!/usr/bin/env bash
# Cuts a release from CHANGELOG.md's Unreleased section.
#
# The bump is inferred from the subsections the bullets land under:
# any Added or Removed infers a minor bump (at 0.x that is also where
# breaking changes ride); only Changed and Fixed infer a patch. Kind
# decides, not amount. The script asks which apps move, bumps their
# Cargo.tomls, inserts the release heading with the three version
# lines, refreshes the lock and installs through build.sh, and commits.
# kumaOS is atomic (no host toolchain), so cargo only runs in the
# container.
#
#   ./scripts/release.sh          # commit the release
#   ./scripts/release.sh --tag    # also tag vN
set -euo pipefail
cd "$(dirname "$0")/.."

APPS=(kuma-shell kuma-files kuma-term)
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

# the three version lines name every app's version as of this release
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

# the build refreshes Cargo.lock and installs the fresh binaries
./scripts/build.sh build --release

git add CHANGELOG.md Cargo.lock "${APPS[@]/%//Cargo.toml}"
git commit -m "release: v$train"
[ "$TAG" = 1 ] && git tag "v$train"
echo "release: v$train committed"
