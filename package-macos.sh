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
mkdir -p "$BUNDLE/Contents/MacOS" "$BUNDLE/Contents/Resources"

# Single source of truth for Info.plist's CFBundleShortVersionString —
# previously that field was hand-typed and had already drifted from this
# value.
VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/version = "(.*)"/\1/')"

# The plain-text VERSION file FrameSW's app actually compares
# (platform::installed_plugin_version / plugin_update_available) is a
# *separate*, richer stamp: Cargo.toml's version alone relies on someone
# remembering to bump it every time the plugin's behavior changes, and
# in practice that didn't happen for 6 real feature/fix commits in a
# row (all shipped as "0.1.0") — so FrameSW never once detected a stale
# installed copy. Appending this repo's own commit hash (plus a -dirty
# marker for uncommitted local changes) makes every real code change
# produce a different stamp automatically, with no bump discipline
# required at all.
GIT_SHA="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
GIT_DIRTY=""
if ! git diff --quiet 2>/dev/null || ! git diff --cached --quiet 2>/dev/null; then
    GIT_DIRTY="-dirty"
fi
STAMP="${VERSION}+${GIT_SHA}${GIT_DIRTY}"
echo -n "$STAMP" > "$BUNDLE/Contents/Resources/VERSION"

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

cat > "$BUNDLE/Contents/Info.plist" << EOF
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
	<string>$VERSION</string>
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
# instead, so it survives being bundled into a notarized FrameSW.app —
# `--options runtime --timestamp` (hardened runtime + a real Apple
# secure timestamp) only in that mode: this binary can never hold its
# own stapled notarization ticket (no `stapler staple` target for a
# bare dylib/bundle without an Info.plist executable entry point), it
# rides the app bundle's notarization once FrameSW.app itself is
# notarized — but that only accepts a nested binary whose own signature
# is already notarization-ready, which requires both flags. Omitted in
# ad-hoc dev mode: meaningless without a real Developer ID identity, and
# unnecessary for a local test install.
if [ "$RELEASE_MODE" = "1" ]; then
    codesign --force --options runtime --timestamp --sign "$SIGN_IDENTITY" "$BUNDLE/Contents/MacOS/framesw-companion"
else
    codesign --force --sign "$SIGN_IDENTITY" "$BUNDLE/Contents/MacOS/framesw-companion"
fi

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
