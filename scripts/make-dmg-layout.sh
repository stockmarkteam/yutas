#!/bin/sh
# Regenerates scripts/dmg/DS_Store: the Finder window layout of the disk
# image (icon view, the app on the left, the Applications folder on the
# right). build-mac.sh copies that file into every image, so the build
# itself never has to drive Finder.
#
# Run this only to change the layout. It asks Finder to arrange a scratch
# image, so macOS may ask once whether your terminal may control Finder.
set -eu
cd "$(dirname "$0")/.."

VOLUME="yutas"   # must match the volume name in build-mac.sh
WORK=$(mktemp -d)
trap 'hdiutil detach -quiet "/Volumes/$VOLUME" 2>/dev/null || true; rm -rf "$WORK"' EXIT

if [ -e "/Volumes/$VOLUME" ]; then
	echo "error: /Volumes/$VOLUME is mounted. Eject it first." >&2
	exit 1
fi

# Stand-ins with the names the real image will have: Finder stores the
# positions by name.
mkdir -p "$WORK/stage/yutas.app"
ln -s /Applications "$WORK/stage/Applications"
hdiutil create -quiet -volname "$VOLUME" -srcfolder "$WORK/stage" -fs HFS+ -format UDRW "$WORK/layout.dmg"
hdiutil attach -quiet -noautoopen "$WORK/layout.dmg"

osascript <<EOF
with timeout of 120 seconds
	tell application "Finder"
		tell disk "$VOLUME"
			open
			set current view of container window to icon view
			set toolbar visible of container window to false
			set statusbar visible of container window to false
			set the bounds of container window to {200, 160, 760, 520}
			set opts to the icon view options of container window
			set arrangement of opts to not arranged
			set icon size of opts to 128
			set text size of opts to 13
			set position of item "yutas.app" of container window to {150, 170}
			set position of item "Applications" of container window to {410, 170}
			close
			open
			update without registering applications
			delay 2
			close
		end tell
	end tell
end timeout
EOF

sync
sleep 1
mkdir -p scripts/dmg
cp "/Volumes/$VOLUME/.DS_Store" scripts/dmg/DS_Store
echo "wrote scripts/dmg/DS_Store ($(wc -c < scripts/dmg/DS_Store | tr -d ' ') bytes)"
