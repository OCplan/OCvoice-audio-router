#!/usr/bin/env bash
# Refuse to build a release whose version numbers disagree.
#
# This app carries its version in three places, and only one of them is the tag:
#
#   Cargo.toml  -> env!("CARGO_PKG_VERSION") -> tray label, /health, startup
#                  banner, and the auto-update comparison against the latest
#                  GitHub release.
#   the v* tag  -> CFBundleVersion/CFBundleShortVersionString, written into the
#                  bundle by scripts/bundle-macos.sh, but only on a tag build.
#   Info.plist  -> the literal those two fields fall back to when no tag is in
#                  play (a workflow_dispatch build).
#
# Tagging without bumping Cargo.toml therefore ships an app that names itself
# with the OLD number and compares updates against the OLD number, so it will
# not see the release it IS. That is not hypothetical: v0.3.1 and v0.3.2 both
# went out while Cargo.toml still said 0.3.0. This gate exists so that failure
# cannot recur silently -- it must fail the build instead.
#
# Usage: scripts/check-version-tag.sh [ref-name]
#   ref-name defaults to $GITHUB_REF_NAME. Run it before tagging:
#       scripts/check-version-tag.sh v0.3.3
#   A ref that is not a v* tag still checks Cargo.toml against Info.plist and
#   then passes -- there is no tag to agree with on a dispatch build.
#
# Run from the repository root. Kept POSIX-ish (no mapfile, no associative
# arrays) so it runs on the bash 3.2 that ships with macOS as well as on CI.
set -euo pipefail

REF_NAME="${1:-${GITHUB_REF_NAME:-}}"
PLIST="resources/macos/Info.plist"

# First `version = "..."` inside [package]; a dependency's version cannot match.
CARGO_VERSION="$(awk '
  /^\[/ { in_package = ($0 == "[package]") }
  in_package && /^version[[:space:]]*=/ {
    sub(/^version[[:space:]]*=[[:space:]]*"/, "")
    sub(/".*$/, "")
    print
    exit
  }
' Cargo.toml)"

if [ -z "${CARGO_VERSION}" ]; then
  echo "::error::could not read version from [package] in Cargo.toml" >&2
  exit 1
fi

# Every <string> on the line after a CFBundle*Version key.
plist_count=0
plist_bad=0
plist_bad_value=""
while IFS= read -r plist_version; do
  plist_count=$((plist_count + 1))
  if [ "${plist_version}" != "${CARGO_VERSION}" ]; then
    plist_bad=1
    plist_bad_value="${plist_version}"
  fi
done < <(awk '
  take { sub(/^[[:space:]]*<string>/, ""); sub(/<\/string>.*$/, ""); print; take = 0 }
  /<key>CFBundleVersion<\/key>|<key>CFBundleShortVersionString<\/key>/ { take = 1 }
' "${PLIST}")

if [ "${plist_count}" -ne 2 ]; then
  echo "::error::expected CFBundleVersion and CFBundleShortVersionString in ${PLIST}, found ${plist_count}" >&2
  exit 1
fi

if [ "${plist_bad}" -ne 0 ]; then
  echo "::error::${PLIST} says '${plist_bad_value}' but Cargo.toml says ${CARGO_VERSION}" >&2
  exit 1
fi

case "${REF_NAME}" in
  v*) ;;
  *)
    echo "Ref '${REF_NAME:-<none>}' is not a v* tag; nothing to compare a tag against."
    echo "Cargo.toml and ${PLIST} agree on ${CARGO_VERSION}."
    exit 0
    ;;
esac

TAG_VERSION="${REF_NAME#v}"
if [ "${TAG_VERSION}" != "${CARGO_VERSION}" ]; then
  echo "::error::tag ${REF_NAME} does not match Cargo.toml version ${CARGO_VERSION} -- bump Cargo.toml, ${PLIST} and Cargo.lock in the commit the tag points at, then re-tag" >&2
  exit 1
fi

echo "Tag ${REF_NAME}, Cargo.toml and ${PLIST} all agree on ${CARGO_VERSION}."
