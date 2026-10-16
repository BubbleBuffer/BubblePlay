//! Placeholder video layer: a calm dark vertical gradient (#1B1D21 to #2A2D33)
//! in horizontal bands, drawn with ClearView. No shaders.

use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11DeviceContext, ID3D11DeviceContext1, ID3D11RenderTargetView, ID3D11View,
};

use super::{Rect, VideoLayer};

const BANDS: usize = 16;
const TOP: [u8; 3] = [0x1B, 0x1D, 0x21];
const BOTTOM: [u8; 3] = [0x2A, 0x2D, 0x33];

#[derive(Default)]
pub struct PlaceholderVideo;

impl VideoLayer for PlaceholderVideo {
    fn draw(&mut self, ctx: &ID3D11DeviceContext, rtv: &ID3D11RenderTargetView, viewport: Rect) {
        let width = viewport.w.round() as i32;
        let height = viewport.h.round() as i32;
        if width <= 0 || height <= 0 {
            return;
        }
        let Ok(ctx1) = ctx.cast::<ID3D11DeviceContext1>() else { return };
        let Ok(view) = rtv.cast::<ID3D11View>() else { return };
        let left = viewport.x.round() as i32;
        let top = viewport.y.round() as i32;
        for band in 0..BANDS {
            let y0 = top + height * band as i32 / BANDS as i32;
            let y1 = top + height * (band as i32 + 1) / BANDS as i32;
            let t = band as f32 / (BANDS - 1) as f32;
            let channel = |k: usize| (TOP[k] as f32 + (BOTTOM[k] as f32 - TOP[k] as f32) * t) / 255.0;
            let color = [channel(0), channel(1), channel(2), 1.0];
            let rect = RECT { left, top: y0, right: left + width, bottom: y1 };
            unsafe { ctx1.ClearView(&view, &color, Some(&[rect])) };
        }
    }
}
