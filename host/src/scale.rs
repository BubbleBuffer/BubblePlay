//! D3D11 video-processor scaling on the capture device.
//!
//! Low and Balanced own persistent BGRA render-target textures. Each capture
//! texture receives a short-lived input view because DDA may rotate the
//! underlying texture between acquisitions. Full is a true zero-copy bypass.

use std::ffi::c_void;
use std::fmt;
use std::mem::ManuallyDrop;
use std::time::Instant;

use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET,
    D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};

use crate::quality::Profile;

#[derive(Debug)]
pub enum ScaleError {
    Windows {
        operation: &'static str,
        source: windows::core::Error,
    },
    MissingOutput(&'static str),
    WrongDevice,
    UnsupportedBgra {
        support: u32,
    },
}

impl fmt::Display for ScaleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Windows { operation, source } => write!(f, "{operation}: {source}"),
            Self::MissingOutput(operation) => write!(f, "{operation} returned no object"),
            Self::WrongDevice => {
                write!(f, "input texture is not on the capture/scaler D3D11 device")
            }
            Self::UnsupportedBgra { support } => {
                write!(
                    f,
                    "video processor does not support BGRA input+output (flags {support:#x})"
                )
            }
        }
    }
}

impl std::error::Error for ScaleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Windows { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn win(operation: &'static str, source: windows::core::Error) -> ScaleError {
    ScaleError::Windows { operation, source }
}

pub struct ScaleOutput {
    pub texture: *mut c_void,
    pub scale_us: u64,
    pub width: u32,
    pub height: u32,
}

struct Target {
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    texture: ID3D11Texture2D,
    output_view: ID3D11VideoProcessorOutputView,
}

pub struct Scaler {
    device: ID3D11Device,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    low: Target,
    balanced: Target,
    full: Target,
}

impl Scaler {
    pub fn is_for_device(&self, device: &ID3D11Device) -> bool {
        self.device.as_raw() == device.as_raw()
    }

    pub fn new(
        device: &ID3D11Device,
        input_width: u32,
        input_height: u32,
    ) -> Result<Self, ScaleError> {
        let video_device: ID3D11VideoDevice =
            device.cast().map_err(|e| win("ID3D11VideoDevice", e))?;
        let immediate =
            unsafe { device.GetImmediateContext() }.map_err(|e| win("GetImmediateContext", e))?;
        let video_context: ID3D11VideoContext =
            immediate.cast().map_err(|e| win("ID3D11VideoContext", e))?;
        let low = Self::create_target(
            device,
            &video_device,
            &video_context,
            input_width,
            input_height,
            960,
            540,
            30,
        )?;
        let balanced = Self::create_target(
            device,
            &video_device,
            &video_context,
            input_width,
            input_height,
            1280,
            720,
            30,
        )?;
        let full = Self::create_target(
            device,
            &video_device,
            &video_context,
            input_width,
            input_height,
            1920,
            1080,
            60,
        )?;
        Ok(Self {
            device: device.clone(),
            video_device,
            video_context,
            low,
            balanced,
            full,
        })
    }

    fn create_target(
        device: &ID3D11Device,
        video_device: &ID3D11VideoDevice,
        video_context: &ID3D11VideoContext,
        input_width: u32,
        input_height: u32,
        output_width: u32,
        output_height: u32,
        output_fps: u32,
    ) -> Result<Target, ScaleError> {
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            InputWidth: input_width,
            InputHeight: input_height,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: output_fps,
                Denominator: 1,
            },
            OutputWidth: output_width,
            OutputHeight: output_height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&content) }
            .map_err(|e| win("CreateVideoProcessorEnumerator", e))?;
        let support = unsafe { enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM) }
            .map_err(|e| win("CheckVideoProcessorFormat(BGRA)", e))?;
        let needed = D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32
            | D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32;
        if support & needed != needed {
            return Err(ScaleError::UnsupportedBgra { support });
        }
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
            .map_err(|e| win("CreateVideoProcessor", e))?;

        let texture_desc = D3D11_TEXTURE2D_DESC {
            Width: output_width,
            Height: output_height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        unsafe { device.CreateTexture2D(&texture_desc, None, Some(&mut texture)) }
            .map_err(|e| win("CreateTexture2D(scale output)", e))?;
        let texture = texture.ok_or(ScaleError::MissingOutput("CreateTexture2D"))?;
        let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut output_view = None;
        unsafe {
            video_device.CreateVideoProcessorOutputView(
                &texture,
                &enumerator,
                &output_desc,
                Some(&mut output_view),
            )
        }
        .map_err(|e| win("CreateVideoProcessorOutputView", e))?;
        let output_view =
            output_view.ok_or(ScaleError::MissingOutput("CreateVideoProcessorOutputView"))?;

        let src = RECT {
            left: 0,
            top: 0,
            right: input_width as i32,
            bottom: input_height as i32,
        };
        let dst = RECT {
            left: 0,
            top: 0,
            right: output_width as i32,
            bottom: output_height as i32,
        };
        unsafe {
            video_context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&dst));
            video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&src));
            video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dst));
        }
        Ok(Target {
            enumerator,
            processor,
            texture,
            output_view,
        })
    }

    /// Submit a GPU scale operation and return a pointer valid until the next
    /// scale into the same target. The caller encodes it synchronously.
    pub fn scale(
        &mut self,
        input: &ID3D11Texture2D,
        profile: Profile,
    ) -> Result<ScaleOutput, ScaleError> {
        let started = Instant::now();
        let input_device = unsafe { input.GetDevice() }.map_err(|e| win("input GetDevice", e))?;
        if input_device.as_raw() != self.device.as_raw() {
            return Err(ScaleError::WrongDevice);
        }
        let target = match profile {
            Profile::Low => &mut self.low,
            Profile::Balanced => &mut self.balanced,
            Profile::Full => &mut self.full,
        };
        let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let mut input_view = None;
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                input,
                &target.enumerator,
                &input_desc,
                Some(&mut input_view),
            )
        }
        .map_err(|e| win("CreateVideoProcessorInputView", e))?;
        let input_view =
            input_view.ok_or(ScaleError::MissingOutput("CreateVideoProcessorInputView"))?;
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM::default();
        stream.Enable = true.into();
        stream.pInputSurface = ManuallyDrop::new(Some(input_view));
        let blt = unsafe {
            self.video_context.VideoProcessorBlt(
                &target.processor,
                &target.output_view,
                0,
                std::slice::from_ref(&stream),
            )
        };
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        blt.map_err(|e| win("VideoProcessorBlt", e))?;
        Ok(ScaleOutput {
            texture: target.texture.as_raw() as *mut c_void,
            scale_us: started.elapsed().as_micros().min(u64::MAX as u128) as u64,
            width: match profile {
                Profile::Low => 960,
                Profile::Balanced => 1280,
                Profile::Full => 1920,
            },
            height: match profile {
                Profile::Low => 540,
                Profile::Balanced => 720,
                Profile::Full => 1080,
            },
        })
    }
}
