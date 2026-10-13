//! Bounded synthetic colour measurement. No desktop capture or host session.
//!
//! Encodes six identical eight-patch frames through unchanged production NVENC
//! options, parses SPS colour signalling, and decodes to raw NV12 with a
//! synchronous Media Foundation decoder. `--output <directory>` is required.
//! The raw YUV samples, rather than decoder-inferred colour attributes, identify
//! the encoder's actual RGB-to-YUV matrix and range.

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::ptr;

use anyhow::{bail, ensure, Context, Result};
use bubbleplay_host::encode::{EncoderOptions, Nvenc, Preset, VideoCodec};
use serde::Serialize;
use windows::core::{Interface, GUID};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED,
};

#[derive(Clone, Copy)]
struct Geometry {
    width: u32,
    height: u32,
}

#[derive(Clone, Copy)]
enum Fixture {
    Patches,
    Solid(&'static str, [u8; 3]),
}
const FRAMES: usize = 6;
const FPS: u32 = 30;
const PATCHES: [(&str, [u8; 3]); 8] = [
    ("black", [0, 0, 0]),
    ("white", [255, 255, 255]),
    ("red", [255, 0, 0]),
    ("green", [0, 255, 0]),
    ("blue", [0, 0, 255]),
    ("gray", [128, 128, 128]),
    ("cyan", [0, 255, 255]),
    ("magenta", [255, 0, 255]),
];

#[derive(Debug, Default, Clone, Serialize)]
struct Vui {
    vui_present: bool,
    video_signal_type_present: bool,
    video_format: Option<u32>,
    video_full_range_flag: Option<bool>,
    colour_description_present: Option<bool>,
    colour_primaries: Option<u32>,
    transfer_characteristics: Option<u32>,
    matrix_coefficients: Option<u32>,
    // Annex E defaults when the fields are absent.
    effective_full_range: bool,
    effective_matrix: u32,
}

struct Bits {
    bytes: Vec<u8>,
    pos: usize,
}
impl Bits {
    fn new(nal: &[u8], header: usize) -> Self {
        let mut bytes = Vec::new();
        let mut zeros = 0;
        for &b in &nal[header..] {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            bytes.push(b);
            zeros = if b == 0 { zeros + 1 } else { 0 };
        }
        Self { bytes, pos: 0 }
    }
    fn get(&mut self, n: usize) -> Result<u32> {
        ensure!(
            n <= 32 && self.pos + n <= self.bytes.len() * 8,
            "truncated SPS at bit {} (+{n})",
            self.pos
        );
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | u32::from((self.bytes[self.pos / 8] >> (7 - self.pos % 8)) & 1);
            self.pos += 1;
        }
        Ok(v)
    }
    fn flag(&mut self) -> Result<bool> {
        Ok(self.get(1)? != 0)
    }
    fn skip(&mut self, n: usize) -> Result<()> {
        for chunk in (0..n).step_by(32) {
            self.get((n - chunk).min(32))?;
        }
        Ok(())
    }
    fn ue(&mut self) -> Result<u32> {
        let mut zeros = 0;
        while !self.flag()? {
            zeros += 1;
            ensure!(zeros < 31, "oversized SPS Exp-Golomb");
        }
        Ok((1u32 << zeros) - 1 + self.get(zeros)?)
    }
    fn se(&mut self) -> Result<i32> {
        let v = self.ue()?;
        Ok(if v & 1 != 0 {
            (v / 2 + 1) as i32
        } else {
            -(v as i32 / 2)
        })
    }
}

fn annex_b_nals(bytes: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let len = if bytes[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if bytes[i..].starts_with(&[0, 0, 1]) {
            3
        } else {
            i += 1;
            continue;
        };
        starts.push((i, i + len));
        i += len;
    }
    starts
        .iter()
        .enumerate()
        .filter_map(|(i, (_, payload))| {
            let end = starts.get(i + 1).map_or(bytes.len(), |(start, _)| *start);
            (end > *payload).then_some(&bytes[*payload..end])
        })
        .collect()
}

fn vui_prefix(b: &mut Bits) -> Result<Vui> {
    let mut v = Vui {
        vui_present: true,
        effective_matrix: 2,
        ..Vui::default()
    };
    if b.flag()? {
        let aspect = b.get(8)?;
        if aspect == 255 {
            b.skip(32)?;
        }
    }
    if b.flag()? {
        b.skip(1)?;
    }
    v.video_signal_type_present = b.flag()?;
    if v.video_signal_type_present {
        v.video_format = Some(b.get(3)?);
        let full = b.flag()?;
        v.video_full_range_flag = Some(full);
        v.effective_full_range = full;
        let colour = b.flag()?;
        v.colour_description_present = Some(colour);
        if colour {
            v.colour_primaries = Some(b.get(8)?);
            v.transfer_characteristics = Some(b.get(8)?);
            let matrix = b.get(8)?;
            v.matrix_coefficients = Some(matrix);
            v.effective_matrix = matrix;
        }
    }
    Ok(v)
}

fn h264_vui(nal: &[u8]) -> Result<Vui> {
    let mut b = Bits::new(nal, 1);
    let profile = b.get(8)?;
    b.skip(16)?;
    b.ue()?;
    if [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].contains(&profile) {
        let chroma = b.ue()?;
        ensure!(chroma <= 3, "invalid SPS chroma format");
        if chroma == 3 {
            b.skip(1)?;
        }
        b.ue()?;
        b.ue()?;
        b.skip(1)?;
        if b.flag()? {
            for i in 0..if chroma == 3 { 12 } else { 8 } {
                if b.flag()? {
                    let mut last = 8;
                    let mut next = 8;
                    for _ in 0..if i < 6 { 16 } else { 64 } {
                        if next != 0 {
                            next = (last + b.se()? + 256) % 256;
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    b.ue()?;
    match b.ue()? {
        0 => {
            b.ue()?;
        }
        1 => {
            b.skip(1)?;
            b.se()?;
            b.se()?;
            let n = b.ue()?;
            ensure!(n < 256, "POC cycle too long");
            for _ in 0..n {
                b.se()?;
            }
        }
        2 => {}
        _ => bail!("invalid pic_order_cnt_type"),
    }
    b.ue()?;
    b.skip(1)?;
    b.ue()?;
    b.ue()?;
    if !b.flag()? {
        b.skip(1)?;
    }
    b.skip(1)?;
    if b.flag()? {
        for _ in 0..4 {
            b.ue()?;
        }
    }
    if b.flag()? {
        vui_prefix(&mut b)
    } else {
        Ok(Vui {
            effective_matrix: 2,
            ..Vui::default()
        })
    }
}

fn hevc_vui(nal: &[u8]) -> Result<Vui> {
    let mut b = Bits::new(nal, 2);
    b.skip(4)?;
    let layers = b.get(3)? as usize;
    b.skip(1)?;
    // profile_tier_level(profilePresentFlag = 1): general syntax is 96 bits.
    b.skip(96)?;
    let mut sub = Vec::new();
    for _ in 0..layers {
        sub.push((b.flag()?, b.flag()?));
    }
    if layers > 0 {
        b.skip((8 - layers) * 2)?;
    }
    for (profile, level) in sub {
        if profile {
            b.skip(88)?;
        }
        if level {
            b.skip(8)?;
        }
    }
    b.ue()?;
    let chroma = b.ue()?;
    ensure!(chroma <= 3, "invalid HEVC chroma format");
    if chroma == 3 {
        b.skip(1)?;
    }
    b.ue()?;
    b.ue()?;
    if b.flag()? {
        for _ in 0..4 {
            b.ue()?;
        }
    }
    b.ue()?;
    b.ue()?;
    let poc_bits = b.ue()? as usize + 4;
    ensure!(poc_bits <= 32, "invalid HEVC POC bit count");
    let ordering_all = b.flag()?;
    for _ in if ordering_all { 0 } else { layers }..=layers {
        b.ue()?;
        b.ue()?;
        b.ue()?;
    }
    for _ in 0..6 {
        b.ue()?;
    }
    if b.flag()? && b.flag()? {
        for size in 0..4 {
            for _matrix in (0..6).step_by(if size == 3 { 3 } else { 1 }) {
                if !b.flag()? {
                    b.ue()?;
                } else {
                    if size > 1 {
                        b.se()?;
                    }
                    for _ in 0..(1usize << (4 + 2 * size)).min(64) {
                        b.se()?;
                    }
                }
            }
        }
    }
    b.skip(2)?;
    if b.flag()? {
        b.skip(8)?;
        b.ue()?;
        b.ue()?;
        b.skip(1)?;
    }
    let sets = b.ue()? as usize;
    ensure!(sets <= 64, "too many HEVC short-term reference sets");
    let mut deltas = Vec::new();
    for i in 0..sets {
        let inter = i > 0 && b.flag()?;
        let mut count = 0;
        if inter {
            b.skip(1)?;
            b.ue()?;
            for _ in 0..=deltas[i - 1] {
                let used = b.flag()?;
                let use_delta = if used { true } else { b.flag()? };
                if use_delta {
                    count += 1;
                }
            }
        } else {
            let neg = b.ue()?;
            let pos = b.ue()?;
            ensure!(neg + pos <= 64, "too many HEVC reference deltas");
            for _ in 0..neg + pos {
                b.ue()?;
                b.skip(1)?;
            }
            count = neg + pos;
        }
        deltas.push(count);
    }
    if b.flag()? {
        let n = b.ue()?;
        ensure!(n <= 32, "too many long-term refs");
        for _ in 0..n {
            b.skip(poc_bits + 1)?;
        }
    }
    b.skip(2)?;
    if b.flag()? {
        vui_prefix(&mut b)
    } else {
        Ok(Vui {
            effective_matrix: 2,
            ..Vui::default()
        })
    }
}

fn parse_vui(data: &[u8], codec: VideoCodec) -> Result<Vui> {
    for nal in annex_b_nals(data) {
        match codec {
            VideoCodec::H264 if nal[0] & 31 == 7 => return h264_vui(nal),
            VideoCodec::Hevc if (nal[0] >> 1) & 63 == 33 => return hevc_vui(nal),
            _ => {}
        }
    }
    bail!("Annex-B stream has no SPS")
}

fn create_device() -> Result<ID3D11Device> {
    let mut device = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    }
    .context("D3D11CreateDevice")?;
    device.context("missing D3D11 device")
}

fn create_texture(device: &ID3D11Device, geometry: Geometry) -> Result<ID3D11Texture2D> {
    create_fixture_texture(device, geometry, Fixture::Patches)
}

fn create_fixture_texture(
    device: &ID3D11Device,
    geometry: Geometry,
    fixture: Fixture,
) -> Result<ID3D11Texture2D> {
    let Geometry { width, height } = geometry;
    let mut pixels = vec![0; (width * height * 4) as usize];
    for y in 0..height {
        for x in 0..width {
            let rgb = match fixture {
                Fixture::Patches => PATCHES[(y * 2 / height * 4 + x * 4 / width) as usize].1,
                Fixture::Solid(_, rgb) => rgb,
            };
            let at = ((y * width + x) * 4) as usize;
            pixels[at..at + 4].copy_from_slice(&[rgb[2], rgb[1], rgb[0], 255]);
        }
    }
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: 0,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let initial = D3D11_SUBRESOURCE_DATA {
        pSysMem: pixels.as_ptr().cast::<c_void>(),
        SysMemPitch: width * 4,
        SysMemSlicePitch: width * height * 4,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, Some(&initial), Some(&mut texture)) }
        .context("CreateTexture2D patches")?;
    texture.context("missing patch texture")
}

fn enumerate_decoder(codec: VideoCodec) -> Result<(IMFTransform, GUID)> {
    let filter = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype(codec),
    };
    let mut array = ptr::null_mut();
    let mut count = 0;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&filter),
            None,
            &mut array,
            &mut count,
        )
    }
    .context("enumerate synchronous MF decoders")?;
    let mut selected = None;
    if !array.is_null() {
        for activation in unsafe { std::slice::from_raw_parts_mut(array, count as usize) } {
            if let Some(a) = activation.take() {
                if selected.is_none() {
                    let clsid =
                        unsafe { a.GetGUID(&MFT_TRANSFORM_CLSID_Attribute) }.unwrap_or_default();
                    if let Ok(t) = unsafe { a.ActivateObject::<IMFTransform>() } {
                        selected = Some((t, clsid));
                    }
                }
            }
        }
        unsafe { CoTaskMemFree(Some(array.cast())) };
    }
    selected.context(format!("no activatable synchronous MF {} decoder (enumerated {count}); HEVC extension may be absent", name(codec)))
}
fn subtype(codec: VideoCodec) -> GUID {
    match codec {
        VideoCodec::H264 => MFVideoFormat_H264,
        VideoCodec::Hevc => MFVideoFormat_HEVC,
    }
}
fn name(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => "h264",
        VideoCodec::Hevc => "hevc",
    }
}

unsafe fn choose_nv12(transform: &IMFTransform) -> Result<()> {
    for i in 0..64 {
        let media = match unsafe { transform.GetOutputAvailableType(0, i) } {
            Ok(t) => t,
            Err(_) => break,
        };
        if unsafe { media.GetGUID(&MF_MT_SUBTYPE) }? == MFVideoFormat_NV12 {
            unsafe { transform.SetOutputType(0, &media, 0) }.context("set decoder NV12 output")?;
            return Ok(());
        }
    }
    bail!("MF decoder did not expose NV12")
}

unsafe fn sample_with_bytes(bytes: &[u8], time: i64) -> Result<IMFSample> {
    let sample = unsafe { MFCreateSample() }?;
    let buffer = unsafe { MFCreateMemoryBuffer(bytes.len() as u32) }?;
    let mut data = ptr::null_mut();
    unsafe { buffer.Lock(&mut data, None, None) }?;
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(bytes.len() as u32)?;
    }
    unsafe {
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(time)?;
        sample.SetSampleDuration(10_000_000 / FPS as i64)?;
    }
    Ok(sample)
}

#[derive(Serialize)]
struct Decoded {
    decoder_clsid: String,
    width: u32,
    height: u32,
    active_width: u32,
    active_height: u32,
    minimum_display_aperture: Option<[i32; 4]>,
    geometric_aperture: Option<[i32; 4]>,
    stride: u32,
    buffer_length: usize,
    mf_yuv_matrix: Option<u32>,
    mf_nominal_range: Option<u32>,
    patches: Vec<PatchMeasurement>,
    candidate_errors: Vec<(String, f64)>,
    inferred_matrix_range: String,
    #[serde(skip)]
    bytes: Vec<u8>,
}
#[derive(Serialize)]
struct PatchMeasurement {
    name: &'static str,
    rgb: [u8; 3],
    ycbcr: [f64; 3],
}

unsafe fn pull_output(
    transform: &IMFTransform,
    geometry: Geometry,
) -> Result<Option<(Vec<u8>, IMFMediaType)>> {
    for _ in 0..8 {
        let info = unsafe { transform.GetOutputStreamInfo(0) }?;
        let supplied = if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 == 0 {
            let sample = unsafe { MFCreateSample() }?;
            let buffer = unsafe {
                MFCreateAlignedMemoryBuffer(
                    info.cbSize.max(geometry.width * geometry.height * 3 / 2),
                    info.cbAlignment.saturating_sub(1),
                )
            }?;
            unsafe { sample.AddBuffer(&buffer) }?;
            Some(sample)
        } else {
            None
        };
        let mut out = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(supplied),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        };
        let mut status = 0;
        let result =
            unsafe { transform.ProcessOutput(0, std::slice::from_mut(&mut out), &mut status) };
        let sample = unsafe { ManuallyDrop::take(&mut out.pSample) };
        drop(unsafe { ManuallyDrop::take(&mut out.pEvents) });
        match result {
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                unsafe { choose_nv12(transform) }?;
                continue;
            }
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
            Err(e) => return Err(e).context("MF decoder ProcessOutput"),
            Ok(()) => {
                let sample = sample.context("decoder returned no sample")?;
                let buffer = unsafe { sample.ConvertToContiguousBuffer() }?;
                let mut pointer = ptr::null_mut();
                let mut len = 0;
                unsafe { buffer.Lock(&mut pointer, None, Some(&mut len)) }?;
                let bytes = unsafe { std::slice::from_raw_parts(pointer, len as usize).to_vec() };
                unsafe { buffer.Unlock() }?;
                let media = unsafe { transform.GetOutputCurrentType(0) }?;
                return Ok(Some((bytes, media)));
            }
        }
    }
    bail!("decoder exceeded bounded stream-change attempts")
}

fn expected(rgb: [u8; 3], kr: f64, kb: f64, limited: bool) -> [f64; 3] {
    let [r, g, b] = rgb.map(f64::from);
    let y = kr * r + (1.0 - kr - kb) * g + kb * b;
    let cb = (b - y) / (2.0 * (1.0 - kb));
    let cr = (r - y) / (2.0 * (1.0 - kr));
    if limited {
        [
            16.0 + y * 219.0 / 255.0,
            128.0 + cb * 224.0 / 255.0,
            128.0 + cr * 224.0 / 255.0,
        ]
    } else {
        [
            y,
            (128.0 + cb).clamp(0.0, 255.0),
            (128.0 + cr).clamp(0.0, 255.0),
        ]
    }
}

fn measure(
    bytes: Vec<u8>,
    media: &IMFMediaType,
    clsid: GUID,
    geometry: Geometry,
    fixture: Fixture,
) -> Result<Decoded> {
    let size = unsafe { media.GetUINT64(&MF_MT_FRAME_SIZE) }?;
    let width = (size >> 32) as u32;
    let height = size as u32;
    // Decoders may expose coded alignment (HEVC 720 -> 736, H264 1080 -> 1088).
    // NV12's UV plane follows storage height; sample only the known active area.
    ensure!(
        width >= geometry.width
            && width < geometry.width + 64
            && height >= geometry.height
            && height < geometry.height + 64,
        "decoder storage {width}x{height} differs from known active {}x{}",
        geometry.width,
        geometry.height
    );
    let aperture = |key: &GUID| -> Option<[i32; 4]> {
        let mut blob = vec![0u8; std::mem::size_of::<MFVideoArea>()];
        unsafe { media.GetBlob(key, &mut blob, None) }.ok()?;
        let area = unsafe { ptr::read_unaligned(blob.as_ptr().cast::<MFVideoArea>()) };
        if area.OffsetX.fract != 0 || area.OffsetY.fract != 0 {
            return None;
        }
        Some([
            i32::from(area.OffsetX.value),
            i32::from(area.OffsetY.value),
            area.Area.cx,
            area.Area.cy,
        ])
    };
    let minimum_display_aperture = aperture(&MF_MT_MINIMUM_DISPLAY_APERTURE);
    let geometric_aperture = aperture(&MF_MT_GEOMETRIC_APERTURE);
    for area in [minimum_display_aperture, geometric_aperture]
        .into_iter()
        .flatten()
    {
        ensure!(
            area[0] == 0
                && area[1] == 0
                && area[2] >= geometry.width as i32
                && area[3] >= geometry.height as i32,
            "decoder aperture {area:?} excludes known fixture"
        );
    }
    let stride = unsafe { media.GetUINT32(&MF_MT_DEFAULT_STRIDE) }.unwrap_or(width);
    ensure!(
        stride as i32 > 0,
        "negative NV12 stride unsupported in this bounded probe"
    );
    ensure!(
        bytes.len() >= (stride * height * 3 / 2) as usize,
        "NV12 buffer too short: {} vs {}",
        bytes.len(),
        stride * height * 3 / 2
    );
    let mut patches = Vec::new();
    let fixture_patches = match fixture {
        Fixture::Patches => PATCHES.to_vec(),
        Fixture::Solid(name, rgb) => vec![(name, rgb)],
    };
    for (i, &(patch_name, rgb)) in fixture_patches.iter().enumerate() {
        let (origin_x, origin_y) = match fixture {
            Fixture::Patches => (
                (i as u32 % 4) * (geometry.width / 4) + (geometry.width / 4) / 2 - 16,
                (i as u32 / 4) * (geometry.height / 2) + (geometry.height / 2) / 2 - 16,
            ),
            Fixture::Solid(_, _) => (geometry.width / 2 - 16, geometry.height / 2 - 16),
        };
        let mut sum = [0u64; 3];
        for y in origin_y..origin_y + 32 {
            for x in origin_x..origin_x + 32 {
                let luma = (y * stride + x) as usize;
                let chroma = (stride * height + (y / 2) * stride + (x / 2) * 2) as usize;
                sum[0] += u64::from(bytes[luma]);
                sum[1] += u64::from(bytes[chroma]);
                sum[2] += u64::from(bytes[chroma + 1]);
            }
        }
        patches.push(PatchMeasurement {
            name: patch_name,
            rgb,
            ycbcr: sum.map(|v| v as f64 / 1024.0),
        });
    }
    let mut candidate_errors = Vec::new();
    for (matrix, kr, kb) in [("bt601", 0.299, 0.114), ("bt709", 0.2126, 0.0722)] {
        for limited in [true, false] {
            let error = patches
                .iter()
                .map(|p| {
                    let e = expected(p.rgb, kr, kb, limited);
                    (0..3).map(|i| (e[i] - p.ycbcr[i]).powi(2)).sum::<f64>()
                })
                .sum::<f64>()
                / (patches.len() * 3) as f64;
            candidate_errors.push((
                format!("{matrix}-{}", if limited { "studio" } else { "full" }),
                error.sqrt(),
            ));
        }
    }
    candidate_errors.sort_by(|a, b| a.1.total_cmp(&b.1));
    let unique_matrix = candidate_errors[1].1 > candidate_errors[0].1 + 3.0;
    ensure!(
        candidate_errors[0].1 < 3.0 && (unique_matrix || matches!(fixture, Fixture::Solid(_, _))),
        "colour measurement does not identify a unique matrix/range: {candidate_errors:?}"
    );
    Ok(Decoded {
        decoder_clsid: format!("{clsid:?}"),
        width,
        height,
        active_width: geometry.width,
        active_height: geometry.height,
        minimum_display_aperture,
        geometric_aperture,
        stride,
        buffer_length: bytes.len(),
        mf_yuv_matrix: unsafe { media.GetUINT32(&MF_MT_YUV_MATRIX) }.ok(),
        mf_nominal_range: unsafe { media.GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE) }.ok(),
        patches,
        inferred_matrix_range: if unique_matrix {
            candidate_errors[0].0.clone()
        } else {
            "neutral solid: matrix not identifiable independently".to_owned()
        },
        candidate_errors,
        bytes,
    })
}

fn decode(codec: VideoCodec, frames: &[Vec<u8>], geometry: Geometry) -> Result<Decoded> {
    decode_fixture(codec, frames, geometry, Fixture::Patches)
}

fn decode_fixture(
    codec: VideoCodec,
    frames: &[Vec<u8>],
    geometry: Geometry,
    fixture: Fixture,
) -> Result<Decoded> {
    let (transform, clsid) = enumerate_decoder(codec)?;
    unsafe {
        let input = MFCreateMediaType()?;
        input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        input.SetGUID(&MF_MT_SUBTYPE, &subtype(codec))?;
        input.SetUINT64(
            &MF_MT_FRAME_SIZE,
            (u64::from(geometry.width) << 32) | u64::from(geometry.height),
        )?;
        input.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(FPS) << 32) | 1)?;
        input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
        input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        if codec == VideoCodec::Hevc {
            input.SetUINT32(&MF_MT_MPEG2_PROFILE, 1)?;
        }
        transform
            .SetInputType(0, &input, 0)
            .context("set decoder compressed input")?;
        choose_nv12(&transform)?;
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        for (i, bytes) in frames.iter().enumerate() {
            let sample = sample_with_bytes(bytes, i as i64 * 10_000_000 / FPS as i64)?;
            transform
                .ProcessInput(0, &sample, 0)
                .context("MF decoder ProcessInput")?;
            if let Some((bytes, media)) = pull_output(&transform, geometry)? {
                return measure(bytes, &media, clsid, geometry, fixture);
            }
        }
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
        transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
        if let Some((bytes, media)) = pull_output(&transform, geometry)? {
            return measure(bytes, &media, clsid, geometry, fixture);
        }
    }
    bail!("MF decoder produced no NV12 frame after six samples and drain")
}

#[derive(Serialize)]
struct Report {
    codec: &'static str,
    configuration: &'static str,
    width: u32,
    height: u32,
    max_width: Option<u32>,
    max_height: Option<u32>,
    frames: usize,
    frame_bytes: Vec<usize>,
    vui: Vui,
    decoded: Option<Decoded>,
    decode_error: Option<String>,
}

fn run_codec(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
    codec: VideoCodec,
    output: &Path,
    geometry: Geometry,
    max_geometry: Geometry,
) -> Result<bool> {
    run_codec_fixture(
        device,
        texture,
        codec,
        output,
        geometry,
        max_geometry,
        Fixture::Patches,
    )
}

fn run_codec_fixture(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
    codec: VideoCodec,
    output: &Path,
    geometry: Geometry,
    max_geometry: Geometry,
    fixture: Fixture,
) -> Result<bool> {
    let mut encoder = Nvenc::new_with_options_native_codec_gop(
        device.as_raw(),
        geometry.width,
        geometry.height,
        max_geometry.width,
        max_geometry.height,
        FPS,
        2_000_000,
        EncoderOptions::production(Preset::P3),
        codec,
        true,
    )?;
    let mut frames = Vec::new();
    for _ in 0..FRAMES {
        frames.push(encoder.encode_texture(texture.as_raw())?.data);
    }
    drop(encoder);
    report_fixture_frames(
        codec,
        &frames,
        output,
        geometry,
        max_geometry,
        "Fresh production(P3) Nvenc with infinite GOP; unchanged encoder, 30fps CBR2000000",
        fixture,
    )
}

fn report_frames(
    codec: VideoCodec,
    frames: &[Vec<u8>],
    output: &Path,
    geometry: Geometry,
    max_geometry: Geometry,
    configuration: &'static str,
) -> Result<bool> {
    report_fixture_frames(
        codec,
        frames,
        output,
        geometry,
        max_geometry,
        configuration,
        Fixture::Patches,
    )
}

fn report_fixture_frames(
    codec: VideoCodec,
    frames: &[Vec<u8>],
    output: &Path,
    geometry: Geometry,
    max_geometry: Geometry,
    configuration: &'static str,
    fixture: Fixture,
) -> Result<bool> {
    let stream: Vec<u8> = frames.iter().flatten().copied().collect();
    std::fs::write(
        output.join(format!(
            "nvenc-{}.{}",
            if matches!(fixture, Fixture::Patches) {
                "patches"
            } else {
                "solid"
            },
            name(codec)
        )),
        &stream,
    )?;
    let vui = parse_vui(&stream, codec).context("parse SPS VUI")?;
    let (decoded, decode_error) = match decode_fixture(codec, frames, geometry, fixture) {
        Ok(d) => {
            std::fs::write(
                output.join(format!("nvenc-{}-decoded.nv12", name(codec))),
                &d.bytes,
            )?;
            (Some(d), None)
        }
        Err(e) => (None, Some(format!("{e:#}"))),
    };
    let ok = decoded.is_some();
    let report = Report {
        codec: name(codec),
        configuration,
        width: geometry.width,
        height: geometry.height,
        max_width: Some(max_geometry.width),
        max_height: Some(max_geometry.height),
        frames: frames.len(),
        frame_bytes: frames.iter().map(Vec::len).collect(),
        vui,
        decoded,
        decode_error,
    };
    let json = serde_json::to_string_pretty(&report)?;
    std::fs::write(
        output.join(format!("nvenc-{}-colour.json", name(codec))),
        format!("{json}\n"),
    )?;
    println!("{json}");
    Ok(ok)
}

fn run_retained(device: &ID3D11Device, codec: VideoCodec, output: &Path) -> Result<()> {
    let max_geometry = Geometry {
        width: 1920,
        height: 1080,
    };
    let mut encoder = Nvenc::new_with_options_native_codec_gop(
        device.as_raw(),
        1920,
        1080,
        1920,
        1080,
        FPS,
        2_000_000,
        EncoderOptions::production(Preset::P3),
        codec,
        true,
    )?;
    for (stage, (width, height)) in [
        (1920, 1080),
        (960, 540),
        (1280, 720),
        (1920, 1080),
        (640, 576),
        (1920, 1080),
    ]
    .into_iter()
    .enumerate()
    {
        let geometry = Geometry { width, height };
        let texture = create_texture(device, geometry)?;
        if stage > 0 {
            encoder.reconfigure_profile(width, height, FPS)?;
            encoder.force_idr();
        }
        let mut frames = Vec::new();
        for frame_index in 0..FRAMES {
            let frame = encoder.encode_texture(texture.as_raw())?;
            ensure!(
                frame_index != 0 || frame.is_idr,
                "retained transition did not start with IDR"
            );
            frames.push(frame.data);
        }
        let stage_output = output.join(format!(
            "retained-{}/stage-{stage}-{width}x{height}",
            name(codec)
        ));
        std::fs::create_dir_all(&stage_output)?;
        let ok = report_frames(codec, &frames, &stage_output, geometry, max_geometry,
            "Retained production(P3) Nvenc initialized1920x1080 max1920x1080, infinite GOP; reconfigure_profile+forced IDR")?;
        ensure!(ok, "retained {} stage{stage} decode failed", name(codec));
    }
    Ok(())
}

fn main() -> Result<()> {
    let mut args: Vec<_> = std::env::args_os().skip(1).collect();
    let solid_colors = if let Some(index) = args.iter().position(|arg| arg == "--solid-colors") {
        args.remove(index);
        true
    } else {
        false
    };
    let mut output = None;
    let mut decode_file = None;
    let mut decode_geometry = None;
    ensure!(args.len() % 2 == 0, "arguments require values");
    for pair in args.chunks_exact(2) {
        match pair[0].to_str().context("argument name is not Unicode")? {
            "--output" => output = Some(PathBuf::from(&pair[1])),
            "--decode-h264-file" => decode_file = Some(PathBuf::from(&pair[1])),
            "--size" => {
                let size = pair[1].to_str().context("size is not Unicode")?;
                let (width, height) = size.split_once('x').context("size must be <width>x<height>")?;
                let geometry = Geometry { width: width.parse()?, height: height.parse()? };
                ensure!(geometry.width >= 256 && geometry.width <= 4096 && geometry.width % 2 == 0
                    && geometry.height >= 128 && geometry.height <= 2304 && geometry.height % 2 == 0,
                    "unsupported synthetic patch geometry");
                decode_geometry = Some(geometry);
            }
            _ => bail!("usage: probe_encoder_color --output <directory> [--solid-colors | --decode-h264-file <first-au.h264> --size <width>x<height>]"),
        }
    }
    let output = output.context("--output <directory> is required")?;
    ensure!(
        !solid_colors || decode_file.is_none(),
        "solid encode and external decode modes are exclusive"
    );
    ensure!(
        decode_file.is_some() == decode_geometry.is_some(),
        "--decode-h264-file and --size must be supplied together"
    );
    std::fs::create_dir_all(&output)?;
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .context("CoInitializeEx")?;
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.context("MFStartup")?;
    let result = (|| {
        if let (Some(path), Some(geometry)) = (decode_file, decode_geometry) {
            let data = std::fs::read(&path)
                .with_context(|| format!("read synthetic AU {}", path.display()))?;
            let vui = parse_vui(&data, VideoCodec::H264).context("parse external H264 SPS VUI")?;
            let frame_bytes = vec![data.len()];
            let decoded = decode(VideoCodec::H264, &[data], geometry)
                .context("decode external synthetic H264 AU")?;
            std::fs::write(output.join("decoded-file.nv12"), &decoded.bytes)?;
            let report = Report {
                codec: "h264",
                configuration:
                    "External synthetic first AU; decoder only, no encoder or D3D device created",
                width: geometry.width,
                height: geometry.height,
                max_width: None,
                max_height: None,
                frames: 1,
                frame_bytes,
                vui,
                decoded: Some(decoded),
                decode_error: None,
            };
            let json = serde_json::to_string_pretty(&report)?;
            std::fs::write(output.join("decoded-file-colour.json"), format!("{json}\n"))?;
            println!("{json}");
            return Ok(());
        }
        let device = create_device()?;
        if solid_colors {
            for (width, height) in [(512, 256), (1280, 720)] {
                let geometry = Geometry { width, height };
                for &(color_name, rgb) in &PATCHES[..5] {
                    let fixture = Fixture::Solid(color_name, rgb);
                    let texture = create_fixture_texture(&device, geometry, fixture)?;
                    let case_output = output.join(format!("solid-{width}x{height}/{color_name}"));
                    std::fs::create_dir_all(&case_output)?;
                    ensure!(
                        run_codec_fixture(
                            &device,
                            &texture,
                            VideoCodec::H264,
                            &case_output,
                            geometry,
                            Geometry {
                                width: 1920,
                                height: 1080
                            },
                            fixture
                        )?,
                        "whole-frame solid measurement failed"
                    );
                }
            }
            return Ok(());
        }
        for (width, height) in [
            (512, 256),
            (1280, 720),
            (848, 480),
            (960, 540),
            (1920, 1080),
            (640, 576),
            (640, 578),
            (720, 576),
            (722, 576),
            (766, 576),
            (768, 576),
            (770, 576),
            (784, 576),
            (798, 576),
            (800, 576),
            (802, 576),
            (832, 576),
            (1278, 540),
            (1280, 540),
            (640, 720),
            (1024, 600),
            (960, 600),
        ] {
            let geometry = Geometry { width, height };
            let case_output = output.join(format!("{}x{}", geometry.width, geometry.height));
            std::fs::create_dir_all(&case_output)?;
            let texture = create_texture(&device, geometry)?;
            let h264_ok = run_codec(
                &device,
                &texture,
                VideoCodec::H264,
                &case_output,
                geometry,
                geometry,
            )
            .context("H264 synthetic measurement")?;
            if [(512, 256), (1280, 720), (1920, 1080)].contains(&(width, height)) {
                if let Err(e) = run_codec(
                    &device,
                    &texture,
                    VideoCodec::Hevc,
                    &case_output,
                    geometry,
                    geometry,
                ) {
                    println!("HEVC measurement unavailable: {e:#}");
                }
            }
            ensure!(
                h264_ok,
                "H264 decoding failed; see colour JSON and saved stream"
            );
        }
        for (width, height) in [(848, 480), (960, 540), (640, 576)] {
            let geometry = Geometry { width, height };
            let texture = create_texture(&device, geometry)?;
            let case_output = output.join(format!("native-max-1920x1080/{width}x{height}"));
            std::fs::create_dir_all(&case_output)?;
            ensure!(
                run_codec(
                    &device,
                    &texture,
                    VideoCodec::H264,
                    &case_output,
                    geometry,
                    Geometry {
                        width: 1920,
                        height: 1080
                    }
                )?,
                "nativeMax SD case failed"
            );
        }
        run_retained(&device, VideoCodec::H264, &output)?;
        if let Err(e) = run_retained(&device, VideoCodec::Hevc, &output) {
            println!("retained HEVC unavailable: {e:#}");
        }
        Ok(())
    })();
    unsafe {
        let _ = MFShutdown();
        CoUninitialize();
    }
    result
}
