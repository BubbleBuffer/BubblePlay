//! Isolation probe: run only the inline VDD keepalive (no DDA, no encoder)
//! and sample display attach state each second. Answers: does the keepalive
//! alone hold the virtual display, or does the flapping come from the
//! DDA/encoder interaction?

use std::thread;
use std::time::Duration;

use anyhow::Result;
#[path = "../vdd.rs"]
mod vdd;
use windows::Win32::Graphics::Gdi::{
    EnumDisplayDevicesW, DISPLAY_DEVICEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP,
};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    println!("starting VDD keepalive (15s, no DDA)");
    let _keep = vdd::VddKeepalive::start()?;

    for i in 1..=15 {
        thread::sleep(Duration::from_secs(1));
        let attached = parsec_attached_count();
        println!("t={i}s parsec_attached={attached}");
    }
    println!("dropping keepalive");
    Ok(())
}

fn parsec_attached_count() -> u32 {
    let mut n = 0u32;
    let mut i = 0u32;
    loop {
        let mut d = DISPLAY_DEVICEW::default();
        d.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
        let ok = unsafe { EnumDisplayDevicesW(None, i, &mut d, 0) };
        if !ok.as_bool() {
            break;
        }
        let name = d
            .DeviceString
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| char::from_u32(c as u32).unwrap_or('?'))
            .collect::<String>();
        if name.contains("Parsec")
            && d.StateFlags.contains(DISPLAY_DEVICE_ATTACHED_TO_DESKTOP)
        {
            n += 1;
        }
        i += 1;
    }
    n
}
