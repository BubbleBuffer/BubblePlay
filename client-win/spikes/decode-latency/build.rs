use std::{env, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    println!("cargo:rerun-if-changed=ffi/ffmpeg_shim.c");
    let target = env::var("TARGET").expect("Cargo TARGET");
    assert!(
        target == "x86_64-pc-windows-msvc",
        "This D3D11 decode spike requires x86_64-pc-windows-msvc"
    );
    let root = PathBuf::from(env::var_os("FFMPEG_DIR").expect(
        "Set process-local FFMPEG_DIR to the FFmpeg 8.1 LGPL shared dev directory containing include/lib/bin",
    ));
    for relative in [
        "include/libavcodec/avcodec.h",
        "include/libavutil/hwcontext_d3d11va.h",
        "lib/avcodec.lib",
        "lib/avutil.lib",
    ] {
        assert!(
            root.join(relative).is_file(),
            "Missing {}",
            root.join(relative).display()
        );
    }
    // cc discovers the installed MSVC toolchain without changing the user's PATH.
    // All FFmpeg struct access happens in C against these exact dev headers.
    cc::Build::new()
        .file("ffi/ffmpeg_shim.c")
        .include(root.join("include"))
        .warnings(true)
        .compile("bubbleplay_ffmpeg_shim");
    println!(
        "cargo:rustc-link-search=native={}",
        root.join("lib").display()
    );
    println!("cargo:rustc-link-lib=dylib=avcodec");
    println!("cargo:rustc-link-lib=dylib=avutil");
}
