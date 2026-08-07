//! The NVENC import libs are resolved by the `nvidia-video-codec-sdk` crate's own build
//! script via `NVIDIA_VIDEO_CODEC_SDK_PATH` (set in `.cargo/config.toml`). If you see a
//! "Could not find NVIDIA Video Codec SDK libraries" panic, run:
//!
//!     powershell -File scripts/make-nvenc-stubs.ps1
//!
//! once on this machine (needs Visual Studio's `lib.exe` and the NVIDIA display driver).
fn main() {
    println!("cargo:rerun-if-env-changed=NVIDIA_VIDEO_CODEC_SDK_PATH");
}
