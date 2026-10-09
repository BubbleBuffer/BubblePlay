//! Diagnostic: what does this driver's NVENC actually accept?
//!
//! Prints the max supported API version, opens a session on a fresh D3D11
//! device, then bisects `NvEncGetEncodePresetConfigEx` across struct
//! versions 1..=8 (H.264 + P1 + ULL) to find which one the driver wants.

use nvidia_video_codec_sdk::safe::ENCODE_API;
use nvidia_video_codec_sdk::sys::nvEncodeAPI::{
    GUID, NVENCAPI_VERSION, NV_ENC_CAPS, NV_ENC_CAPS_PARAM, NV_ENC_CAPS_PARAM_VER,
    NV_ENC_DEVICE_TYPE, NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS,
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER, NV_ENC_PRESET_CONFIG, NV_ENC_TUNING_INFO,
};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL};
use windows::Win32::Graphics::Direct3D11::D3D11CreateDevice;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;

const NV_ENC_CODEC_H264_GUID: GUID = GUID {
    Data1: 0x6bc8_2762,
    Data2: 0x4e63,
    Data3: 0x4ca4,
    Data4: [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
};
const NV_ENC_CODEC_HEVC_GUID: GUID = GUID {
    Data1: 0x790c_dc88,
    Data2: 0x4522,
    Data3: 0x4d7b,
    Data4: [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03],
};
const NV_ENC_PRESET_P1_GUID: GUID = GUID {
    Data1: 0xfc0a_8d3e,
    Data2: 0x45f8,
    Data3: 0x4cf8,
    Data4: [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf],
};

/// Zero-init like the C SDK (`memset` + set version). `std::mem::zeroed`
/// trips a validity panic on this union-heavy struct; MaybeUninit works.
unsafe fn zeroed<T>() -> T {
    // Genuine runtime memset via a raw pointer (the C SDK pattern); this is
    // not const-folded into `zeroed`, so no validity panic.
    let layout = std::alloc::Layout::new::<T>();
    let ptr = std::alloc::alloc(layout);
    if ptr.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    std::ptr::write_bytes(ptr, 0, layout.size());
    let val = std::ptr::read(ptr as *const T);
    std::alloc::dealloc(ptr, layout);
    val
}

fn main() {
    // 1. Max supported version.
    let mut max_ver: u32 = 0;
    let st = unsafe {
        nvidia_video_codec_sdk::sys::nvEncodeAPI::NvEncodeAPIGetMaxSupportedVersion(&mut max_ver)
    };
    println!(
        "NvEncodeAPIGetMaxSupportedVersion -> status {st:?}, version {max_ver} (major {}, minor {})",
        max_ver >> 24,
        max_ver & 0xFFFF
    );

    // 2. Fresh D3D11 device.
    let mut device: Option<ID3D11Device> = None;
    let mut level = D3D_FEATURE_LEVEL::default();
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            windows::Win32::Foundation::HMODULE::default(),
            Default::default(),
            None,
            7, // D3D11_SDK_VERSION
            Some(&mut device),
            Some(&mut level),
            None,
        )
        .expect("D3D11CreateDevice");
    }
    let device = device.unwrap();
    println!("D3D11 device created, feature level {:?}", level.0);

    // 3. Open session.
    let mut params: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = unsafe { std::mem::zeroed() };
    params.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
    params.deviceType = NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX;
    params.device = device.as_raw() as *mut core::ffi::c_void;
    params.apiVersion = NVENCAPI_VERSION;
    let mut session: *mut core::ffi::c_void = core::ptr::null_mut();
    let st = unsafe { (ENCODE_API.open_encode_session_ex)(&mut params, &mut session) };
    println!("NvEncOpenEncodeSessionEx -> status {:?}", st);
    if session.is_null() {
        return;
    }

    // This is the decisive capability check for the codec-native feedback
    // experiment: it asks the active driver about HEVC reconstructed-output
    // surfaces rather than inferring support from the SDK header alone.
    {
        let mut caps: NV_ENC_CAPS_PARAM = unsafe { zeroed() };
        caps.version = NV_ENC_CAPS_PARAM_VER;
        caps.capsToQuery = NV_ENC_CAPS::NV_ENC_CAPS_OUTPUT_RECON_SURFACE;
        let mut supported = -1;
        let st = unsafe {
            (ENCODE_API.get_encode_caps)(session, NV_ENC_CODEC_HEVC_GUID, &mut caps, &mut supported)
        };
        println!("GetEncodeCaps(HEVC OUTPUT_RECON_SURFACE) -> {st:?}, supported={supported}");
    }

    // 4. Matrix: (api base) x (struct ver) x (1<<31 flag) for preset config.
    //    a) Enumerate encode codec GUIDs + preset GUIDs the driver knows.
    //    b) Try legacy NvEncGetEncodePresetConfig (no tuningInfo).
    //    c) Try GetEncodePresetConfigEx with SDK 13.0 constants.
    use nvidia_video_codec_sdk::sys::nvEncodeAPI::NVENCSTATUS;

    let mut count = 0u32;
    let st = unsafe { (ENCODE_API.get_encode_guid_count)(session, &mut count) };
    println!("GetEncodeGUIDCount -> {st:?}, count={count}");
    let mut guids = vec![GUID::default(); count as usize];
    let mut got = 0u32;
    let st = unsafe {
        (ENCODE_API.get_encode_guids)(session, guids.as_mut_ptr(), guids.len() as u32, &mut got)
    };
    println!("GetEncodeGUIDs -> {st:?}, got={got}");
    for (i, g) in guids.iter().take(got as usize).enumerate() {
        let label = if *g == NV_ENC_CODEC_H264_GUID {
            " <== H264"
        } else if *g == NV_ENC_CODEC_HEVC_GUID {
            " <== HEVC"
        } else {
            ""
        };
        println!(
            "  encode[{i}] {:08X}-{:04X}-{:04X}{label}",
            g.Data1, g.Data2, g.Data3
        );
    }

    let mut pcount = 0u32;
    let st = unsafe {
        (ENCODE_API.get_encode_preset_count)(session, NV_ENC_CODEC_H264_GUID, &mut pcount)
    };
    println!("GetEncodePresetCount(H264) -> {st:?}, count={pcount}");
    let mut pguids = vec![GUID::default(); pcount as usize];
    let mut pgot = 0u32;
    let st = unsafe {
        (ENCODE_API.get_encode_preset_guids)(
            session,
            NV_ENC_CODEC_H264_GUID,
            pguids.as_mut_ptr(),
            pguids.len() as u32,
            &mut pgot,
        )
    };
    println!("GetEncodePresetGUIDs -> {st:?}, got={pgot}");
    for (i, g) in pguids.iter().take(pgot as usize).enumerate() {
        let label = if *g == NV_ENC_PRESET_P1_GUID {
            " <== P1"
        } else {
            ""
        };
        println!(
            "  preset[{i}] {:08X}-{:04X}-{:04X}-{:02X?}{label}",
            g.Data1, g.Data2, g.Data3, g.Data4
        );
    }

    // Legacy (non-Ex) preset config, SDK 12.1 struct v4.
    {
        let mut cfg: NV_ENC_PRESET_CONFIG = unsafe { zeroed() };
        cfg.version = (0x7 << 28) | (4 << 16) | 0x0100_000C | (1u32 << 31);
        let st = unsafe {
            (ENCODE_API.get_encode_preset_config)(
                session,
                NV_ENC_CODEC_H264_GUID,
                NV_ENC_PRESET_P1_GUID,
                &mut cfg,
            )
        };
        println!("GetEncodePresetConfig(legacy, 12.1 v4) -> {st:?}");
    }

    // Ex variant with SDK 13.0 exact constants.
    {
        let mut cfg: NV_ENC_PRESET_CONFIG = unsafe { zeroed() };
        cfg.version = (0x7 << 28) | (5 << 16) | 0x0D | (1u32 << 31);
        let st = unsafe {
            (ENCODE_API.get_encode_preset_config_ex)(
                session,
                NV_ENC_CODEC_H264_GUID,
                NV_ENC_PRESET_P1_GUID,
                NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                &mut cfg,
            )
        };
        println!("GetEncodePresetConfigEx(13.0 v5 ULL) -> {st:?}");
        let err = unsafe { (ENCODE_API.get_last_error_string)(session) };
        if !err.is_null() {
            let s = unsafe { std::ffi::CStr::from_ptr(err) };
            println!("  last error: {:?}", s);
        }
    }

    // Thorough brute force on preset config version word: base in {0x0D,0xD0},
    // struct-ver 1..=12, f31 both, nibble in {7}. Print first few hits.
    let mut hits = 0;
    for base in [0x0Du32, 0xD0] {
        for v in 1u32..=12 {
            for f31 in [0u32, 1u32 << 31] {
                let version = (0x7 << 28) | (v << 16) | base | f31;
                let mut cfg: NV_ENC_PRESET_CONFIG = unsafe { zeroed() };
                cfg.version = version;
                let st = unsafe {
                    (ENCODE_API.get_encode_preset_config_ex)(
                        session,
                        NV_ENC_CODEC_H264_GUID,
                        NV_ENC_PRESET_P1_GUID,
                        NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                        &mut cfg,
                    )
                };
                if matches!(st, NVENCSTATUS::NV_ENC_SUCCESS) {
                    println!(
                        "BRUTE FORCE HIT: v={v} f31={} base=0x{base:X} version=0x{version:08X} gop={} rc={:?}",
                        if f31 != 0 { 1 } else { 0 },
                        cfg.presetCfg.gopLength,
                        cfg.presetCfg.rcParams.rateControlMode
                    );
                    hits += 1;
                }
            }
        }
    }
    println!("brute force hits: {hits}");

    // Which DLL actually backs these function pointers?
    unsafe {
        use windows::Win32::System::LibraryLoader::{
            GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
            GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        };
        let fp = ENCODE_API.get_encode_preset_config_ex as usize;
        let mut module = windows::Win32::Foundation::HMODULE::default();
        let _ = GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            windows::core::PCWSTR(fp as *const u16),
            &mut module,
        );
        let mut buf = [0u16; 1024];
        let n = GetModuleFileNameW(Some(module), &mut buf);
        let path = String::from_utf16_lossy(&buf[..n as usize]);
        println!("function list backed by module: {path}");
    }

    // Does GetEncodeCaps even check versions? Try base 0xD0 and garbage.
    {
        use nvidia_video_codec_sdk::sys::nvEncodeAPI::{NV_ENC_CAPS, NV_ENC_CAPS_PARAM};
        for (version, label) in [
            ((0x7 << 28) | (1 << 16) | 0xD0u32, "base0xD0"),
            ((0x7 << 28) | (1 << 16) | 0x0D, "base0x0D"),
            (0x1234_5678u32, "garbage"),
            ((0x7 << 28) | (1 << 16) | 0x0100_000C, "crate12.1"),
        ] {
            let mut caps: NV_ENC_CAPS_PARAM = unsafe { zeroed() };
            caps.version = version;
            caps.capsToQuery = NV_ENC_CAPS::NV_ENC_CAPS_NUM_MAX_BFRAMES;
            let mut val: i32 = -1;
            let st = unsafe {
                (ENCODE_API.get_encode_caps)(session, NV_ENC_CODEC_H264_GUID, &mut caps, &mut val)
            };
            println!("caps[{label}] version=0x{version:08X} -> {st:?} val={val}");
        }
    }

    // Preset config with base 0xD0 (=208, the raw max-version number).
    for v in [4u32, 5, 6] {
        for f31 in [0u32, 1u32 << 31] {
            let version = (0x7 << 28) | (v << 16) | 0xD0 | f31;
            let mut cfg: NV_ENC_PRESET_CONFIG = unsafe { zeroed() };
            cfg.version = version;
            let st = unsafe {
                (ENCODE_API.get_encode_preset_config_ex)(
                    session,
                    NV_ENC_CODEC_H264_GUID,
                    NV_ENC_PRESET_P1_GUID,
                    NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                    &mut cfg,
                )
            };
            println!(
                "presetCfg v{v} f31={} base0xD0 -> {st:?}",
                if f31 != 0 { 1 } else { 0 }
            );
        }
    }

    // Sanity: does ANY call with version work? Try caps query.
    {
        use nvidia_video_codec_sdk::sys::nvEncodeAPI::{NV_ENC_CAPS, NV_ENC_CAPS_PARAM};
        let mut caps: NV_ENC_CAPS_PARAM = unsafe { zeroed() };
        caps.version = (0x7 << 28) | (1 << 16) | 0x0D;
        caps.capsToQuery = NV_ENC_CAPS::NV_ENC_CAPS_NUM_MAX_BFRAMES;
        let mut val: i32 = -1;
        let st = unsafe {
            (ENCODE_API.get_encode_caps)(session, NV_ENC_CODEC_H264_GUID, &mut caps, &mut val)
        };
        println!("GetEncodeCaps(NUM_MAX_BFRAMES, 13.0 v1) -> {st:?}, val={val}");
    }

    unsafe { (ENCODE_API.destroy_encoder)(session) };
}
