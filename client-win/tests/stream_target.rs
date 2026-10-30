//! The shared texture bridge: a frame drawn on the session's device is readable on a second
//! device through the DXGI shared handle, the way Flutter's engine reads it.
#![cfg(windows)]

use bubbleplay_client::stream::{player::create_device, target::SharedPair};
use windows::{
    Win32::Graphics::Direct3D11::{
        ID3D11Device, ID3D11Texture2D, D3D11_TEXTURE2D_DESC,
    },
};

    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_USAGE_STAGING,
    };

    fn read_pixel(device: &ID3D11Device, texture: &ID3D11Texture2D) -> [u8; 4] {
        let ctx = unsafe { device.GetImmediateContext().unwrap() };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        let mut staging = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging)).unwrap() };
        let staging = staging.unwrap();
        unsafe { ctx.CopyResource(&staging, texture) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).unwrap() };
        let pixel = unsafe { *(mapped.pData as *const [u8; 4]) };
        unsafe { ctx.Unmap(&staging, 0) };
        pixel
    }

    /// A frame drawn on the session's device is visible on a second device through the shared
    /// handle, which is how Flutter's engine reads it. The two textures alternate.
    #[test]
    fn a_finished_frame_is_readable_through_the_shared_handle_on_another_device() {
        let producer = create_device().expect("producer device");
        let consumer = create_device().expect("consumer device");
        let ctx = unsafe { producer.GetImmediateContext().unwrap() };
        let mut pair = SharedPair::new(&producer, 64, 36).unwrap();
        assert!(pair.front().is_none());
        for (n, color) in [[1.0f32, 0.0, 0.0, 1.0], [0.0, 1.0, 0.0, 1.0]].iter().enumerate() {
            let back_handle = pair.back().handle();
            unsafe { ctx.ClearRenderTargetView(pair.back().rtv(), color) };
            pair.finish(&ctx);
            let front = pair.front().expect("a front surface after finish");
            assert_eq!(front.handle(), back_handle, "front is the one just drawn ({n})");
            let mut opened: Option<ID3D11Texture2D> = None;
            unsafe { consumer.OpenSharedResource(front.handle(), &mut opened) }
                .expect("open the shared handle");
            let pixel = read_pixel(&consumer, &opened.unwrap());
            // BGRA
            let expected = [
                (color[2] * 255.0) as u8,
                (color[1] * 255.0) as u8,
                (color[0] * 255.0) as u8,
                255,
            ];
            assert_eq!(pixel, expected, "pixel of frame {n}");
        }
        assert_ne!(pair.handles()[0], pair.handles()[1]);
    }
