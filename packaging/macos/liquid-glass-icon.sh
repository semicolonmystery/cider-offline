#!/bin/sh
# Builds a layered (Liquid Glass, macOS 26+) app icon with Xcode 26's actool.
#
#   packaging/macos/liquid-glass-icon.sh <out-dir>
#
# The Icon Composer `.icon` bundle is generated here from the sources of truth:
# the glyph of packaging/icons/src/tray-template.svg (recoloured white) as the
# foreground layer, on the app tile's #FF7A3D -> #E2145A gradient. actool
# compiles it to <out-dir>/Assets.car with the icon named "AppIcon" (the value
# for CFBundleIconName). The .icns that cargo-packager builds stays the
# fallback for macOS 11-15.
#
# Exits non-zero with the reason on stderr when this runner cannot do it
# (no Xcode 26 actool, compile error, no Assets.car); the release workflow
# then ships the .icns only.
set -eu

out=${1:?usage: liquid-glass-icon.sh <out-dir>}
here=$(cd "$(dirname "$0")" && pwd)
glyph_src="$here/../icons/src/tray-template.svg"

xcode=$(xcodebuild -version 2>/dev/null | sed -n 's/^Xcode \([0-9][0-9]*\).*/\1/p')
if [ -z "$xcode" ] || [ "$xcode" -lt 26 ]; then
    echo "needs Xcode 26 actool; the selected Xcode is '${xcode:-none}'" >&2
    exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
icon="$work/AppIcon.icon"
mkdir -p "$icon/Assets" "$out"

# Foreground: the glyph paths, white, on the 256-unit canvas of app.svg so it
# keeps the app icon's proportions, drawn at Icon Composer's 1024 px canvas.
{
    echo '<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024" viewBox="0 0 256 256">'
    sed -n '/<g /,/<\/g>/p' "$glyph_src" | sed 's/#000/#fff/g'
    echo '</svg>'
} > "$icon/Assets/glyph.svg"

# #FF7A3D = (255, 122, 61), #E2145A = (226, 20, 90), as sRGB fractions.
cat > "$icon/icon.json" <<'JSON'
{
  "fill" : {
    "linear-gradient" : [
      "srgb:1.00000,0.47843,0.23922,1.00000",
      "srgb:0.88627,0.07843,0.35294,1.00000"
    ]
  },
  "groups" : [
    {
      "layers" : [
        {
          "glass" : true,
          "image-name" : "glyph.svg",
          "name" : "glyph"
        }
      ],
      "shadow" : {
        "kind" : "neutral",
        "opacity" : 0.5
      },
      "translucency" : {
        "enabled" : true,
        "value" : 0.5
      }
    }
  ],
  "supported-platforms" : {
    "squares" : "shared"
  }
}
JSON

if ! xcrun actool \
    --compile "$out" \
    --platform macosx \
    --target-device mac \
    --minimum-deployment-target 11.0 \
    --app-icon AppIcon \
    --include-all-app-icons \
    --output-partial-info-plist "$work/partial.plist" \
    --output-format human-readable-text \
    --notices --warnings --errors \
    "$icon" >&2; then
    echo "actool failed to compile the .icon (Xcode $xcode)" >&2
    exit 1
fi

if [ ! -s "$out/Assets.car" ]; then
    echo "actool (Xcode $xcode) produced no Assets.car" >&2
    exit 1
fi
echo "built $out/Assets.car with Xcode $xcode"
