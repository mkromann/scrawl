# Scrawl on macOS 13

This fork of [kitlangton/scrawl](https://github.com/kitlangton/scrawl) runs on macOS 13 Ventura on Apple silicon, built with Command Line Tools and no Xcode. Upstream needs macOS 15 and the Xcode `metal` tool. The aim is the same app with no noticeable loss, and a small diff so upstream merges cleanly.

## Set up a fresh Ventura Mac

No sudo is needed after Homebrew. The first build fills about 4.5 GB in `target/`. Scrawl downloads a 731 MB speech model at first launch.

```sh
xcode-select --install                # Command Line Tools: SDK 13.3, too old a Swift
curl -LO https://download.swift.org/swift-6.1.3-release/xcode/swift-6.1.3-RELEASE/swift-6.1.3-RELEASE-osx.pkg
installer -pkg swift-6.1.3-RELEASE-osx.pkg -target CurrentUserHomeDirectory   # 1.5 GB, into ~/Library/Developer/Toolchains
curl -sSf https://sh.rustup.rs | sh
brew install cmake                    # Parakeet's build needs it

git clone https://github.com/mkromann/scrawl
cd scrawl
./scripts/install-app.sh
```

That builds, signs and opens `~/Applications/Scrawl.app`. Grant Screen Recording, Microphone and Accessibility when it asks. For a build by hand, run `source scripts/macos13-env.sh` first: it puts the new Swift on `PATH`.

`/Applications/Scrawl.app`, if you have upstream's there, won't launch on Ventura. Open the one in `~/Applications`.

## What differs from upstream

- **Build:** `scripts/macos13-env.sh`, `vendor/` (see `vendor/README.md`), `helper/SDKCompat.swift` for SwiftUI and AVFoundation newer than the SDK, and Metal shaders compiled at launch, as there's no `metal` tool.
- **Recording:** macOS 13 has no `SCRecordingOutput` or `SCScreenshotManager`. `src/capture/sck_compat.rs` keeps those names on `SCStream` and `AVAssetWriter` (`docs/capture.md`). If the process dies, `scrawl recover` builds a bundle from the movie's fragments.
- **Transcription:** macOS 13 has no SpeechAnalyzer. English goes to Parakeet on Metal in a child process (`docs/keyframes-and-text.md`). Other languages go to SFSpeechRecognizer.
- **Identity:** the app is `dev.kromann.scrawl`, the helper `dev.kromann.scrawl.helper`, so macOS can't confuse them with upstream's app. Settings › About's Source link points here. The website link and the agent skill stay upstream's, as the skill's content is the same.
- **Signing:** `scripts/setup-signing.sh` makes a self-signed code-signing certificate in `~/Library/Keychains/scrawl-signing.keychain-db`, adds that keychain to your search list, and keeps its password beside it in `scrawl-signing.password` (mode 600). The login keychain isn't touched. The signature's designated requirement names the certificate rather than the build, so permissions survive rebuilds. If the script fails, `install-app.sh` warns and signs ad hoc, and macOS forgets the permissions at each rebuild. Delete that keychain and macOS asks for them again.
- **Updates:** no Sparkle in the bundle and no feed in `Info.plist`, as an update from upstream's appcast would replace this build with one that can't run here. Check for Updates… doesn't show.

`scripts/release-app.sh`, `scripts/install-app-remote.sh`, `scripts/setup-sparkle.sh` and `docs/release.md` are upstream's release tooling, left as they are apart from a note at the top of the first two. They still use Kit's Developer ID, `dev.kitlangton.scrawl`, Sparkle and macOS 15. This fork doesn't use them. The same goes for upstream's DMG and Homebrew cask.

## Pull upstream

Merge, never rebase: `main` keeps upstream's history, so each merge brings only what's new.

```sh
git fetch upstream
git merge upstream/main
source scripts/macos13-env.sh
taskpolicy -c utility cargo test -j 4 -- --test-threads=2
./scripts/install-app.sh
```

In a conflict, keep these:

- `scripts/install-app.sh`: the `dev.kromann` ids, `LSMinimumSystemVersion` 13.0, no `SU*` keys, no Sparkle, `setup-signing.sh`.
- `Cargo.toml`: `macos_13_0` on `screencapturekit`, `runtime_shaders` on `gpui`, `[patch.crates-io]`.

After a merge, new Apple API can fail the build against SDK 13.3. Add it to `helper/SDKCompat.swift` or `sck_compat` under the SDK's name, so the call sites stay upstream's.

## Known differences

- Movies are hardware H.264 from `AVAssetWriter`, about 12 Mbps at 2560 px.
- Live transcription (`liveTranscribe`) and Liquid Glass need macOS 26, so they're off.
- Parakeet takes 830 MB to 1.1 GB while it transcribes, gone when the child exits. It knows only English and spells it American.
- The first Parakeet run under a new binary path compiles Metal shaders for about 20 s. A background run does this after install, so only a capture finished in those first seconds waits.
- A language other than English needs the Speech Recognition permission and Dictation's model.
- To update, pull and run `install-app.sh`. Nothing tells you there's a new version.
- The app isn't notarized and the certificate isn't trusted, so it's for the Mac that built it. Anything running as you can read the keychain password and sign code that macOS treats as Scrawl, with its permissions.
- The Windows code is unchanged in intent but isn't built or tested here.
