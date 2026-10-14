//! Latest-frame mailbox and the `VideoLayer` that letterboxes NV12 decoder output
//! into the shell's render target with `ID3D11VideoProcessor`. No CPU copies.
//!
//! Lease rule: a decoded sample returns to the decoder pool only when its
//! `DecodedFrame` drops. A frame that was never drawn can drop at once. A drawn
//! frame stays in `in_flight` until the GPU fence signaled after its blt has
//! completed, so the decoder cannot reuse a surface the GPU is still reading.

use super::{
    backend::{Codec, DecodedFrame},
    mf::video_interface_error,
    stats::MediaStats,
};
use crate::shell::{Rect, VideoLayer};
use anyhow::{bail, Context, Result};
use std::{
    collections::VecDeque,
    mem::ManuallyDrop,
    rc::Rc,
    sync::{Arc, Mutex, MutexGuard},
    time::Instant,
};
use windows::core::{Interface, BOOL};
use windows::Win32::{
    Foundation::RECT,
    Graphics::{
        Direct3D11::*,
        Dxgi::Common::{
            DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            DXGI_RATIONAL,
        },
    },
};

/// Called on the decode thread after a frame is published. Typically wakes the UI loop.
pub type WakeFn = Arc<dyn Fn() + Send + Sync>;

/// Optional development observer. Callbacks run on decode/UI threads and must
/// return promptly without panicking. `draw_enter` and an empty `draw_return`
/// use PTS 0 because no frame has been selected; `draw_selected` identifies
/// every selection, including redraws of the current frame. The existing
/// presented callback separately marks the first successful blit submission.
pub type TraceFn = Arc<dyn Fn(&'static str, i64, Instant) + Send + Sync>;

struct Pending {
    frame: DecodedFrame,
    decoded_at: Instant,
}

struct Shared {
    mailbox: Mutex<Option<Pending>>,
    wake: Option<WakeFn>,
    trace: Mutex<Option<TraceFn>>,
    stats: MediaStats,
}

impl Shared {
    fn mailbox(&self) -> MutexGuard<'_, Option<Pending>> {
        self.mailbox.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn trace(&self, stage: &'static str, pts: i64, at: Instant) {
        let callback = { self.trace.lock().unwrap_or_else(|e| e.into_inner()).clone() };
        if let Some(callback) = callback {
            callback(stage, pts, at);
        }
    }
}

/// Producer side of the mailbox. Cheap to clone; safe to move to the decode thread.
#[derive(Clone)]
pub struct FrameSink {
    shared: Arc<Shared>,
}

impl FrameSink {
    /// Discards an undrawn frame when the session invalidates its decode chain.
    pub fn clear_pending(&self) {
        if let Some(old) = self.shared.mailbox().take() {
            self.shared.stats.note_dropped(Instant::now());
            drop(old);
        }
    }

    /// Publishes the newest decoded frame. If an undrawn frame is still waiting,
    /// it is replaced and counted as a drop; its surface returns to the decoder.
    #[cfg(test)]
    pub fn push(&self, frame: DecodedFrame) {
        self.push_timed(frame, Instant::now());
    }

    /// Publishes output using the decoder's actual completion timestamp.
    /// Mailbox overwrites are observed separately from reset/upstream drops.
    pub fn push_timed(&self, frame: DecodedFrame, decode_done: Instant) {
        let pts = frame.pts_100ns;
        self.shared.trace("decoded_done", pts, decode_done);
        self.shared
            .stats
            .set_format(frame.codec, frame.width, frame.height);
        self.shared.stats.note_decoded(pts, decode_done);
        let (replaced, published_at) = {
            let mut mailbox = self.shared.mailbox();
            let replaced = mailbox.replace(Pending {
                frame,
                decoded_at: decode_done,
            });
            (replaced, Instant::now())
        };
        self.shared.trace("mailbox_publish", pts, published_at);
        if let Some(old) = replaced {
            self.shared
                .trace("mailbox_overwrite", old.frame.pts_100ns, published_at);
            self.shared.stats.note_dropped(published_at);
            drop(old);
        }
        if let Some(wake) = &self.shared.wake {
            self.shared.trace("wake_call", pts, Instant::now());
            wake();
            self.shared.trace("wake_return", pts, Instant::now());
        }
    }
}

// Selected and in-flight frames are retained only by the UI thread.
// The cross-thread mailbox transfers an owned DecodedFrame before this point.
struct Current {
    frame: Rc<DecodedFrame>,
    decoded_at: Instant,
    /// Whether this frame has been counted as presented.
    counted: bool,
}

struct Processor {
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    input: (u32, u32),
    output: (u32, u32),
}

/// `VideoLayer` that draws the newest decoded frame into the viewport, scaled to
/// fit and letterboxed. Drawn before egui; never draws UI.
///
/// Thread: the UI thread only. Frames arrive through [`VideoRenderer::sink`].
pub struct VideoRenderer {
    // Retain the device for the renderer lifetime.
    _device: ID3D11Device,
    video_device: ID3D11VideoDevice,
    fence: ID3D11Fence,
    fence_value: u64,
    shared: Arc<Shared>,
    processor: Option<Processor>,
    current: Option<Current>,
    in_flight: VecDeque<(u64, Rc<DecodedFrame>)>,
    last_error: Option<String>,
    presented_callback: Option<Arc<dyn Fn(i64, Instant) + Send + Sync>>,
}

impl VideoRenderer {
    /// `device` must be the shared shell device: video support, BGRA, and
    /// multithread protection. `wake` runs on the decode thread after each frame.
    pub fn new(device: &ID3D11Device, stats: MediaStats, wake: Option<WakeFn>) -> Result<Self> {
        unsafe {
            let video_device: ID3D11VideoDevice = device.cast().map_err(|e| {
                video_interface_error(
                    Codec::H264,
                    e,
                    "D3D11 device has no ID3D11VideoDevice interface",
                )
            })?;
            // Fail capability checks before entering the draw loop, where VideoLayer
            // reports presentation errors through its display/statistics contract.
            let context = device.GetImmediateContext()?;
            let _: ID3D11VideoContext = context.cast().map_err(|e| {
                video_interface_error(
                    Codec::H264,
                    e,
                    "D3D11 device context has no ID3D11VideoContext interface",
                )
            })?;
            let _: ID3D11VideoContext1 = context.cast().map_err(|e| {
                video_interface_error(
                    Codec::H264,
                    e,
                    "D3D11 device context has no ID3D11VideoContext1 interface",
                )
            })?;
            let device5: ID3D11Device5 = device
                .cast()
                .context("ID3D11Device5 required for the GPU fence")?;
            let mut fence: Option<ID3D11Fence> = None;
            device5.CreateFence(0, D3D11_FENCE_FLAG_NONE, &mut fence)?;
            let fence = fence.context("CreateFence returned no fence")?;
            Ok(Self {
                _device: device.clone(),
                video_device,
                fence,
                fence_value: 0,
                shared: Arc::new(Shared {
                    mailbox: Mutex::new(None),
                    wake,
                    trace: Mutex::new(None),
                    stats,
                }),
                processor: None,
                current: None,
                in_flight: VecDeque::new(),
                last_error: None,
                presented_callback: None,
            })
        }
    }

    /// Producer handle for the decode thread.
    pub fn sink(&self) -> FrameSink {
        FrameSink {
            shared: self.shared.clone(),
        }
    }

    /// Enables or removes the optional development timing observer.
    pub fn set_trace_callback(&mut self, callback: Option<TraceFn>) {
        *self.shared.trace.lock().unwrap_or_else(|e| e.into_inner()) = callback;
    }

    /// The most recent draw error, cleared by the next successful draw.
    #[cfg(test)]
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Called once after a new decoded frame is successfully submitted to the
    /// render target. The shell owns the subsequent swap-chain Present.
    pub fn set_presented_callback(&mut self, callback: Arc<dyn Fn(i64, Instant) + Send + Sync>) {
        self.presented_callback = Some(callback);
    }

    fn retire_completed(&mut self) {
        let completed = unsafe { self.fence.GetCompletedValue() };
        while self
            .in_flight
            .front()
            .is_some_and(|(value, _)| *value <= completed)
        {
            self.in_flight.pop_front();
        }
    }

    /// Offscreen tests run decode and draw on one thread. Retire completed
    /// leases before returning to a synchronous decoder that may need them.
    #[cfg(test)]
    pub(crate) fn gpu_work_pending(&mut self) -> bool {
        self.retire_completed();
        !self.in_flight.is_empty()
    }

    fn take_pending(&mut self) {
        let Some(pending) = self.shared.mailbox().take() else {
            return;
        };
        let frame = pending.frame;
        self.current = Some(Current {
            frame: Rc::new(frame),
            decoded_at: pending.decoded_at,
            counted: false,
        });
    }

    unsafe fn ensure_processor(
        &mut self,
        ctx: &ID3D11DeviceContext,
        codec: Codec,
        input: (u32, u32),
        output: (u32, u32),
    ) -> Result<()> {
        if let Some(p) = &self.processor {
            if p.input == input && p.output == output {
                return Ok(());
            }
        }
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            InputWidth: input.0,
            InputHeight: input.1,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            OutputWidth: output.0,
            OutputHeight: output.1,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = self.video_device.CreateVideoProcessorEnumerator(&desc)?;
        let processor = self.video_device.CreateVideoProcessor(&enumerator, 0)?;
        let video_context: ID3D11VideoContext = ctx.cast().map_err(|e| {
            video_interface_error(
                codec,
                e,
                "D3D11 device context has no ID3D11VideoContext interface",
            )
        })?;
        video_context.VideoProcessorSetStreamFrameFormat(
            &processor,
            0,
            D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
        );
        video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
        // Limited-range BT.709 in, full-range RGB out. Matches the spike; VUI
        // colour signalling is not parsed yet.
        let video_context1: ID3D11VideoContext1 = ctx.cast().map_err(|e| {
            video_interface_error(
                codec,
                e,
                "D3D11 device context has no ID3D11VideoContext1 interface",
            )
        })?;
        video_context1.VideoProcessorSetStreamColorSpace1(
            &processor,
            0,
            DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
        );
        video_context1.VideoProcessorSetOutputColorSpace1(
            &processor,
            DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
        );
        let black = D3D11_VIDEO_COLOR {
            Anonymous: D3D11_VIDEO_COLOR_0 {
                RGBA: D3D11_VIDEO_COLOR_RGBA {
                    R: 0.0,
                    G: 0.0,
                    B: 0.0,
                    A: 1.0,
                },
            },
        };
        video_context.VideoProcessorSetOutputBackgroundColor(&processor, false, &black);
        self.processor = Some(Processor {
            enumerator,
            processor,
            input,
            output,
        });
        Ok(())
    }

    /// Blits `frame` into `viewport` of the render target. Returns the fence value
    /// that marks completion of this blit.
    unsafe fn blit(
        &mut self,
        ctx: &ID3D11DeviceContext,
        rtv: &ID3D11RenderTargetView,
        viewport: Rect,
        frame: &DecodedFrame,
    ) -> Result<u64> {
        if frame.width == 0 || frame.height == 0 {
            bail!("decoded frame has no visible size");
        }
        let pts = frame.pts_100ns;
        self.shared
            .trace("vp_surface_prepare_enter", pts, Instant::now());
        let back: ID3D11Texture2D = rtv
            .GetResource()?
            .cast()
            .context("render target is not a 2D texture")?;
        self.shared.trace("vp_get_desc_enter", pts, Instant::now());
        let mut back_desc = D3D11_TEXTURE2D_DESC::default();
        back.GetDesc(&mut back_desc);
        let mut input_desc = D3D11_TEXTURE2D_DESC::default();
        frame.texture.GetDesc(&mut input_desc);
        self.shared.trace("vp_get_desc_return", pts, Instant::now());
        self.shared
            .trace("vp_ensure_processor_enter", pts, Instant::now());
        let ensure = self.ensure_processor(
            ctx,
            frame.codec,
            (input_desc.Width, input_desc.Height),
            (back_desc.Width, back_desc.Height),
        );
        self.shared
            .trace("vp_ensure_processor_return", pts, Instant::now());
        ensure?;
        let processor = self.processor.as_ref().context("processor missing")?;

        let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: frame.array_slice,
                },
            },
        };
        let mut input = None;
        self.shared
            .trace("vp_input_view_enter", pts, Instant::now());
        let created = self.video_device.CreateVideoProcessorInputView(
            &frame.texture,
            &processor.enumerator,
            &input_desc,
            Some(&mut input),
        );
        self.shared
            .trace("vp_input_view_return", pts, Instant::now());
        created?;
        let input = input.context("input view was not created")?;

        let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut output = None;
        self.shared
            .trace("vp_output_view_enter", pts, Instant::now());
        let created = self.video_device.CreateVideoProcessorOutputView(
            &back,
            &processor.enumerator,
            &output_desc,
            Some(&mut output),
        );
        self.shared
            .trace("vp_output_view_return", pts, Instant::now());
        created?;
        let output = output.context("output view was not created")?;
        self.shared
            .trace("vp_surface_prepare_return", pts, Instant::now());

        self.shared
            .trace("vp_rect_setup_enter", pts, Instant::now());
        let source = frame.source_rect;
        let dest = letterbox(frame.width, frame.height, viewport);
        let target = viewport_rect(viewport);
        let video_context: ID3D11VideoContext = ctx.cast().map_err(|e| {
            video_interface_error(
                frame.codec,
                e,
                "D3D11 device context has no ID3D11VideoContext interface",
            )
        })?;
        let vp = &processor.processor;
        video_context.VideoProcessorSetStreamSourceRect(vp, 0, true, Some(&source));
        video_context.VideoProcessorSetStreamDestRect(vp, 0, true, Some(&dest));
        video_context.VideoProcessorSetOutputTargetRect(vp, true, Some(&target));
        self.shared
            .trace("vp_rect_setup_return", pts, Instant::now());

        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: BOOL(1),
            pInputSurface: ManuallyDrop::new(Some(input)),
            ..Default::default()
        };
        self.shared.trace("vp_blt_enter", pts, Instant::now());
        let blt = video_context.VideoProcessorBlt(vp, &output, 0, std::slice::from_ref(&stream));
        self.shared.trace("vp_blt_return", pts, Instant::now());
        ManuallyDrop::drop(&mut stream.pInputSurface);
        blt.context("VideoProcessorBlt")?;

        let ctx4: ID3D11DeviceContext4 = ctx
            .cast()
            .context("ID3D11DeviceContext4 required for the GPU fence")?;
        let value = self.fence_value + 1;
        self.shared
            .trace("vp_fence_signal_enter", pts, Instant::now());
        let signaled = ctx4.Signal(&self.fence, value);
        self.shared
            .trace("vp_fence_signal_return", pts, Instant::now());
        signaled?;
        self.fence_value = value;
        Ok(value)
    }
}

impl VideoLayer for VideoRenderer {
    fn draw(&mut self, ctx: &ID3D11DeviceContext, rtv: &ID3D11RenderTargetView, viewport: Rect) {
        self.shared.trace("draw_enter", 0, Instant::now());
        self.shared.trace("vp_retire_enter", 0, Instant::now());
        self.retire_completed();
        self.shared.trace("vp_retire_return", 0, Instant::now());
        self.take_pending();
        let Some(mut current) = self.current.take() else {
            self.shared.trace("draw_return", 0, Instant::now());
            return;
        };
        let pts = current.frame.pts_100ns;
        self.shared.trace("draw_selected", pts, Instant::now());
        let frame = current.frame.clone();
        self.shared
            .trace("vp_blit_total_enter", pts, Instant::now());
        let blit = unsafe { self.blit(ctx, rtv, viewport, &frame) };
        self.shared
            .trace("vp_blit_total_return", pts, Instant::now());
        match blit {
            Ok(value) => {
                self.in_flight.push_back((value, frame));
                if !current.counted {
                    current.counted = true;
                    let now = Instant::now();
                    self.shared.stats.note_presented(current.decoded_at, now);
                    if let Some(callback) = &self.presented_callback {
                        callback(current.frame.pts_100ns, now);
                    }
                }
                self.last_error = None;
            }
            Err(error) => {
                self.shared.stats.note_present_error();
                self.last_error = Some(format!("{error:#}"));
            }
        }
        self.current = Some(current);
        self.shared.trace("draw_return", pts, Instant::now());
    }
}

/// Largest centred rectangle with the frame's aspect ratio inside `viewport`.
fn letterbox(width: u32, height: u32, viewport: Rect) -> RECT {
    let (fw, fh) = (width as f32, height as f32);
    let scale = (viewport.w / fw).min(viewport.h / fh).max(0.0);
    let (dw, dh) = ((fw * scale).round(), (fh * scale).round());
    let left = viewport.x + ((viewport.w - dw) / 2.0).floor();
    let top = viewport.y + ((viewport.h - dh) / 2.0).floor();
    RECT {
        left: left as i32,
        top: top as i32,
        right: (left + dw) as i32,
        bottom: (top + dh) as i32,
    }
}

fn viewport_rect(viewport: Rect) -> RECT {
    RECT {
        left: viewport.x.floor() as i32,
        top: viewport.y.floor() as i32,
        right: (viewport.x + viewport.w).ceil() as i32,
        bottom: (viewport.y + viewport.h).ceil() as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letterbox_fits_and_centres() {
        let v = Rect {
            x: 10.0,
            y: 20.0,
            w: 1000.0,
            h: 1000.0,
        };
        // 2:1 source into a square viewport: full width, centred vertically.
        let r = letterbox(2000, 1000, v);
        assert_eq!((r.left, r.right), (10, 1010));
        assert_eq!((r.top, r.bottom), (20 + 250, 20 + 250 + 500));
        // Taller source: full height, centred horizontally.
        let r = letterbox(1000, 2000, v);
        assert_eq!((r.top, r.bottom), (20, 1020));
        assert_eq!((r.left, r.right), (10 + 250, 10 + 250 + 500));
    }

    #[test]
    fn codec_wire_ids_round_trip() {
        assert_eq!(Codec::from_wire_id(1), Some(Codec::Hevc));
        assert_eq!(Codec::from_wire_id(2), Some(Codec::H264));
        assert_eq!(Codec::from_wire_id(0), None);
        assert_eq!(Codec::Hevc.wire_id(), 1);
        assert_eq!(Codec::H264.wire_id(), 2);
    }
}
