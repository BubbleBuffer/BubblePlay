//! Diagnostic: snapshot one frame from each monitor to PNG so we can see
//! what DWM is actually presenting on each output (headless debugging).

use std::time::{Duration, Instant};

use windows_capture::dxgi_duplication_api::{DxgiDuplicationApi, Error as CaptureError};
use windows_capture::encoder::ImageFormat;
use windows_capture::monitor::Monitor;

fn main() {
    let monitors = Monitor::enumerate().expect("enumerate monitors");
    for (i, m) in monitors.iter().enumerate() {
        let dev = m.device_name().unwrap_or_default();
        let mut api = match DxgiDuplicationApi::new(*m) {
            Ok(api) => api,
            Err(e) => {
                println!("[{i}] {dev}: session create FAILED: {e:?}");
                continue;
            }
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match api.acquire_next_frame(200) {
                Ok(mut frame) => {
                    let dev_clean: String = dev
                        .chars()
                        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                        .collect();
                    let path = std::env::temp_dir().join(format!("dda_snap_{i}_{dev_clean}.png"));
                    match frame.buffer() {
                        Ok(mut buf) => match buf.save_as_image(&path, ImageFormat::Png) {
                            Ok(()) => println!("[{i}] {dev}: saved {}", path.display()),
                            Err(e) => println!("[{i}] {dev}: save failed: {e:?}"),
                        },
                        Err(e) => println!("[{i}] {dev}: buffer failed: {e:?}"),
                    }
                    break;
                }
                Err(CaptureError::Timeout) => {
                    if Instant::now() > deadline {
                        println!("[{i}] {dev}: no frame in 3s");
                        break;
                    }
                }
                Err(e) => {
                    println!("[{i}] {dev}: acquire error: {e:?}");
                    break;
                }
            }
        }
    }
}
