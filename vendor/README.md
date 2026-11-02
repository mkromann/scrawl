# vendor

Crates patched in through `[patch.crates-io]` in `Cargo.toml` so Scrawl builds with Command Line
Tools and the macOS 13 SDK (`scripts/macos13-env.sh`). `screencapturekit` needs both.

- `apple-cf`: 0.11.0 from crates.io, without its tests and examples and with a manifest that
  resolves outside its monorepo. One source change: `acf_dispatch_async_and_wait_f` in
  `swift-bridge/Sources/DispatchBridge/DispatchExtras.swift` calls
  `asyncAndWait(execute: DispatchWorkItem { ... })`, because the closure form is newer than this SDK.
- `apple-metal`: not the crate. Its Swift bridge uses Metal API from the macOS 14 to 26 SDKs with
  no way to leave it out, and `screencapturekit` only needs it for
  `MetalDevice::as_apple_metal`, which Scrawl never calls. `lib.rs` is that function's types. The
  real crate's build script was also the only one that told the linker where the Swift libraries
  are; `link_swift_libraries` in `build.rs` does that now, with or without Xcode.

When `screencapturekit` moves to other versions of these, cargo warns that the patch is unused:
redo the change above for the new version or drop it.
