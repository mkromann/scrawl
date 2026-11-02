use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rustc-check-cfg=cfg(scrawl_runtime_shaders)");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_windows_resources();
        stage_transcribe_dlls();
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    compile_laser_shader();
    compile_helper();
    link_swift_libraries();
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    if let Ok(output) = Command::new("xcode-select").arg("-p").output()
        && output.status.success()
        && let Ok(developer_dir) = String::from_utf8(output.stdout)
    {
        for runtime in ["swift-5.5/macosx", "swift/macosx"] {
            let swift_runtime = format!(
                "{}/Toolchains/XcodeDefault.xctoolchain/usr/lib/{runtime}",
                developer_dir.trim()
            );
            println!("cargo:rustc-link-arg=-Wl,-rpath,{swift_runtime}");
        }
    }
}

/// transcribe.cpp's DLLs (`dynamic-backends`: `transcribe.dll`, `ggml*.dll`) beside `scrawl.exe`
/// and the test binaries, which load them at start. Its own build script copies them there too,
/// but only when it runs, and a build restored from CI's cache skips it.
#[cfg(windows)]
fn stage_transcribe_dlls() {
    println!("cargo:rerun-if-env-changed=DEP_TRANSCRIBE_CPP_RUNTIME_DIR");
    let Some(runtime) = std::env::var_os("DEP_TRANSCRIBE_CPP_RUNTIME_DIR") else {
        return;
    };
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    // OUT_DIR is <target>/<profile>/build/scrawl-<hash>/out.
    let Some(profile) = out_dir.ancestors().nth(3) else {
        return;
    };
    let dlls: Vec<PathBuf> = std::fs::read_dir(&runtime)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "dll"))
        .collect();
    for dir in [profile.to_path_buf(), profile.join("deps")] {
        let _ = std::fs::create_dir_all(&dir);
        for dll in &dlls {
            if let Err(error) = std::fs::copy(dll, dir.join(dll.file_name().unwrap())) {
                println!("cargo:warning=could not copy {}: {error}", dll.display());
            }
        }
    }
}

/// The app icon (Explorer, the taskbar, Alt+Tab, and each window's title bar icon) and version
/// info. GPUI's `windows-manifest` feature embeds the DPI-aware manifest, so none is added here.
#[cfg(windows)]
fn embed_windows_resources() {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let output_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let icon = manifest_dir.join("assets/Scrawl.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let numeric = version
        .split(['.', '-'])
        .take(3)
        .map(|part| part.parse::<u16>().unwrap_or(0).to_string())
        .chain(std::iter::repeat("0".to_owned()))
        .take(4)
        .collect::<Vec<_>>()
        .join(",");
    let script = format!(
        r#"#include <winver.h>
1 ICON "{icon}"
1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
FILEOS VOS_NT_WINDOWS32
FILETYPE VFT_APP
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "FileDescription", "Scrawl"
      VALUE "ProductName", "Scrawl"
      VALUE "FileVersion", "{version}"
      VALUE "ProductVersion", "{version}"
      VALUE "OriginalFilename", "scrawl.exe"
      VALUE "InternalName", "scrawl"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        icon = icon.display().to_string().replace('\\', "/")
    );
    let resource = output_dir.join("scrawl.rc");
    std::fs::write(&resource, script).expect("write the Windows resource script");
    embed_resource::compile(&resource, embed_resource::NONE)
        .manifest_optional()
        .expect("compile the Windows resources");
}

/// The swift.org toolchain `scripts/macos13-env.sh` chooses on a Mac without Xcode, where xcrun
/// ignores TOOLCHAINS and only ever finds Command Line Tools' older Swift. None with Xcode.
fn swift_toolchain() -> Option<PathBuf> {
    println!("cargo:rerun-if-env-changed=SCRAWL_SWIFT_TOOLCHAIN");
    std::env::var_os("SCRAWL_SWIFT_TOOLCHAIN").map(PathBuf::from)
}

/// The Swift bridges autolink the Swift libraries by name, and the linker doesn't look in the
/// SDK's directory by itself. The real `apple-metal` used to add the Xcode toolchain's (see
/// `vendor/README.md`).
fn link_swift_libraries() {
    let sdk = Command::new("xcrun")
        .args(["--sdk", "macosx", "--show-sdk-path"])
        .output()
        .expect("xcrun is required to find the macOS SDK");
    let sdk = String::from_utf8_lossy(&sdk.stdout);
    println!(
        "cargo:rustc-link-search=native={}/usr/lib/swift",
        sdk.trim()
    );
}

/// The major version of the macOS SDK. An SDK that can't be read counts as a current one, which
/// is what the build expected before it asked.
fn sdk_major() -> u32 {
    Command::new("xcrun")
        .args(["--sdk", "macosx", "--show-sdk-version"])
        .output()
        .ok()
        .and_then(|output| {
            let version = String::from_utf8(output.stdout).ok()?;
            version.trim().split('.').next()?.parse().ok()
        })
        .unwrap_or(26)
}

fn compile_laser_shader() {
    // Command Line Tools have no `metal`, so the shaders ship as source (`native::shaders`).
    let has_metal = Command::new("xcrun")
        .args(["-f", "metal"])
        .output()
        .is_ok_and(|output| output.status.success());
    if !has_metal {
        println!("cargo:rustc-cfg=scrawl_runtime_shaders");
        return;
    }
    for (src_rel, name) in [
        ("src/laser_overlay.metal", "laser_overlay"),
        ("src/native/pill/pill.metal", "pill"),
    ] {
        compile_metal_shader(src_rel, name);
    }
}

fn compile_metal_shader(src_rel: &str, name: &str) {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let output_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let source = manifest_dir.join(src_rel);
    let air = output_dir.join(format!("{name}.air"));
    let library = output_dir.join(format!("{name}.metallib"));
    println!("cargo:rerun-if-changed={}", source.display());

    let metal = Command::new("xcrun")
        .args([
            "--sdk",
            "macosx",
            "metal",
            "-c",
            "-mmacosx-version-min=15.0",
        ])
        .arg(&source)
        .arg("-o")
        .arg(&air)
        .status()
        .expect("xcrun metal is required to build Metal shaders");
    assert!(
        metal.success(),
        "Metal shader compilation failed for {src_rel}"
    );

    let metallib = Command::new("xcrun")
        .args(["--sdk", "macosx", "metallib"])
        .arg(&air)
        .arg("-o")
        .arg(&library)
        .status()
        .expect("xcrun metallib is required to build Metal shaders");
    assert!(
        metallib.success(),
        "Metal library creation failed for {src_rel}"
    );
}

/// Builds `scrawl-helper` (Swift: SpeechAnalyzer, iPhone capture, movie decoding) beside the
/// `scrawl` binary, where `helper::path` and `scripts/install-app.sh` look for it.
fn compile_helper() {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let helper_dir = manifest_dir.join("helper");
    println!("cargo:rerun-if-changed={}", helper_dir.display());
    let mut sources: Vec<PathBuf> = std::fs::read_dir(&helper_dir)
        .expect("helper/ is missing")
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "swift"))
        .collect();
    sources.sort();
    for source in &sources {
        println!("cargo:rerun-if-changed={}", source.display());
    }
    let plist = helper_dir.join("Info.plist");
    println!("cargo:rerun-if-changed={}", plist.display());
    // OUT_DIR is target/<profile>/build/scrawl-<hash>/out.
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let profile_dir = out_dir
        .ancestors()
        .nth(3)
        .expect("Unexpected OUT_DIR layout");
    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        Ok(other) => other,
        Err(_) => "arm64",
    }
    .to_owned();
    let built = out_dir.join("scrawl-helper");
    let mut swiftc = match swift_toolchain() {
        Some(toolchain) => Command::new(toolchain.join("usr/bin/swiftc")),
        None => {
            let mut xcrun = Command::new("xcrun");
            xcrun.arg("swiftc");
            xcrun
        }
    };
    // Before the macOS 14 SDK the helper builds with `SDKCompat.swift`, for macOS 13.
    let sdk = sdk_major();
    let minimum = if sdk >= 14 { "15.0" } else { "13.0" };
    for (define, since) in [("SCRAWL_SDK_14", 14), ("SCRAWL_SDK_26", 26)] {
        if sdk >= since {
            swiftc.arg(format!("-D{define}"));
        }
    }
    let status = swiftc
        .args(["-O", "-swift-version", "5", "-target"])
        .arg(format!("{arch}-apple-macos{minimum}"))
        .args(&sources)
        .arg("-o")
        .arg(&built)
        .args([
            "-Xlinker",
            "-sectcreate",
            "-Xlinker",
            "__TEXT",
            "-Xlinker",
            "__info_plist",
            "-Xlinker",
        ])
        .arg(&plist)
        .status()
        .expect("xcrun swiftc is required to build scrawl-helper");
    assert!(
        status.success(),
        "scrawl-helper failed to compile (needs the macOS 26 SDK for all of it; on macOS 13 \
         source scripts/macos13-env.sh first)"
    );
    let dest = profile_dir.join("scrawl-helper");
    let _ = std::fs::remove_file(&dest);
    std::fs::copy(&built, dest).expect("Could not place scrawl-helper");
}
