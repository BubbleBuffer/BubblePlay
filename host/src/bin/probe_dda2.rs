//! Diagnostic: walk every DXGI adapter/output, create a device per adapter,
//! and attempt DuplicateOutput on each output. Reveals which output(s) DWM
//! actually presents frames on (headless + IddCx virtual display hunting).

use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1, IDXGIResource,
    DXGI_OUTDUPL_FRAME_INFO,
};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFOEXW};

fn monitor_name(h: HMONITOR) -> String {
    let mut mi = MONITORINFOEXW::default();
    mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    unsafe {
        if GetMonitorInfoW(h, &mut mi.monitorInfo).as_bool() {
            let end = mi
                .szDevice
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(mi.szDevice.len());
            String::from_utf16_lossy(&mi.szDevice[..end])
        } else {
            "<GetMonitorInfoW failed>".into()
        }
    }
}

fn wide_to_string(s: &[u16]) -> String {
    let end = s.iter().position(|c| *c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..end])
}

fn probe_output(adapter: &IDXGIAdapter1, output: &IDXGIOutput) {
    let odesc = unsafe { output.GetDesc() }.expect("output GetDesc");
    let dev_name = wide_to_string(&odesc.DeviceName);
    let mon = monitor_name(odesc.Monitor);
    println!(
        "output {dev_name} mon={mon} rect={}x{} attached={} rotation={:?}",
        odesc.DesktopCoordinates.right - odesc.DesktopCoordinates.left,
        odesc.DesktopCoordinates.bottom - odesc.DesktopCoordinates.top,
        odesc.AttachedToDesktop.as_bool(),
        odesc.Rotation,
    );

    // Device on THIS adapter (D3D11CreateDevice wants the IDXGIAdapter).
    let mut device: Option<ID3D11Device> = None;
    let mut ctx: Option<ID3D11DeviceContext> = None;
    let mut level = windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL::default();
    let levels = [D3D_FEATURE_LEVEL_11_1];
    if let Err(e) = unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            windows::Win32::Foundation::HMODULE::default(),
            Default::default(),
            Some(&levels),
            7,
            Some(&mut device),
            Some(&mut level),
            Some(&mut ctx),
        )
    } {
        println!("  d3d11 device create failed: {e:?}");
        return;
    }
    let device = device.expect("device");

    let Ok(out1) = output.cast::<IDXGIOutput1>() else {
        println!("  no IDXGIOutput1");
        return;
    };
    let mut dup = match unsafe { out1.DuplicateOutput(&device) } {
        Ok(d) => d,
        Err(e) => {
            println!("  DuplicateOutput failed: {e:?}");
            return;
        }
    };
    let ddesc = unsafe { dup.GetDesc() };
    println!(
        "  DuplicateOutput OK {}x{} fmt={:?}",
        ddesc.ModeDesc.Width, ddesc.ModeDesc.Height, ddesc.ModeDesc.Format
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut acquired = 0u64;
    let mut presents = 0u64;
    let mut last: i64 = -1;
    while Instant::now() < deadline {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res: Option<IDXGIResource> = None;
        match unsafe { dup.AcquireNextFrame(200, &mut info, &mut res) } {
            Ok(()) => {
                acquired += 1;
                let qpc = info.LastPresentTime;
                if qpc != last && qpc != 0 {
                    presents += 1;
                    last = qpc;
                }
                drop(res);
                let _ = unsafe { dup.ReleaseFrame() };
            }
            Err(e) => {
                if acquired == 0 {
                    println!("  acquire err: {e:?}");
                }
                break;
            }
        }
    }
    println!("  => acquired={acquired} new_presents={presents}");
}

fn main() {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.expect("CreateDXGIFactory1");
    for ai in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(ai) }) else {
            break;
        };
        let adesc = unsafe { adapter.GetDesc1() }.unwrap();
        let name = wide_to_string(&adesc.Description);
        println!(
            "adapter {ai}: {name} vendor=0x{:X} flags=0x{:X}",
            adesc.VendorId, adesc.Flags
        );
        for oi in 0.. {
            let Ok(output) = (unsafe { adapter.EnumOutputs(oi) }) else {
                break;
            };
            probe_output(&adapter, &output);
        }
    }
}
