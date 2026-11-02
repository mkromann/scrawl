# Source this before `cargo build` on a Mac with Command Line Tools and no Xcode (macOS 13):
#
#   source scripts/macos13-env.sh
#
# Those tools ship Swift 5.8, too old for scrawl-helper and for the `swift build` in the
# screencapturekit and apple-cf build scripts, so a newer Swift from swift.org goes first on PATH.
# xcrun ignores TOOLCHAINS and --toolchain without Xcode, and always finds the old swiftc, so
# build.rs takes the toolchain from SCRAWL_SWIFT_TOOLCHAIN instead. Sourced by bash and zsh.

SCRAWL_SWIFT_TOOLCHAIN="${SCRAWL_SWIFT_TOOLCHAIN:-${HOME}/Library/Developer/Toolchains/swift-6.1.3-RELEASE.xctoolchain}"

if [ ! -x "${SCRAWL_SWIFT_TOOLCHAIN}/usr/bin/swiftc" ]; then
  echo "macos13-env: no swiftc in ${SCRAWL_SWIFT_TOOLCHAIN}" >&2
  unset SCRAWL_SWIFT_TOOLCHAIN
  return 1 2>/dev/null || exit 1
fi

export SCRAWL_SWIFT_TOOLCHAIN
case ":${PATH}:" in
  *":${SCRAWL_SWIFT_TOOLCHAIN}/usr/bin:"*) ;;
  *) export PATH="${SCRAWL_SWIFT_TOOLCHAIN}/usr/bin:${PATH}" ;;
esac
# swiftc and SwiftPM from a toolchain outside Xcode take the SDK from here.
SDKROOT="${SDKROOT:-$(xcrun --sdk macosx --show-sdk-path)}"
export SDKROOT
