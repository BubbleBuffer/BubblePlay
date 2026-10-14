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
    let resource = out.join("bubbleplay.rc");
    let compiled = out.join("bubbleplay.res");
    std::fs::write(
        &resource,
        format!(
            "1 ICON \"{}\"\n",
            icon.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();
    let compiler = std::env::var_os("RC")
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
        });
    let status = std::process::Command::new(compiler)
        .arg("/nologo")
        .arg("/fo")
        .arg(&compiled)
        .arg(&resource)
        .status()
        .expect("Windows SDK rc.exe is required (or set RC)");
    assert!(status.success(), "Windows icon resource compilation failed");
    for binary in ["bubbleplay-host", "bubbleplay_tray"] {
        println!("cargo:rustc-link-arg-bin={binary}={}", compiled.display());
    }
}
