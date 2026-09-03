//! Diagnostic: enumerate monitors and probe DDA frame availability per output.
//!
//! Headless context: the desktop may be attached to outputs that never present
//! (no scan-out clock). This probe creates a DDA session per monitor and counts
//! how many frames (and new presents) arrive in a fixed window.

use std::time::{Duration, Instant};

use windows_capture::dxgi_duplication_api::{DxgiDuplicationApi, Error as CaptureError};
use windows_capture::monitor::Monitor;

fn main() {
    let monitors = Monitor::enumerate().expect("enumerate monitors");
    println!("{} monitors:", monitors.len());
    for m in &monitors {
        println!(
            "  idx={:?} name={:?} dev={:?} {:?}x{:?} @{:?}Hz",
            m.index(),
            m.name(),
            m.device_name(),
            m.width(),
            m.height(),
            m.refresh_rate(),
        );
    }

    for (i, m) in monitors.iter().enumerate() {
        let mut api = match DxgiDuplicationApi::new(m.clone()) {
            Ok(api) => api,
            Err(e) => {
                println!("[{i}] session create FAILED: {e:?}");
                continue;
            }
        };
        println!("[{i}] session ok {}x{}", api.width(), api.height());
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut acquired = 0u64;
        let mut presents = 0u64;
        let mut last_qpc: i64 = 0;
        while Instant::now() < deadline {
            match api.acquire_next_frame(200) {
                Ok(frame) => {
                    acquired += 1;
                    let qpc = frame.frame_info().LastPresentTime;
                    if qpc != 0 && qpc != last_qpc {
                        presents += 1;
                        last_qpc = qpc;
                    }
                }
                Err(CaptureError::Timeout) => {}
                Err(CaptureError::AccessLost) => {
                    println!("[{i}] ACCESS LOST");
                    break;
                }
                Err(e) => {
                    println!("[{i}] acquire error: {e:?}");
                    break;
                }
            }
        }
        println!("[{i}] acquired={acquired} new_presents={presents}");
    }
}
