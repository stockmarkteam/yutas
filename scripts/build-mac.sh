#!/bin/sh
# Builds yutas.app and a .dmg of it, signed with a Developer ID and notarized.
#
# The credentials are the same ones roku-log uses, under the same names, read
# from the shell or from .env in this folder (ENV_FILE=path picks another file):
#
#   ELECTROBUN_DEVELOPER_ID   signing identity, e.g. "Developer ID Application: NAME (TEAMID)"
#   ELECTROBUN_APPLEID        Apple ID for notarization
#   ELECTROBUN_APPLEIDPASS    its app-specific password
#   ELECTROBUN_TEAMID         team ID
#
# Tauri's own names (APPLE_SIGNING_IDENTITY, APPLE_ID, APPLE_PASSWORD,
# APPLE_TEAM_ID) work too and win when both are set.
#
# Without an identity, the one Developer ID certificate in the keychain is
# used. Without the Apple ID variables the app is signed but not notarized:
# fine on this Mac, but macOS will refuse to open it on someone else's.
set -eu
cd "$(dirname "$0")/.."

ENV_FILE="${ENV_FILE:-.env}"
if [ -f "$ENV_FILE" ]; then
	set -a
	# shellcheck disable=SC1090
	. "./$ENV_FILE" 2>/dev/null || . "$ENV_FILE"
	set +a
fi

export APPLE_SIGNING_IDENTITY="${APPLE_SIGNING_IDENTITY:-${ELECTROBUN_DEVELOPER_ID:-}}"
export APPLE_ID="${APPLE_ID:-${ELECTROBUN_APPLEID:-}}"
export APPLE_PASSWORD="${APPLE_PASSWORD:-${ELECTROBUN_APPLEIDPASS:-}}"
export APPLE_TEAM_ID="${APPLE_TEAM_ID:-${ELECTROBUN_TEAMID:-}}"

if [ -z "$APPLE_SIGNING_IDENTITY" ]; then
	found=$(security find-identity -v -p codesigning | sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p')
	if [ "$(printf '%s\n' "$found" | grep -c .)" -eq 1 ]; then
		APPLE_SIGNING_IDENTITY="$found"
	else
		echo "error: no signing identity. Set ELECTROBUN_DEVELOPER_ID (found in the keychain: $(printf '%s\n' "$found" | grep -c .))" >&2
		exit 1
	fi
fi
echo "signing as: $APPLE_SIGNING_IDENTITY"

if [ -n "$APPLE_ID" ] && [ -n "$APPLE_PASSWORD" ] && [ -n "$APPLE_TEAM_ID" ]; then
	echo "notarizing as: $APPLE_ID (team $APPLE_TEAM_ID)"
else
	# Tauri notarizes only when all three are present; make "none" explicit.
	unset APPLE_ID APPLE_PASSWORD APPLE_TEAM_ID
	echo "warning: ELECTROBUN_APPLEID / ELECTROBUN_APPLEIDPASS / ELECTROBUN_TEAMID are not all set: NOT notarizing" >&2
fi

npx tauri build --bundles app

APP="src-tauri/target/release/bundle/macos/yutas.app"
codesign --verify --deep --strict "$APP"
codesign -d -vv "$APP" 2>&1 | grep -E '^(Identifier|Authority=Developer ID|TeamIdentifier|Runtime)'

# The disk image: the app next to a shortcut to /Applications. Built with
# hdiutil alone (Tauri's own DMG step drives Finder, which needs a permission
# prompt answered and times out otherwise).
VERSION=$(sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' src-tauri/tauri.conf.json | head -1)
DMG="src-tauri/target/release/bundle/yutas_${VERSION}_$(uname -m).dmg"
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
rm -f "$DMG"
hdiutil create -quiet -volname yutas -srcfolder "$STAGE" -format UDZO "$DMG"
codesign --force --sign "$APPLE_SIGNING_IDENTITY" "$DMG"

if [ -n "${APPLE_ID:-}" ]; then
	# Tauri has notarized and stapled the app; the image needs its own ticket.
	xcrun notarytool submit "$DMG" --apple-id "$APPLE_ID" --password "$APPLE_PASSWORD" --team-id "$APPLE_TEAM_ID" --wait
	xcrun stapler staple "$DMG"
fi

if xcrun stapler validate "$APP" >/dev/null 2>&1 && xcrun stapler validate "$DMG" >/dev/null 2>&1; then
	echo "notarized: yes"
else
	echo "notarized: no"
fi
echo "built: $APP"
echo "built: $DMG"
