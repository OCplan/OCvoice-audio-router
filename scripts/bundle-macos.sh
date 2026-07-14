#!/usr/bin/env bash

set -euo pipefail

ROUTER_BINARY="${ROUTER_BINARY:-}"
ENGINE_BINARY="${ENGINE_BINARY:-}"
FFMPEG_BINARY="${FFMPEG_BINARY:-}"
INFO_PLIST="${INFO_PLIST:-resources/macos/Info.plist}"
ENTITLEMENTS="${ENTITLEMENTS:-resources/macos/entitlements.plist}"
APP_NAME="${APP_NAME:-OCvoice Audio Router.app}"
APP_VERSION="${APP_VERSION:-}"
SIGNING_IDENTITY="${SIGNING_IDENTITY:-}"

require_file() {
  local variable_name="$1"
  local variable_value="${!variable_name:-}"

  if [[ -z "${variable_value}" ]]; then
    echo "error: ${variable_name} is required" >&2
    exit 1
  fi

  if [[ ! -f "${variable_value}" ]]; then
    echo "error: ${variable_name} does not exist or is not a file: ${variable_value}" >&2
    exit 1
  fi
}

require_file ROUTER_BINARY
require_file ENGINE_BINARY
require_file FFMPEG_BINARY
require_file INFO_PLIST
require_file ENTITLEMENTS

if [[ "${APP_NAME}" != *.app ]]; then
  echo "error: APP_NAME must end in .app: ${APP_NAME}" >&2
  exit 1
fi

if ! command -v codesign >/dev/null 2>&1; then
  echo "error: codesign is required to build the macOS app bundle" >&2
  exit 1
fi

if [[ -n "${APP_VERSION}" && ! -x /usr/libexec/PlistBuddy ]]; then
  echo "error: /usr/libexec/PlistBuddy is required when APP_VERSION is set" >&2
  exit 1
fi

rm -rf "${APP_NAME}"
mkdir -p "${APP_NAME}/Contents/MacOS" "${APP_NAME}/Contents/Resources"

cp "${ROUTER_BINARY}" "${APP_NAME}/Contents/MacOS/ocvoice-audio-router"
cp "${ENGINE_BINARY}" "${APP_NAME}/Contents/Resources/ocvoice-restream-engine"
cp "${FFMPEG_BINARY}" "${APP_NAME}/Contents/Resources/ffmpeg"
cp "${INFO_PLIST}" "${APP_NAME}/Contents/Info.plist"

chmod +x \
  "${APP_NAME}/Contents/MacOS/ocvoice-audio-router" \
  "${APP_NAME}/Contents/Resources/ocvoice-restream-engine" \
  "${APP_NAME}/Contents/Resources/ffmpeg"

if [[ -n "${APP_VERSION}" ]]; then
  /usr/libexec/PlistBuddy -c "Set :CFBundleVersion ${APP_VERSION}" "${APP_NAME}/Contents/Info.plist"
  /usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString ${APP_VERSION}" "${APP_NAME}/Contents/Info.plist"
fi

signing_identity="-"
if [[ -n "${SIGNING_IDENTITY}" ]]; then
  signing_identity="${SIGNING_IDENTITY}"
fi

sign() {
  if [[ "${signing_identity}" == "-" ]]; then
    codesign --force --options runtime \
      --entitlements "${ENTITLEMENTS}" \
      --sign "${signing_identity}" \
      "$1"
  else
    codesign --force --options runtime \
      --entitlements "${ENTITLEMENTS}" \
      --sign "${signing_identity}" \
      --timestamp \
      "$1"
  fi
}

# Sign nested code before its containing bundle (inside-out).
sign "${APP_NAME}/Contents/Resources/ffmpeg"
sign "${APP_NAME}/Contents/Resources/ocvoice-restream-engine"
sign "${APP_NAME}/Contents/MacOS/ocvoice-audio-router"
sign "${APP_NAME}"

codesign --verify --deep --strict --verbose=2 "${APP_NAME}"
codesign -dv --entitlements - "${APP_NAME}/Contents/Resources/ocvoice-restream-engine"

app_parent=$(cd "$(dirname "${APP_NAME}")" && pwd -P)
echo "bundle ready at ${app_parent}/$(basename "${APP_NAME}")"
