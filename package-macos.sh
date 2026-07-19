#!/bin/bash
# Packages the Companion Plugin as a real macOS OBS plugin bundle, matching
# the exact structure of an already-installed, working plugin on this
# machine (~/Library/Application Support/obs-studio/plugins/distroav.plugin)
# — inspected directly as ground truth rather than guessed: a CFBundle
# (.plugin) with Contents/MacOS/<name> as the Mach-O binary and a minimal
# Info.plist, no Resources needed yet (Phase 1 has no locale/UI files).
#
# Two modes:
#   ./package-macos.sh              — quick local dev-test build: current
#                                      arch only, debug, ad-hoc signed.
#                                      Unchanged from before this comment.
#   ./package-macos.sh --release "<Developer ID Application: ...>"
#                                    — release build for bundling into a
#                                      real FrameSW.app: universal
#                                      (arm64 + x86_64), release profile,
#                                      signed with the given identity
#                                      instead of ad-hoc. This is what
#                                      scripts/package-macos.sh invokes.
set -euo pipefail

cd "$(dirname "$0")"

RELEASE_MODE=0
SIGN_IDENTITY="-"
if [ "${1:-}" = "--release" ]; then
    RELEASE_MODE=1
    SIGN_IDENTITY="${2:?--release requires a signing identity as the second argument}"
fi

BUNDLE="target/framesw-companion.plugin"
rm -rf "$BUNDLE"
mkdir -p "$BUNDLE/Contents/MacOS"

if [ "$RELEASE_MODE" = "1" ]; then
    echo "Building universal release binary..."
    cargo build --release --target aarch64-apple-darwin
    cargo build --release --target x86_64-apple-darwin
    lipo -create \
        "target/aarch64-apple-darwin/release/libframesw_obs_plugin.dylib" \
        "target/x86_64-apple-darwin/release/libframesw_obs_plugin.dylib" \
        -output "$BUNDLE/Contents/MacOS/framesw-companion"
else
    ARCH="$(uname -m)"
    if [ "$ARCH" = "arm64" ]; then
        TARGET="aarch64-apple-darwin"
    else
        TARGET="x86_64-apple-darwin"
    fi
    echo "Building for $TARGET..."
    cargo build --target "$TARGET"
    cp "target/$TARGET/debug/libframesw_obs_plugin.dylib" "$BUNDLE/Contents/MacOS/framesw-companion"
fi
chmod +x "$BUNDLE/Contents/MacOS/framesw-companion"

cat > "$BUNDLE/Contents/Info.plist" << 'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleExecutable</key>
	<string>framesw-companion</string>
	<key>CFBundleIdentifier</key>
	<string>com.framesw.companion</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>framesw-companion</string>
	<key>CFBundlePackageType</key>
	<string>BNDL</string>
	<key>CFBundleShortVersionString</key>
	<string>0.1.0-phase1-spike</string>
	<key>CFBundleSupportedPlatforms</key>
	<array>
		<string>MacOSX</string>
	</array>
	<key>LSMinimumSystemVersion</key>
	<string>12.0</string>
</dict>
</plist>
EOF

# Ad-hoc sign in dev mode (no real identity needed for a local dev test —
# this isn't being distributed, and code built locally isn't
# Gatekeeper-quarantined the way a downloaded file is, but signing it
# anyway matches how the already-installed real plugins on this machine
# are set up). Release mode signs with the real Developer ID identity
# instead, so it survives being bundled into a notarized FrameSW.app.
codesign --force --sign "$SIGN_IDENTITY" "$BUNDLE/Contents/MacOS/framesw-companion"

echo ""
echo "Built: $BUNDLE"
if [ "$RELEASE_MODE" = "0" ]; then
    echo ""
    echo "To install for testing:"
    echo "  cp -R $BUNDLE ~/Library/Application\ Support/obs-studio/plugins/"
    echo "Then fully quit and relaunch OBS Studio, and check its log"
    echo "(Help > Log Files > View Current Log, or ~/Library/Application Support/obs-studio/logs/)"
    echo "for lines starting with \"[framesw]\"."
fi
