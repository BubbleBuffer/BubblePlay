//! NVENC H.264 encoder on D3D11 textures via the raw `nvidia-video-codec-sdk`
//! sys bindings (`ENCODE_API` function-pointer table).
//!
//! Zero-copy path: register the captured desktop texture (same D3D11 device)
//! -> map -> encode_picture -> lock_bitstream -> memcpy Annex B out.
//! The input never leaves VRAM; only the bitstream is copied to host memory.
//!
//! SDK version audit (2026-08-07, driver 591.86 = SDK 13.0):
//! The `nvidia-video-codec-sdk` crate ships SDK 12.1 headers, and that is
//! fine: a C probe compiled against the genuine 12.1 header runs the full
//! session->preset->init->register->map->encode->lock cycle successfully on
//! this driver (30 frames, H.264 High 1080p, ffprobe-verified). The driver
//! accepts the old version words. Two call-site details matter:
//!   1. `NV_ENC_PRESET_CONFIG::presetCfg.version` must be set to
//!      `NV_ENC_CONFIG_VER` before `GetEncodePresetConfigEx` (else 15).
//!   2. `NV_ENC_MAP_INPUT_RESOURCE::inputResource` is *deprecated* ("Do not
//!      use" in the SDK header); the registered handle goes in
//!      `registeredResource` (else generic error 20 at Map).
//!
//! Config (locked decisions): H.264 High, CBR 20 Mbps start, preset P1 +
//! ultra-low-latency tune, GOP/IDR = 2 * fps, repeatSPSPPS on.

use std::ffi::c_void;
use std::ptr;

use anyhow::{bail, ensure, Context, Result};
use nvidia_video_codec_sdk::safe::ENCODE_API;
use nvidia_video_codec_sdk::sys::nvEncodeAPI::{
    GUID, NVENCAPI_VERSION, NV_ENC_BUFFER_FORMAT, NV_ENC_CONFIG, NV_ENC_CONFIG_VER,
    NV_ENC_CREATE_BITSTREAM_BUFFER, NV_ENC_CREATE_BITSTREAM_BUFFER_VER, NV_ENC_DEVICE_TYPE,
    NV_ENC_INITIALIZE_PARAMS, NV_ENC_INITIALIZE_PARAMS_VER, NV_ENC_INPUT_RESOURCE_TYPE,
    NV_ENC_LOCK_BITSTREAM, NV_ENC_LOCK_BITSTREAM_VER, NV_ENC_MAP_INPUT_RESOURCE,
    NV_ENC_MAP_INPUT_RESOURCE_VER, NV_ENC_MULTI_PASS, NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS,
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER, NV_ENC_PARAMS_RC_MODE, NV_ENC_PIC_FLAGS,
    NV_ENC_PIC_PARAMS, NV_ENC_PIC_PARAMS_VER, NV_ENC_PIC_STRUCT, NV_ENC_PIC_TYPE,
    NV_ENC_PRESET_CONFIG, NV_ENC_PRESET_CONFIG_VER, NV_ENC_RECONFIGURE_PARAMS,
    NV_ENC_RECONFIGURE_PARAMS_VER, NV_ENC_REGISTER_RESOURCE, NV_ENC_REGISTER_RESOURCE_VER,
    NV_ENC_SEQUENCE_PARAM_PAYLOAD, NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER, NV_ENC_TUNING_INFO,
};
use tracing::{debug, info, warn};

// The crate keeps its GUID table in a private `sys::guid` module; the values
// below are copied verbatim from there (they mirror the C SDK headers).
const NV_ENC_CODEC_H264_GUID: GUID = GUID {
    Data1: 0x6bc8_2762,
    Data2: 0x4e63,
    Data3: 0x4ca4,
    Data4: [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
};
const NV_ENC_PRESET_P1_GUID: GUID = GUID {
    Data1: 0xfc0a_8d3e,
    Data2: 0x45f8,
    Data3: 0x4cf8,
    Data4: [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf],
};
const NV_ENC_PRESET_P3_GUID: GUID = GUID {
    Data1: 0x3685_0110,
    Data2: 0x3a07,
    Data3: 0x441f,
    Data4: [0x94, 0xd5, 0x36, 0x70, 0x63, 0x1f, 0x91, 0xf6],
};
const NV_ENC_H264_PROFILE_HIGH_GUID: GUID = GUID {
    Data1: 0xe7cb_c309,
    Data2: 0x4f7a,
    Data3: 0x4b89,
    Data4: [0xaf, 0x2a, 0xd5, 0x37, 0xc9, 0x2b, 0xe3, 0x10],
};

/// Zero-initialize a repr(C) NVENC params struct. This mirrors the C SDK usage
/// pattern (`memset(&p, 0, sizeof(p)); p.version = ...;`): every field we do
/// not set explicitly must be zero for the API.
#[inline]
unsafe fn zeroed<T>() -> T {
    std::mem::MaybeUninit::zeroed().assume_init()
}

pub struct EncodedFrame {
    /// Annex B access unit (SPS/PPS included on IDR when repeatSPSPPS is on).
    pub data: Vec<u8>,
    pub is_idr: bool,
    /// Encoder-side latency: register+map+encode+lock combined, in microseconds.
    pub encode_us: u64,
}
#[derive(Debug, Clone, Copy)]
pub enum Preset {
    P1,
    P3,
}
impl Preset {
    fn guid(self) -> GUID {
        match self {
            Self::P1 => NV_ENC_PRESET_P1_GUID,
            Self::P3 => NV_ENC_PRESET_P3_GUID,
        }
    }
}

pub struct Nvenc {
    session: *mut c_void,
    device: *mut c_void,
    bitstream: *mut c_void,
    width: u32,
    height: u32,
    fps: u32,
    max_width: u32,
    max_height: u32,
    bitrate_bps: u32,
    frame_idx: u64,
    force_idr_next: bool,
    last_mapped: Option<*mut c_void>,
    /// Config snapshot kept alive for reconfigure; `init_params.encodeConfig`
    /// points at `cfg`. Both are moved together on reconfigure.
    cfg: Box<NV_ENC_CONFIG>,
    init_params: Box<NV_ENC_INITIALIZE_PARAMS>,
}

// The session is bound to one thread; we never share it across threads.
unsafe impl Send for Nvenc {}

impl Nvenc {
    /// Open an encoder session on the given D3D11 device. For zero-copy
    /// register/map this must be the same device the capture texture lives on.
    pub fn new(
        device: *mut c_void,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
    ) -> Result<Self> {
        Self::new_with_preset_native(
            device,
            width,
            height,
            width,
            height,
            fps,
            bitrate_bps,
            Preset::P3,
        )
    }
    pub fn new_with_preset(
        device: *mut c_void,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
        preset: Preset,
    ) -> Result<Self> {
        Self::new_with_preset_native(
            device,
            width,
            height,
            width,
            height,
            fps,
            bitrate_bps,
            preset,
        )
    }
    pub fn new_with_preset_native(
        device: *mut c_void,
        width: u32,
        height: u32,
        max_width: u32,
        max_height: u32,
        fps: u32,
        bitrate_bps: u32,
        preset: Preset,
    ) -> Result<Self> {
        ensure!(!device.is_null(), "null D3D11 device");
        ensure!(
            width <= max_width && height <= max_height,
            "active dimensions exceed native maximum"
        );

        // 1. Open session on the D3D11 device.
        let mut session_params: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = unsafe { zeroed() };
        session_params.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
        session_params.deviceType = NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX;
        session_params.device = device;
        session_params.apiVersion = NVENCAPI_VERSION;
        let mut session: *mut c_void = ptr::null_mut();
        let st = unsafe { (ENCODE_API.open_encode_session_ex)(&mut session_params, &mut session) };
        st.result_without_string()
            .context("NvEncOpenEncodeSessionEx")?;
        info!("NVENC session opened on capture D3D11 device");

        // 2. Query preset config: P1 + ultra-low-latency tune.
        let mut preset_cfg: NV_ENC_PRESET_CONFIG = unsafe { zeroed() };
        preset_cfg.version = NV_ENC_PRESET_CONFIG_VER;
        // The driver validates the *inner* config version too; leaving it
        // zero yields NV_ENC_ERR_INVALID_VERSION (15) on SDK 13.0 drivers.
        preset_cfg.presetCfg.version = NV_ENC_CONFIG_VER;
        let st = unsafe {
            (ENCODE_API.get_encode_preset_config_ex)(
                session,
                NV_ENC_CODEC_H264_GUID,
                preset.guid(),
                NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                &mut preset_cfg,
            )
        };
        st.result_without_string()
            .context("GetEncodePresetConfigEx")?;

        let mut cfg = Box::new(preset_cfg.presetCfg);
        cfg.profileGUID = NV_ENC_H264_PROFILE_HIGH_GUID;
        cfg.gopLength = fps * 2; // IDR every 2 s
        cfg.frameIntervalP = 1; // P-only (ULL preset default)

        // Rate control: CBR at the starting bitrate.
        cfg.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
        cfg.rcParams.averageBitRate = bitrate_bps;
        cfg.rcParams.maxBitRate = bitrate_bps;
        cfg.rcParams.vbvBufferSize = bitrate_bps / fps.max(1);
        cfg.rcParams.vbvInitialDelay = cfg.rcParams.vbvBufferSize;
        cfg.rcParams.set_enableAQ(1);
        cfg.rcParams.set_aqStrength(8);
        cfg.rcParams.set_enableTemporalAQ(0);
        cfg.rcParams.set_enableLookahead(0);
        cfg.rcParams.lookaheadDepth = 0;
        cfg.rcParams.multiPass = NV_ENC_MULTI_PASS::NV_ENC_MULTI_PASS_DISABLED;

        // H.264 specifics: IDR period + repeat SPS/PPS on every IDR.
        unsafe {
            let h264 = &mut cfg.encodeCodecConfig.h264Config;
            h264.idrPeriod = fps * 2;
            h264.set_repeatSPSPPS(1);
        }

        // 3. Initialize the encoder.
        let mut init = Box::<NV_ENC_INITIALIZE_PARAMS>::new(unsafe { zeroed() });
        init.version = NV_ENC_INITIALIZE_PARAMS_VER;
        init.encodeGUID = NV_ENC_CODEC_H264_GUID;
        init.presetGUID = preset.guid();
        init.encodeWidth = width;
        init.encodeHeight = height;
        init.darWidth = width;
        init.darHeight = height;
        init.frameRateNum = fps;
        init.frameRateDen = 1;
        init.maxEncodeWidth = max_width;
        init.maxEncodeHeight = max_height;
        init.enableEncodeAsync = 0; // sync D3D11 path
        init.enablePTD = 1;
        init.encodeConfig = cfg.as_mut();
        init.tuningInfo = NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
        init.bufferFormat = NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB;
        let st = unsafe { (ENCODE_API.initialize_encoder)(session, init.as_mut()) };
        st.result_without_string()
            .context("NvEncInitializeEncoder")?;
        info!(
            width, height, max_width, max_height, fps, bitrate_bps, preset = ?preset,
            "NVENC initialized (H264 High, CBR, ULL)"
        );

        // 4. Bitstream output buffer.
        let mut bs_params: NV_ENC_CREATE_BITSTREAM_BUFFER = unsafe { zeroed() };
        bs_params.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
        let st = unsafe { (ENCODE_API.create_bitstream_buffer)(session, &mut bs_params) };
        st.result_without_string()
            .context("NvEncCreateBitstreamBuffer")?;

        Ok(Self {
            session,
            device,
            bitstream: bs_params.bitstreamBuffer,
            width,
            height,
            fps,
            max_width,
            max_height,
            bitrate_bps,
            frame_idx: 0,
            force_idr_next: true, // first frame must be IDR for the decoder
            last_mapped: None,
            cfg,
            init_params: init,
        })
    }

    /// Force the next encoded picture to be an IDR (e.g. on PLI).
    pub fn force_idr(&mut self) {
        self.force_idr_next = true;
    }

    pub fn is_for_device(&self, device: *mut c_void) -> bool {
        self.device == device
    }

    /// Reconfigure the encoder bitrate (BWE clamp). NVENC applies it from the
    /// next picture onward.
    pub fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<()> {
        if bitrate_bps == self.bitrate_bps {
            return Ok(());
        }
        let mut cfg = *self.cfg;
        cfg.rcParams.averageBitRate = bitrate_bps;
        cfg.rcParams.maxBitRate = bitrate_bps;
        cfg.rcParams.vbvBufferSize = bitrate_bps / self.fps.max(1);
        cfg.rcParams.vbvInitialDelay = cfg.rcParams.vbvBufferSize;
        let mut init = *self.init_params;
        init.encodeConfig = &mut cfg;
        let mut reconf: NV_ENC_RECONFIGURE_PARAMS = unsafe { zeroed() };
        reconf.version = NV_ENC_RECONFIGURE_PARAMS_VER;
        reconf.reInitEncodeParams = init;
        let st = unsafe { (ENCODE_API.reconfigure_encoder)(self.session, &mut reconf) };
        match st.result_without_string() {
            Ok(()) => {
                *self.cfg = cfg;
                *self.init_params = init;
                self.init_params.encodeConfig = self.cfg.as_mut();
                self.bitrate_bps = bitrate_bps;
                info!(bitrate_bps, "NVENC bitrate reconfigured");
                Ok(())
            }
            Err(e) => {
                warn!(bitrate_bps, ?e, "NVENC reconfigure failed");
                Err(e.into())
            }
        }
    }

    /// Dimension/fps transition. The host requests its wall-clock IDR on the
    /// first frame after this succeeds; GOP cadence itself is not reconfigured.
    pub fn reconfigure_profile(&mut self, width: u32, height: u32, fps: u32) -> Result<()> {
        if width > self.max_width || height > self.max_height {
            bail!("profile exceeds native encoder dimensions");
        }
        if width == self.width && height == self.height && fps == self.fps {
            return Ok(());
        }
        let mut cfg = *self.cfg;
        cfg.rcParams.vbvBufferSize = self.bitrate_bps / fps.max(1);
        cfg.rcParams.vbvInitialDelay = cfg.rcParams.vbvBufferSize;
        let mut init = *self.init_params;
        init.encodeWidth = width;
        init.encodeHeight = height;
        init.darWidth = width;
        init.darHeight = height;
        init.frameRateNum = fps;
        init.encodeConfig = &mut cfg;
        let mut reconf: NV_ENC_RECONFIGURE_PARAMS = unsafe { zeroed() };
        reconf.version = NV_ENC_RECONFIGURE_PARAMS_VER;
        reconf.reInitEncodeParams = init;
        unsafe {
            reconf.set_resetEncoder(1);
        }
        unsafe { (ENCODE_API.reconfigure_encoder)(self.session, &mut reconf) }
            .result_without_string()
            .context("NvEncReconfigureEncoder profile")?;
        *self.cfg = cfg;
        *self.init_params = init;
        self.init_params.encodeConfig = self.cfg.as_mut();
        self.width = width;
        self.height = height;
        self.fps = fps;
        self.force_idr_next = true;
        Ok(())
    }

    /// Encode one captured desktop texture (zero-copy).
    ///
    /// `texture` must be an `ID3D11Texture2D` from the same D3D11 device the
    /// session was opened on. Returns the Annex B access unit.
    pub fn encode_texture(&mut self, texture: *mut c_void) -> Result<EncodedFrame> {
        let t0 = std::time::Instant::now();

        // Register -> map the input texture.
        let mut reg: NV_ENC_REGISTER_RESOURCE = unsafe { zeroed() };
        reg.version = NV_ENC_REGISTER_RESOURCE_VER;
        reg.resourceType = NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
        reg.width = self.width;
        reg.height = self.height;
        reg.pitch = 0; // must be 0 for D3D resources
        reg.resourceToRegister = texture;
        reg.bufferFormat = NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB;
        // bufferUsage zero == NV_ENC_INPUT_IMAGE.
        let st = unsafe { (ENCODE_API.register_resource)(self.session, &mut reg) };
        st.result_without_string()
            .context("NvEncRegisterResource")?;
        let registered = reg.registeredResource;

        let mut map: NV_ENC_MAP_INPUT_RESOURCE = unsafe { zeroed() };
        map.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
        // `inputResource` is a deprecated field ("Do not use" per the SDK
        // header); the registered handle belongs in `registeredResource`.
        map.registeredResource = registered;
        let st = unsafe { (ENCODE_API.map_input_resource)(self.session, &mut map) };
        if let Err(e) = st.result_without_string().context("NvEncMapInputResource") {
            let _ = unsafe { (ENCODE_API.unregister_resource)(self.session, registered) };
            return Err(e);
        }
        let mapped = map.mappedResource;
        self.last_mapped = Some(mapped);

        // Encode.
        let force_idr = self.force_idr_next;
        let mut pic: NV_ENC_PIC_PARAMS = unsafe { zeroed() };
        pic.version = NV_ENC_PIC_PARAMS_VER;
        pic.inputWidth = self.width;
        pic.inputHeight = self.height;
        pic.inputPitch = self.width;
        pic.encodePicFlags = if force_idr {
            NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR as u32
        } else {
            0
        };
        pic.frameIdx = self.frame_idx as u32;
        pic.inputTimeStamp = self.frame_idx;
        pic.inputBuffer = mapped;
        pic.outputBitstream = self.bitstream;
        pic.bufferFmt = NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB;
        pic.pictureStruct = NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME;
        pic.pictureType = NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_UNKNOWN;
        let st = unsafe { (ENCODE_API.encode_picture)(self.session, &mut pic) };
        let res = st.result_without_string().context("NvEncEncodePicture");
        if let Err(e) = res {
            self.unmap_current();
            let _ = unsafe { (ENCODE_API.unregister_resource)(self.session, registered) };
            return Err(e);
        }

        // Lock bitstream (sync path: with ULL tune the output is ready right
        // after encode_picture returns).
        let mut lock: NV_ENC_LOCK_BITSTREAM = unsafe { zeroed() };
        lock.version = NV_ENC_LOCK_BITSTREAM_VER;
        lock.outputBitstream = self.bitstream as *mut c_void;
        let st = unsafe { (ENCODE_API.lock_bitstream)(self.session, &mut lock) };
        let locked = st.result_without_string().context("NvEncLockBitstream");

        let mut result = None;
        if locked.is_ok() {
            let len = lock.bitstreamSizeInBytes as usize;
            let mut out = vec![0u8; len];
            unsafe {
                ptr::copy_nonoverlapping(
                    lock.bitstreamBufferPtr as *const u8,
                    out.as_mut_ptr(),
                    len,
                );
            }
            let st = unsafe { (ENCODE_API.unlock_bitstream)(self.session, self.bitstream) };
            if let Err(e) = st.result_without_string() {
                warn!(?e, "unlock_bitstream");
            }
            result = Some(out);
        } else if let Err(e) = locked {
            warn!(?e, "lock_bitstream failed");
        }

        // Unmap + unregister the input.
        self.unmap_current();
        let st = unsafe { (ENCODE_API.unregister_resource)(self.session, registered) };
        if let Err(e) = st.result_without_string() {
            debug!(?e, "unregister_resource");
        }

        let Some(data) = result else {
            bail!("no bitstream output")
        };
        let is_idr = lock.pictureType == NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR || force_idr;
        self.frame_idx += 1;
        self.force_idr_next = false;

        Ok(EncodedFrame {
            is_idr,
            encode_us: t0.elapsed().as_micros() as u64,
            data,
        })
    }

    fn unmap_current(&mut self) {
        if let Some(mapped) = self.last_mapped.take() {
            let st = unsafe { (ENCODE_API.unmap_input_resource)(self.session, mapped) };
            if let Err(e) = st.result_without_string() {
                debug!(?e, "unmap_input_resource");
            }
        }
    }

    /// Fetch SPS/PPS (Annex B) — for decoder config / sanity checks.
    pub fn sequence_params(&self) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; 512];
        let mut out_size: u32 = 0;
        let mut payload: NV_ENC_SEQUENCE_PARAM_PAYLOAD = unsafe { zeroed() };
        payload.version = NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER;
        payload.inBufferSize = buf.len() as u32;
        payload.spsppsBuffer = buf.as_mut_ptr() as *mut c_void;
        payload.outSPSPPSPayloadSize = &mut out_size;
        let st = unsafe { (ENCODE_API.get_sequence_params)(self.session, &mut payload) };
        st.result_without_string()
            .context("NvEncGetSequenceParams")?;
        buf.truncate(out_size as usize);
        Ok(buf)
    }

    pub fn bitrate(&self) -> u32 {
        self.bitrate_bps
    }

    pub fn fps(&self) -> u32 {
        self.fps
    }
}

impl Drop for Nvenc {
    fn drop(&mut self) {
        self.unmap_current();
        if !self.bitstream.is_null() {
            let _ = unsafe { (ENCODE_API.destroy_bitstream_buffer)(self.session, self.bitstream) };
            self.bitstream = ptr::null_mut();
        }
        if !self.session.is_null() {
            let _ = unsafe { (ENCODE_API.destroy_encoder)(self.session) };
            self.session = ptr::null_mut();
        }
    }
}
