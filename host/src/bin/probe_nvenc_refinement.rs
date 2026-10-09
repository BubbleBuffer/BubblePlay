//! Standalone hardware validation for a single-stream quiescent HEVC refinement pulse.
//!
//! The probe encodes one unchanged 1080p UI-like texture through production
//! CBR and a matched external-budget VBR pair. The VBR candidate receives a
//! bounded full-frame delta-QP refinement pulse. All cohorts are evaluated
//! against BubblePlay's cumulative 6 Mbps transport budget using exact NVENC
//! reconstructed-frame luma error measured on the GPU.

use bubbleplay_host::encode;
use bubbleplay_host::frame_damage;
use bubbleplay_host::reconstruction_feedback;

use std::ffi::c_void;
use std::path::PathBuf;

use anyhow::{ensure, Context, Result};
use encode::{EncoderOptions, FrameDamage, Nvenc, Preset, RateControlMode, RoiMode, VideoCodec};
use frame_damage::{DamageRect, HevcQpDeltaMap};
use reconstruction_feedback::ReconstructionFeedbackAnalyzer;
use serde::Serialize;
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Texture2D, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const FPS: u32 = 30;
const BASELINE_BITRATE_BPS: u32 = 6_000_000;
const PULSE_QP_DELTA: i8 = -5;
const PULSE_SEQUENCE: u32 = 30;
const PULSE_FRAMES: u32 = 3;
const FOLLOWUP_FRAMES: u32 = 15;
const SUMMARY_FRAMES: usize = 5;
const CANDIDATE_TARGET_QUALITY: u8 = 23;
const CANDIDATE_PEAK_BITRATE_MULTIPLIER: u8 = 4;
const CANDIDATE_VBV_FRAMES: u8 = 4;

#[derive(Debug, Clone, Copy)]
struct RefinementEvidence {
    control_pulse_error: u64,
    candidate_pulse_error: u64,
    production_persistent_error: u64,
    control_persistent_error: u64,
    candidate_persistent_error: u64,
    candidate_total_bytes: u64,
    wire_budget_bytes: u64,
}

fn refinement_gain_is_persistent(evidence: RefinementEvidence) -> bool {
    let control_pulse = u128::from(evidence.control_pulse_error);
    let candidate_pulse = u128::from(evidence.candidate_pulse_error);
    let control_persistent = u128::from(evidence.control_persistent_error);
    let candidate_persistent = u128::from(evidence.candidate_persistent_error);
    candidate_pulse * 100 <= control_pulse * 98
        && candidate_persistent * 100 <= control_persistent * 98
        && candidate_persistent <= u128::from(evidence.production_persistent_error)
        && evidence.candidate_total_bytes <= evidence.wire_budget_bytes
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FrameMeasurement {
    sequence: u32,
    cohort: &'static str,
    phase: &'static str,
    rate_control: &'static str,
    bitrate_bps: u32,
    qp_delta: Option<i8>,
    encoded_bytes: usize,
    is_idr: bool,
    reconstruction_error: u64,
    encode_us: u64,
    feedback_submit_us: u32,
    feedback_readback_us: u32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProbeReport {
    schema_version: u32,
    codec: &'static str,
    width: u32,
    height: u32,
    fps: u32,
    baseline_bitrate_bps: u32,
    wire_budget_bytes: u64,
    candidate_target_quality: u8,
    candidate_peak_bitrate_bps: u32,
    candidate_vbv_frames: u8,
    pulse_qp_delta: i8,
    pulse_frames: u32,
    pulse_kind: &'static str,
    baseline_prefix_exact: bool,
    production_control_persistent_error: u64,
    vbr_control_persistent_error: u64,
    vbr_candidate_persistent_error: u64,
    control_pulse_error: u64,
    candidate_pulse_error: u64,
    control_persistent_error: u64,
    candidate_persistent_error: u64,
    pulse_error_reduction_percent: f64,
    persistent_error_reduction_percent: f64,
    control_pulse_bytes: usize,
    pulse_bytes: usize,
    pulse_extra_bytes: usize,
    pulse_wire_time_at_cap_ms: f64,
    pulse_extra_wire_time_at_cap_ms: f64,
    control_total_bytes: u64,
    candidate_total_bytes: u64,
    production_control_total_bytes: u64,
    candidate_within_wire_budget: bool,
    persistent_gain: bool,
    production_control_frames: Vec<FrameMeasurement>,
    control_frames: Vec<FrameMeasurement>,
    candidate_frames: Vec<FrameMeasurement>,
}

fn texture_desc() -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: WIDTH,
        Height: HEIGHT,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: 0,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    }
}

fn create_device() -> Result<ID3D11Device> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .context("D3D11CreateDevice")?;
    device.context("D3D11CreateDevice returned no device")
}

fn pseudo_glyph_bit(code: u32, x: u32, y: u32) -> bool {
    if x == 0 || x == 5 || y == 0 || y == 8 {
        return (code.wrapping_add(x * 3).wrapping_add(y * 5) & 3) == 0;
    }
    let mixed =
        code.wrapping_mul(0x045d_9f3b).rotate_left((y * 3 + x) & 31) ^ (x * 0x9e37) ^ (y * 0x7f4a);
    mixed & 1 != 0
}

fn create_ui_texture(device: &ID3D11Device) -> Result<ID3D11Texture2D> {
    let mut pixels = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let offset = ((y * WIDTH + x) * 4) as usize;
            let panel = x / 480;
            let gradient = ((x * 31 / WIDTH + y * 19 / HEIGHT) & 0x3f) as u8;
            let mut b = 22u8.saturating_add(gradient / 3);
            let mut g = 25u8.saturating_add(gradient / 2);
            let mut r = 31u8.saturating_add(gradient / 2);

            if x % 480 < 2 || y % 180 < 2 {
                b = 78;
                g = 72;
                r = 66;
            }

            let in_text_area = (48..1872).contains(&x) && (48..920).contains(&y);
            if in_text_area {
                let glyph_x = (x - 48) % 9;
                let glyph_y = (y - 48) % 14;
                let column = (x - 48) / 9;
                let row = (y - 48) / 14;
                if glyph_x < 6
                    && glyph_y < 9
                    && pseudo_glyph_bit(column + row * 211 + panel * 997, glyph_x, glyph_y)
                {
                    let accent = ((row + column) % 11) == 0;
                    b = if accent { 236 } else { 214 };
                    g = if accent { 188 } else { 224 };
                    r = if accent { 105 } else { 232 };
                }
            }

            if (960..1040).contains(&y) {
                let ramp = (x * 255 / (WIDTH - 1)) as u8;
                b = ramp;
                g = 255u8.saturating_sub(ramp);
                r = ramp / 2 + 64;
            }

            pixels[offset] = b;
            pixels[offset + 1] = g;
            pixels[offset + 2] = r;
            pixels[offset + 3] = 0xff;
        }
    }
    let initial = D3D11_SUBRESOURCE_DATA {
        pSysMem: pixels.as_ptr().cast::<c_void>(),
        SysMemPitch: WIDTH * 4,
        SysMemSlicePitch: WIDTH * HEIGHT * 4,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&texture_desc(), Some(&initial), Some(&mut texture)) }
        .context("CreateTexture2D(refinement input)")?;
    texture.context("CreateTexture2D(refinement input) returned no texture")
}

#[allow(clippy::too_many_arguments)]
fn encode_and_measure(
    encoder: &mut Nvenc,
    feedback: &mut ReconstructionFeedbackAnalyzer,
    input: &ID3D11Texture2D,
    sequence: u32,
    cohort: &'static str,
    phase: &'static str,
    rate_control: &'static str,
    bitrate_bps: u32,
    requested_qp_delta: Option<i8>,
    damage: Option<FrameDamage<'_>>,
) -> Result<FrameMeasurement> {
    let encoded = encoder.encode_texture_with_feedback(
        input.as_raw(),
        damage,
        feedback.reconstruction_texture_raw(sequence),
    )?;
    let submitted = feedback.submit(input, sequence)?;
    ensure!(
        submitted.submitted,
        "feedback reducer did not accept sequence {sequence}"
    );
    let reduced = feedback
        .collect_blocking(sequence)?
        .context("feedback reducer returned no exact frame")?;
    ensure!(
        reduced.sequence == sequence,
        "feedback result sequence mismatch"
    );
    let applied_qp_delta = encoded
        .qp_map
        .as_ref()
        .and_then(|stats| (stats.min_delta == stats.max_delta).then_some(stats.min_delta));
    ensure!(
        applied_qp_delta == requested_qp_delta,
        "requested refinement QP map was not applied exactly"
    );
    Ok(FrameMeasurement {
        sequence,
        cohort,
        phase,
        rate_control,
        bitrate_bps,
        qp_delta: applied_qp_delta,
        encoded_bytes: encoded.data.len(),
        is_idr: encoded.is_idr,
        reconstruction_error: reduced.errors.iter().map(|value| u64::from(*value)).sum(),
        encode_us: encoded.encode_us,
        feedback_submit_us: submitted.submit_us,
        feedback_readback_us: reduced.readback_us,
    })
}

fn median(values: impl IntoIterator<Item = u64>) -> u64 {
    let mut values = values.into_iter().collect::<Vec<_>>();
    values.sort_unstable();
    values[values.len() / 2]
}

fn reduction_percent(baseline: u64, candidate: u64) -> f64 {
    (baseline as f64 - candidate as f64) * 100.0 / baseline.max(1) as f64
}

fn run_sequence(
    device: &ID3D11Device,
    input: &ID3D11Texture2D,
    cohort: &'static str,
    rate_control: RateControlMode,
    refinement_pulse: bool,
) -> Result<Vec<FrameMeasurement>> {
    let mut feedback = ReconstructionFeedbackAnalyzer::new(device, WIDTH, HEIGHT)?;
    let mut options = EncoderOptions::production(Preset::P3);
    options.roi_mode = RoiMode::Feedback;
    options.rate_control = rate_control;
    if matches!(rate_control, RateControlMode::ExternalBudgetVbr { .. }) {
        options.vbv_frames = CANDIDATE_VBV_FRAMES;
    }
    let mut encoder = Nvenc::new_with_options_native_codec(
        device.as_raw(),
        WIDTH,
        HEIGHT,
        WIDTH,
        HEIGHT,
        FPS,
        BASELINE_BITRATE_BPS,
        options,
        VideoCodec::Hevc,
    )?;
    let pulse_map = HevcQpDeltaMap {
        columns: WIDTH.div_ceil(32),
        rows: HEIGHT.div_ceil(32),
        values: vec![PULSE_QP_DELTA; (WIDTH.div_ceil(32) * HEIGHT.div_ceil(32)) as usize],
    };
    let empty_damage = [];
    let final_sequence = PULSE_SEQUENCE + PULSE_FRAMES + FOLLOWUP_FRAMES - 1;
    let mut frames = Vec::with_capacity((final_sequence + 1) as usize);
    for sequence in 0..=final_sequence {
        let in_pulse = (PULSE_SEQUENCE..PULSE_SEQUENCE + PULSE_FRAMES).contains(&sequence);
        let phase = if sequence < PULSE_SEQUENCE {
            "baseline"
        } else if in_pulse {
            if refinement_pulse {
                "pulse-qp"
            } else {
                "matched-control"
            }
        } else {
            "restored"
        };
        let qp_delta = if refinement_pulse && in_pulse {
            Some(PULSE_QP_DELTA)
        } else {
            None
        };
        let damage = qp_delta.map(|_| FrameDamage {
            source_width: WIDTH,
            source_height: HEIGHT,
            crop: DamageRect {
                left: 0,
                top: 0,
                right: WIDTH,
                bottom: HEIGHT,
            },
            rects: &empty_damage,
            edge_scores: None,
            prepared_qp_map: Some(&pulse_map),
        });
        let measurement = encode_and_measure(
            &mut encoder,
            &mut feedback,
            input,
            sequence,
            cohort,
            phase,
            match rate_control {
                RateControlMode::Cbr => "cbr",
                RateControlMode::ExternalBudgetVbr { .. } => "external-budget-vbr",
            },
            BASELINE_BITRATE_BPS,
            qp_delta,
            damage,
        )?;
        frames.push(measurement);
    }
    ensure!(
        frames[PULSE_SEQUENCE as usize..(PULSE_SEQUENCE + PULSE_FRAMES) as usize]
            .iter()
            .all(|frame| !frame.is_idr),
        "a matched refinement picture unexpectedly became an IDR"
    );
    Ok(frames)
}

fn run_probe() -> Result<ProbeReport> {
    let device = create_device()?;
    let input = create_ui_texture(&device)?;
    let candidate_rate_control = RateControlMode::ExternalBudgetVbr {
        target_quality: CANDIDATE_TARGET_QUALITY,
        peak_bitrate_multiplier: CANDIDATE_PEAK_BITRATE_MULTIPLIER,
    };
    let production_control_frames = run_sequence(
        &device,
        &input,
        "production-cbr-control",
        RateControlMode::Cbr,
        false,
    )?;
    let control_frames = run_sequence(
        &device,
        &input,
        "vbr-control",
        candidate_rate_control,
        false,
    )?;
    let candidate_frames = run_sequence(
        &device,
        &input,
        "vbr-candidate",
        candidate_rate_control,
        true,
    )?;
    let pulse_end = (PULSE_SEQUENCE + PULSE_FRAMES - 1) as usize;
    let control_pulse = &control_frames[pulse_end];
    let candidate_pulse = &candidate_frames[pulse_end];
    let control_tail = &control_frames[control_frames.len() - SUMMARY_FRAMES..];
    let candidate_tail = &candidate_frames[candidate_frames.len() - SUMMARY_FRAMES..];
    let control_persistent_error =
        median(control_tail.iter().map(|frame| frame.reconstruction_error));
    let candidate_persistent_error = median(
        candidate_tail
            .iter()
            .map(|frame| frame.reconstruction_error),
    );
    let production_control_tail =
        &production_control_frames[production_control_frames.len() - SUMMARY_FRAMES..];
    let production_control_persistent_error = median(
        production_control_tail
            .iter()
            .map(|frame| frame.reconstruction_error),
    );
    let control_total_bytes = control_frames
        .iter()
        .map(|frame| frame.encoded_bytes as u64)
        .sum::<u64>();
    let candidate_total_bytes = candidate_frames
        .iter()
        .map(|frame| frame.encoded_bytes as u64)
        .sum::<u64>();
    let production_control_total_bytes = production_control_frames
        .iter()
        .map(|frame| frame.encoded_bytes as u64)
        .sum::<u64>();
    let wire_budget_bytes =
        candidate_frames.len() as u64 * u64::from(BASELINE_BITRATE_BPS) / u64::from(FPS) / 8;
    let evidence = RefinementEvidence {
        control_pulse_error: control_pulse.reconstruction_error,
        candidate_pulse_error: candidate_pulse.reconstruction_error,
        production_persistent_error: production_control_persistent_error,
        control_persistent_error,
        candidate_persistent_error,
        candidate_total_bytes,
        wire_budget_bytes,
    };
    let pulse_range = PULSE_SEQUENCE as usize..(PULSE_SEQUENCE + PULSE_FRAMES) as usize;
    let control_pulse_bytes = control_frames[pulse_range.clone()]
        .iter()
        .map(|frame| frame.encoded_bytes)
        .sum::<usize>();
    let pulse_bytes = candidate_frames[pulse_range]
        .iter()
        .map(|frame| frame.encoded_bytes)
        .sum::<usize>();
    let pulse_extra_bytes = pulse_bytes.saturating_sub(control_pulse_bytes);

    Ok(ProbeReport {
        schema_version: 2,
        codec: "hevc-main-nvenc",
        width: WIDTH,
        height: HEIGHT,
        fps: FPS,
        baseline_bitrate_bps: BASELINE_BITRATE_BPS,
        wire_budget_bytes,
        candidate_target_quality: CANDIDATE_TARGET_QUALITY,
        candidate_peak_bitrate_bps: BASELINE_BITRATE_BPS
            * u32::from(CANDIDATE_PEAK_BITRATE_MULTIPLIER),
        candidate_vbv_frames: CANDIDATE_VBV_FRAMES,
        pulse_qp_delta: PULSE_QP_DELTA,
        pulse_frames: PULSE_FRAMES,
        pulse_kind: "three-frame-full-frame-delta-qp-p",
        baseline_prefix_exact: control_frames[..PULSE_SEQUENCE as usize]
            .iter()
            .zip(&candidate_frames[..PULSE_SEQUENCE as usize])
            .all(|(control, candidate)| {
                control.encoded_bytes == candidate.encoded_bytes
                    && control.reconstruction_error == candidate.reconstruction_error
            }),
        production_control_persistent_error,
        vbr_control_persistent_error: control_persistent_error,
        vbr_candidate_persistent_error: candidate_persistent_error,
        control_pulse_error: control_pulse.reconstruction_error,
        candidate_pulse_error: candidate_pulse.reconstruction_error,
        control_persistent_error,
        candidate_persistent_error,
        pulse_error_reduction_percent: reduction_percent(
            control_pulse.reconstruction_error,
            candidate_pulse.reconstruction_error,
        ),
        persistent_error_reduction_percent: reduction_percent(
            control_persistent_error,
            candidate_persistent_error,
        ),
        control_pulse_bytes,
        pulse_bytes,
        pulse_extra_bytes,
        pulse_wire_time_at_cap_ms: pulse_bytes as f64 * 8_000.0 / BASELINE_BITRATE_BPS as f64,
        pulse_extra_wire_time_at_cap_ms: pulse_extra_bytes as f64 * 8_000.0
            / BASELINE_BITRATE_BPS as f64,
        control_total_bytes,
        candidate_total_bytes,
        production_control_total_bytes,
        candidate_within_wire_budget: candidate_total_bytes <= wire_budget_bytes,
        persistent_gain: refinement_gain_is_persistent(evidence),
        production_control_frames,
        control_frames,
        candidate_frames,
    })
}

fn main() -> Result<()> {
    let report = run_probe()?;
    let json = serde_json::to_string_pretty(&report)?;
    println!("{json}");
    if let Some(output_path) = output_path_from_args()? {
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create artifact directory {}", parent.display()))?;
        }
        std::fs::write(&output_path, format!("{json}\n"))
            .with_context(|| format!("write artifact {}", output_path.display()))?;
    }
    ensure!(
        report.persistent_gain,
        "bounded refinement burst did not produce a material persistent reconstruction gain"
    );
    Ok(())
}

fn output_path_from_args() -> Result<Option<PathBuf>> {
    let mut args = std::env::args_os().skip(1);
    let Some(flag) = args.next() else {
        return Ok(None);
    };
    ensure!(flag == "--output", "expected --output <path>");
    let path = args.next().context("--output requires a path")?;
    ensure!(args.next().is_none(), "unexpected extra probe arguments");
    Ok(Some(path.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_a_gain_that_survives_after_baseline_rate_is_restored() {
        assert!(refinement_gain_is_persistent(RefinementEvidence {
            control_pulse_error: 10_000,
            candidate_pulse_error: 7_000,
            production_persistent_error: 8_000,
            control_persistent_error: 9_000,
            candidate_persistent_error: 7_200,
            candidate_total_bytes: 900_000,
            wire_budget_bytes: 1_000_000,
        }));
        assert!(!refinement_gain_is_persistent(RefinementEvidence {
            control_pulse_error: 10_000,
            candidate_pulse_error: 7_000,
            production_persistent_error: 8_000,
            control_persistent_error: 6_500,
            candidate_persistent_error: 7_200,
            candidate_total_bytes: 900_000,
            wire_budget_bytes: 1_000_000,
        }));
        assert!(!refinement_gain_is_persistent(RefinementEvidence {
            control_pulse_error: 10_000,
            candidate_pulse_error: 7_000,
            production_persistent_error: 8_000,
            control_persistent_error: 9_000,
            candidate_persistent_error: 7_200,
            candidate_total_bytes: 1_000_001,
            wire_budget_bytes: 1_000_000,
        }));
        assert!(!refinement_gain_is_persistent(RefinementEvidence {
            control_pulse_error: 10_000,
            candidate_pulse_error: 7_000,
            production_persistent_error: 7_000,
            control_persistent_error: 9_000,
            candidate_persistent_error: 7_200,
            candidate_total_bytes: 900_000,
            wire_budget_bytes: 1_000_000,
        }));
    }
}
