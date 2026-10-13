//! Read-only display identity and cadence snapshots. No mode changes or waits.
//! DWM timing is global on modern Windows (the API requires a NULL HWND), while
//! DisplayConfig identifies each active physical target and its rational mode.
use anyhow::{bail, Context, Result};
use std::{collections::BTreeMap, mem::size_of};
use windows::{
    core::{Interface, PCWSTR},
    Win32::{
        Devices::Display::*,
        Foundation::{
            ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_PARAMETER, ERROR_SUCCESS, HWND, LUID,
        },
        Graphics::{
            Direct3D11::ID3D11Device,
            Dwm::{DwmGetCompositionTimingInfo, DWM_TIMING_INFO},
            Dxgi::{IDXGIDevice, DXGI_ERROR_NOT_FOUND},
            Gdi::{
                EnumDisplaySettingsW, GetMonitorInfoW, MonitorFromWindow, DEVMODEW,
                ENUM_CURRENT_SETTINGS, MONITORINFOEXW, MONITOR_DEFAULTTONEAREST,
            },
        },
        System::Performance::QueryPerformanceFrequency,
    },
};

fn wide(s: &[u16]) -> String {
    String::from_utf16_lossy(&s[..s.iter().position(|v| *v == 0).unwrap_or(s.len())])
}
fn luid(id: LUID) -> String {
    format!("{:08X}:{:08X}", id.HighPart as u32, id.LowPart)
}
fn same_luid(a: LUID, b: LUID) -> bool {
    a.HighPart == b.HighPart && a.LowPart == b.LowPart
}
fn ratio(n: u32, d: u32) -> String {
    if d == 0 {
        format!("{n}/{d} (undefined)")
    } else {
        format!("{n}/{d} = {:.6} Hz", n as f64 / d as f64)
    }
}

unsafe fn source_name(id: LUID, source: u32) -> String {
    let mut name = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
    name.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
        r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
        size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
        adapterId: id,
        id: source,
    };
    let status = DisplayConfigGetDeviceInfo(&mut name.header);
    if status == 0 {
        wide(&name.viewGdiDeviceName)
    } else {
        format!("source-name-error={status}")
    }
}
unsafe fn target_name(id: LUID, target: u32) -> String {
    let mut name = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
    name.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
        r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
        size: size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
        adapterId: id,
        id: target,
    };
    let status = DisplayConfigGetDeviceInfo(&mut name.header);
    if status == 0 {
        format!("friendly={:?}; device_path={:?}; EDID_manufacturer={:#06x}; EDID_product={:#06x}; connector={}; technology={}",
            wide(&name.monitorFriendlyDeviceName), wide(&name.monitorDevicePath),
            name.edidManufactureId, name.edidProductCodeId, name.connectorInstance, name.outputTechnology.0)
    } else {
        format!("target-name-error={status}")
    }
}
unsafe fn adapter_name(id: LUID) -> String {
    let mut name = DISPLAYCONFIG_ADAPTER_NAME::default();
    name.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
        r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME,
        size: size_of::<DISPLAYCONFIG_ADAPTER_NAME>() as u32,
        adapterId: id,
        id: 0,
    };
    let status = DisplayConfigGetDeviceInfo(&mut name.header);
    if status == 0 {
        wide(&name.adapterDevicePath)
    } else {
        format!("adapter-name-error={status}")
    }
}

unsafe fn active_paths() -> Result<(
    Vec<DISPLAYCONFIG_PATH_INFO>,
    Vec<DISPLAYCONFIG_MODE_INFO>,
    u32,
)> {
    // Try full Windows 11 cadence awareness, then Windows 10 virtual modes,
    // then the original active-path query. Report the flags that succeeded.
    for flags in [
        QDC_ONLY_ACTIVE_PATHS | QDC_VIRTUAL_MODE_AWARE | QDC_VIRTUAL_REFRESH_RATE_AWARE,
        QDC_ONLY_ACTIVE_PATHS | QDC_VIRTUAL_MODE_AWARE,
        QDC_ONLY_ACTIVE_PATHS,
    ] {
        for _ in 0..4 {
            let (mut path_count, mut mode_count) = (0, 0);
            let status = GetDisplayConfigBufferSizes(flags, &mut path_count, &mut mode_count);
            if status == ERROR_INVALID_PARAMETER {
                break;
            }
            if status != ERROR_SUCCESS {
                bail!(
                    "GetDisplayConfigBufferSizes flags={:#x}: {}",
                    flags.0,
                    status.0
                );
            }
            if path_count > 4096 || mode_count > 16384 {
                bail!("Unreasonable display-config allocation counts {path_count}/{mode_count}");
            }
            let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
            let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
            let status = QueryDisplayConfig(
                flags,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                None,
            );
            if status == ERROR_INSUFFICIENT_BUFFER {
                continue;
            }
            if status == ERROR_INVALID_PARAMETER {
                break;
            }
            if status != ERROR_SUCCESS {
                bail!("QueryDisplayConfig flags={:#x}: {}", flags.0, status.0);
            }
            paths.truncate(path_count as usize);
            modes.truncate(mode_count as usize);
            return Ok((paths, modes, flags.0));
        }
    }
    bail!("Active display query failed after bounded topology retries/flag fallbacks")
}

/// Returns a reviewable multi-line identity/cadence snapshot. Query failures for
/// optional global DWM/DisplayConfig data remain explicit in the report.
pub fn report(device: &ID3D11Device, hwnd: HWND) -> Result<String> {
    unsafe {
        let mut lines = vec![format!("display_snapshot HWND={:?}", hwnd.0)];
        let dxgi: IDXGIDevice = device.cast().context("DXGI device for display report")?;
        let adapter = dxgi.GetAdapter()?;
        let desc = adapter.GetDesc()?;
        lines.push(format!(
            "decode_adapter={:?}; LUID={}; vendor={:#x}; device={:#x}",
            wide(&desc.Description),
            luid(desc.AdapterLuid),
            desc.VendorId,
            desc.DeviceId
        ));

        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        let selected = if GetMonitorInfoW(monitor, &mut info.monitorInfo).as_bool() {
            let name = wide(&info.szDevice);
            lines.push(format!("window_monitor={:?}; HMONITOR={:?}; desktop={:?}; primary={}; selection=largest-window-intersection-or-nearest", 
                name, monitor.0, info.monitorInfo.rcMonitor, info.monitorInfo.dwFlags & 1 != 0));
            name
        } else {
            lines.push("window_monitor=unavailable (GetMonitorInfoW failed)".into());
            String::new()
        };
        for index in 0..64 {
            let output = match adapter.EnumOutputs(index) {
                Ok(output) => output,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => {
                    lines.push(format!("DXGI EnumOutputs({index}) failed: {e}"));
                    break;
                }
            };
            let desc = output.GetDesc()?;
            let name = wide(&desc.DeviceName);
            let mut mode = DEVMODEW::default();
            mode.dmSize = size_of::<DEVMODEW>() as u16;
            let settings = if EnumDisplaySettingsW(
                PCWSTR(desc.DeviceName.as_ptr()),
                ENUM_CURRENT_SETTINGS,
                &mut mode,
            )
            .as_bool()
            {
                format!(
                    "{}x{} integer_refresh={} Hz; flags={:#x}",
                    mode.dmPelsWidth,
                    mode.dmPelsHeight,
                    mode.dmDisplayFrequency,
                    mode.Anonymous2.dmDisplayFlags
                )
            } else {
                "EnumDisplaySettingsW failed".into()
            };
            lines.push(format!("DXGI_output[{index}]={name:?}; selected_HMONITOR={}; attached={}; desktop={:?}; {settings}",
                desc.Monitor == monitor, desc.AttachedToDesktop.as_bool(), desc.DesktopCoordinates));
        }

        let mut timing = DWM_TIMING_INFO::default();
        timing.cbSize = size_of::<DWM_TIMING_INFO>() as u32;
        // NULL is mandatory since Windows 8.1; not a per-HWND measurement.
        match DwmGetCompositionTimingInfo(HWND::default(), &mut timing) {
            Ok(()) => {
                // DWM structures are packed, so copy every field before format!
                // takes references to it.
                let rn = timing.rateRefresh.uiNumerator;
                let rd = timing.rateRefresh.uiDenominator;
                let cn = timing.rateCompose.uiNumerator;
                let cd = timing.rateCompose.uiDenominator;
                let period = timing.qpcRefreshPeriod;
                let vblank = timing.qpcVBlank;
                let refresh = timing.cRefresh;
                let dxrefresh = timing.cDXRefresh;
                let composed = timing.qpcCompose;
                let mut frequency = 0;
                QueryPerformanceFrequency(&mut frequency)?;
                let derived = if period > 0 {
                    frequency as f64 / period as f64
                } else {
                    f64::NAN
                };
                lines.push(format!("DWM_GLOBAL rateRefresh={}; rateCompose={}; qpcRefreshPeriod={period}; QPC_frequency={frequency}; period_derived_Hz={derived:.6}; qpcVBlank={vblank}; cRefresh={refresh}; cDXRefresh={dxrefresh}; qpcCompose={composed}; HWND=NULL, no target association promised", ratio(rn, rd), ratio(cn, cd)));
            }
            Err(e) => lines.push(format!("DWM_GLOBAL unavailable: {e}")),
        }

        match active_paths() {
            Ok((paths, modes, flags)) => {
                lines.push(format!(
                    "DisplayConfig active_paths={}; query_flags={flags:#x}; mode_count={}",
                    paths.len(),
                    modes.len()
                ));
                let mut sources = BTreeMap::<String, Vec<String>>::new();
                for (index, path) in paths.iter().enumerate() {
                    let source = source_name(path.sourceInfo.adapterId, path.sourceInfo.id);
                    let target = target_name(path.targetInfo.adapterId, path.targetInfo.id);
                    let source_key =
                        format!("{}:{}", luid(path.sourceInfo.adapterId), path.sourceInfo.id);
                    sources.entry(source_key.clone()).or_default().push(format!(
                        "{}:{}",
                        luid(path.targetInfo.adapterId),
                        path.targetInfo.id
                    ));
                    let source_mode = modes
                        .iter()
                        .find(|m| {
                            m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
                                && m.id == path.sourceInfo.id
                                && same_luid(m.adapterId, path.sourceInfo.adapterId)
                        })
                        .map(|m| {
                            let mode = m.Anonymous.sourceMode;
                            format!(
                                "{}x{} at ({},{})",
                                mode.width, mode.height, mode.position.x, mode.position.y
                            )
                        })
                        .unwrap_or_else(|| "source-mode unavailable".into());
                    let signal = modes.iter().find(|m| m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_TARGET
                        && m.id == path.targetInfo.id && same_luid(m.adapterId, path.targetInfo.adapterId))
                        .map(|m| {
                            let signal = m.Anonymous.targetMode.targetVideoSignalInfo;
                            format!("physical_vSync={}; hSync={}; active={}x{}; total={}x{}; pixelRate={}; scanline={}",
                                ratio(signal.vSyncFreq.Numerator, signal.vSyncFreq.Denominator),
                                ratio(signal.hSyncFreq.Numerator, signal.hSyncFreq.Denominator),
                                signal.activeSize.cx, signal.activeSize.cy, signal.totalSize.cx, signal.totalSize.cy,
                                signal.pixelRate, signal.scanLineOrdering.0)
                        }).unwrap_or_else(|| "target-signal unavailable".into());
                    lines.push(format!("DisplayConfig_path[{index}] selected_source={}; source={source:?}; source_key={source_key}; source_mode={source_mode}; target_adapter={}; target_id={}; path_refresh={}; rotation={}; scaling={}; available={}; path_flags={:#x}; {signal}; {target}; adapter_path={:?}",
                        !selected.is_empty() && source.eq_ignore_ascii_case(&selected),
                        luid(path.targetInfo.adapterId), path.targetInfo.id,
                        ratio(path.targetInfo.refreshRate.Numerator, path.targetInfo.refreshRate.Denominator),
                        path.targetInfo.rotation.0, path.targetInfo.scaling.0, path.targetInfo.targetAvailable.as_bool(),
                        path.flags, adapter_name(path.targetInfo.adapterId)));
                }
                let clones: Vec<String> = sources
                    .iter()
                    .filter(|(_, targets)| targets.len() > 1)
                    .map(|(source, targets)| format!("source={source} targets={targets:?}"))
                    .collect();
                lines.push(format!(
                    "active_source_count={}; clone_groups={}; {}",
                    sources.len(),
                    clones.len(),
                    if clones.is_empty() {
                        "no repeated active source IDs observed".into()
                    } else {
                        clones.join("; ")
                    }
                ));
            }
            Err(e) => lines.push(format!("DisplayConfig unavailable: {e:#}")),
        }
        lines.push("Cadences above describe configured modes and a global DWM snapshot; DXGI counter-derived rate alone does not identify the physical output, VRR state, clone topology, or cause of a discrepancy. No optical/actual-flip timestamp is claimed.".into());
        Ok(lines.join("\n"))
    }
}
