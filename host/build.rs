//! The NVENC import libs are resolved by the `nvidia-video-codec-sdk` crate's own build
//! script via `NVIDIA_VIDEO_CODEC_SDK_PATH` (set in `.cargo/config.toml`). If you see a
//! "Could not find NVIDIA Video Codec SDK libraries" panic, run:
//!
//!     powershell -File scripts/make-nvenc-stubs.ps1
//!
//! once on this machine (needs Visual Studio's `lib.exe` and the NVIDIA display driver).
fn main() {
    println!("cargo:rerun-if-env-changed=NVIDIA_VIDEO_CODEC_SDK_PATH");
    println!("cargo:rerun-if-changed=../branding/out/bubbleplay.ico");
    println!("cargo:rerun-if-changed=../branding/out/bubbleplay-tray.ico");
    println!("cargo:rerun-if-env-changed=RC");
    println!("cargo:rerun-if-env-changed=WindowsSdkDir");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    assert_eq!(
        std::env::var("CARGO_CFG_TARGET_ENV").as_deref(),
        Ok("msvc"),
        "BubblePlay icon resources currently require the Windows MSVC toolchain"
    );
    // The SDK uses an import library even though its function table is lazy.
    // Delay every final binary's NVIDIA import so non-NVIDIA PCs can reach
    // local encoder selection. This also covers the privileged-host binary.
    println!("cargo:rustc-link-lib=delayimp");
    // Standalone diagnostic bins do not all link this package's library, so
    // pass the helper library to their final links explicitly as well.
    println!("cargo:rustc-link-arg=delayimp.lib");
    println!("cargo:rustc-link-arg=/DELAYLOAD:nvEncodeAPI64.dll");
    let root = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let icon = root
        .join("../branding/out/bubbleplay.ico")
        .canonicalize()
        .expect("run python branding/build-icons.py before building the host");
    let icon_line = format!(
        "1 ICON \"{}\"\n",
        icon.display().to_string().replace('\\', "/")
    );
    // Resource 2 (tray only): the notification-area icon, the mark without its ivory tile.
    let tray_icon = root
        .join("../branding/out/bubbleplay-tray.ico")
        .canonicalize()
        .expect("run python branding/build-icons.py --followups before building the host");
    // Common Controls 6 is embedded in the tray only, so its dialogs and message boxes get the
    // current Windows look. The host binary keeps its icon-only resource.
    let manifest = out.join("bubbleplay_tray.manifest");
    std::fs::write(&manifest, TRAY_MANIFEST).unwrap();
    let tray_resource = format!(
        "{icon_line}2 ICON \"{}\"\n1 24 \"{}\"\n",
        tray_icon.display().to_string().replace('\\', "/"),
        manifest.display().to_string().replace('\\', "/")
    );
    let host = compile_resource(&out, "bubbleplay-host", &icon_line);
    let tray = compile_resource(&out, "bubbleplay_tray", &tray_resource);
    println!(
        "cargo:rustc-link-arg-bin=bubbleplay-host={}",
        host.display()
    );
    println!(
        "cargo:rustc-link-arg-bin=bubbleplay_tray={}",
        tray.display()
    );
}

/// Side-by-side assembly that loads Common Controls 6 (themed controls and message boxes).
const TRAY_MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*" />
    </dependentAssembly>
  </dependency>
</assembly>
"#;

/// Compiles the resource script `body` as `name` and returns the compiled `.res` path.
fn compile_resource(out: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let resource = out.join(format!("{name}.rc"));
    let compiled = out.join(format!("{name}.res"));
    std::fs::write(&resource, body).unwrap();
    let status = std::process::Command::new(rc_compiler())
        .arg("/nologo")
        .arg("/fo")
        .arg(&compiled)
        .arg(&resource)
        .status()
        .expect("Windows SDK rc.exe is required (or set RC)");
    assert!(
        status.success(),
        "Windows resource compilation failed for {name}"
    );
    compiled
}

/// `RC` if set, otherwise the newest Windows SDK `rc.exe` on this build machine.
fn rc_compiler() -> std::path::PathBuf {
    std::env::var_os("RC")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let sdk = std::env::var_os("WindowsSdkDir")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| {
                    std::path::PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10")
                });
            // rc runs on the build machine, independently of the binary's target architecture.
            let arch = if std::env::consts::ARCH == "aarch64" {
                "arm64"
            } else {
                "x64"
            };
            let mut candidates: Vec<_> = std::fs::read_dir(sdk.join("bin"))
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|entry| entry.path().join(arch).join("rc.exe"))
                .filter(|path| path.is_file())
                .collect();
            candidates.sort();
            candidates.pop().unwrap_or_else(|| "rc.exe".into())
        })
}
