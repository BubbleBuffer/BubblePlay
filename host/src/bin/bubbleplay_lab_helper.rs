//! Demand-started LocalSystem service for BubblePlay stream-lab campaigns.
//!
//! The helper intentionally exposes no generic process, path, or script
//! execution. A single client with the same Windows user SID can issue only
//! bounded, schema-validated JSON commands over a local named pipe. Its SID is
//! loaded from the installer-protected service config, never inferred from the
//! service token. Dropping the connection, ending the campaign, lease expiry,
//! or SCM stop removes run-owned state and drops the optional VDD keepalive.

#![windows_subsystem = "windows"]
#![allow(clippy::manual_is_multiple_of)] // inherited from the shared VDD module

use std::ffi::{c_void, OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use windows::core::GUID;
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult, ServiceStatusHandle,
};
use windows_service::{define_windows_service, service_dispatcher};

#[cfg(test)]
#[allow(dead_code)]
#[path = "../benchmark.rs"]
mod host_benchmark;
#[path = "../vdd.rs"]
mod vdd;

const PIPE_NAME: &str = r"\\.\pipe\BubblePlay.StreamLab.v1";
const MAX_COMMAND_BYTES: usize = 64 * 1024;
const MAX_LEASE_SECONDS: u64 = 10 * 60;
const PROFILE_FILE_NAME: &str = "benchmark-v1.json";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const SERVICE_NAME: &str = "BubblePlayLabHelper";
const CLIENT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const SERVICE_CONFIG_FILE_NAME: &str = "lab-service.json";
const LAB_HELPER_LOG_NAME: &str = "lab-helper.combined.log";
const MAX_LAB_HELPER_LOG_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessMode {
    ScmService,
    Console,
}

fn process_mode(arguments: &[OsString]) -> ProcessMode {
    if arguments.iter().any(|argument| argument == "--console") {
        ProcessMode::Console
    } else {
        ProcessMode::ScmService
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LabServiceConfig {
    schema_version: u32,
    owner_sid: String,
}

fn client_is_authorized(configured_owner_sid: &str, actual_client_sid: &str) -> bool {
    configured_owner_sid.eq_ignore_ascii_case(actual_client_sid)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceExitCause {
    End,
    Disconnect,
    Expiry,
    ScmStop,
}

fn service_exit_required(cause: ServiceExitCause) -> bool {
    matches!(
        cause,
        ServiceExitCause::End
            | ServiceExitCause::Disconnect
            | ServiceExitCause::Expiry
            | ServiceExitCause::ScmStop
    )
}

define_windows_service!(ffi_service_main, service_main);

#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "command", rename_all = "camelCase", deny_unknown_fields)]
enum LabCommand {
    Begin {
        #[serde(rename = "runId")]
        run_id: String,
        #[serde(rename = "leaseSeconds")]
        lease_seconds: u64,
        vdd: bool,
    },
    Apply {
        #[serde(rename = "runId")]
        run_id: String,
        #[serde(rename = "leaseSeconds")]
        lease_seconds: u64,
        #[serde(rename = "bandwidthMbps")]
        bandwidth_mbps: Option<u32>,
        profile: BenchmarkProfile,
    },
    Renew {
        #[serde(rename = "runId")]
        run_id: String,
        #[serde(rename = "leaseSeconds")]
        lease_seconds: u64,
    },
    Shape {
        #[serde(rename = "runId")]
        run_id: String,
        #[serde(rename = "leaseSeconds")]
        lease_seconds: u64,
        #[serde(rename = "bandwidthMbps")]
        bandwidth_mbps: Option<u32>,
    },
    Clear {
        #[serde(rename = "runId")]
        run_id: String,
    },
    End {
        #[serde(rename = "runId")]
        run_id: String,
    },
}

impl LabCommand {
    fn run_id(&self) -> &str {
        match self {
            Self::Begin { run_id, .. }
            | Self::Apply { run_id, .. }
            | Self::Renew { run_id, .. }
            | Self::Shape { run_id, .. }
            | Self::Clear { run_id }
            | Self::End { run_id } => run_id,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BenchmarkProfile {
    schema_version: u32,
    #[serde(default)]
    implementation_variant: ImplementationVariant,
    #[serde(default)]
    experiment_id: Option<String>,
    transport: Transport,
    codec: String,
    #[serde(default)]
    stream_mode: StreamMode,
    adaptive: bool,
    width: u16,
    height: u16,
    fps: u16,
    bitrate_bps: u32,
    #[serde(default = "default_gop_seconds")]
    gop_seconds: u8,
    #[serde(default)]
    wss_topology: Option<WssTopology>,
    #[serde(default)]
    hybrid_mode: HybridMode,
    #[serde(default)]
    hybrid_cache_mode: HybridCacheMode,
    encoder: BenchmarkEncoder,
}

const fn default_gop_seconds() -> u8 {
    2
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
enum StreamMode {
    #[default]
    Work,
    Game,
}

impl StreamMode {
    const fn max_fps(self) -> u16 {
        match self {
            Self::Work => 30,
            Self::Game => 60,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BenchmarkEncoder {
    preset: Preset,
    tuning: Tuning,
    multipass: Multipass,
    aq_strength: u8,
    #[serde(default)]
    temporal_aq: bool,
    #[serde(default)]
    aq_mode: Option<AqMode>,
    vbv_frames: u8,
    #[serde(default)]
    lookahead_depth: u8,
    #[serde(default)]
    weighted_prediction: bool,
    #[serde(default)]
    dirty_qp_map: bool,
    #[serde(default)]
    main10: bool,
    #[serde(default)]
    roi_mode: RoiMode,
    #[serde(default)]
    resource_mode: ResourceMode,
    #[serde(default)]
    async_depth: u8,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
enum ImplementationVariant {
    #[default]
    Production,
    Candidate,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum AqMode {
    None,
    Spatial,
    Temporal,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Default)]
#[serde(rename_all = "kebab-case")]
enum RoiMode {
    #[default]
    Off,
    Damage,
    TextEdge,
    Feedback,
    FeedbackHeadroom,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Default)]
#[serde(rename_all = "kebab-case")]
enum ResourceMode {
    #[default]
    PerFrame,
    Persistent,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Default)]
#[serde(rename_all = "kebab-case")]
enum HybridMode {
    #[default]
    Off,
    PngStaticRoi,
    PngQuiescentDetail,
    HevcQuiescentTiles,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Default)]
#[serde(rename_all = "kebab-case")]
enum HybridCacheMode {
    DdaGated,
    #[default]
    SuppressReactivate,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WssTopology {
    lane_count: u8,
    data_shards: u8,
    parity_shards: u8,
}

impl WssTopology {
    const fn is_valid(self) -> bool {
        self.lane_count >= 1
            && self.lane_count <= 16
            && self.data_shards >= 1
            && self.parity_shards <= 2
            && self.lane_count == self.data_shards + self.parity_shards
    }

    const fn is_production_control(self) -> bool {
        self.lane_count == 16 && self.data_shards == 15 && self.parity_shards == 1
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Transport {
    Wss,
    Webrtc,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Preset {
    P1,
    P2,
    P3,
    P4,
    P5,
    P6,
    P7,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
enum Tuning {
    UltraLowLatency,
    LowLatency,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Multipass {
    Disabled,
    Quarter,
    Full,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredProfile {
    schema_version: u32,
    #[serde(default)]
    implementation_variant: ImplementationVariant,
    #[serde(default)]
    experiment_id: Option<String>,
    run_id: String,
    expires_at_ms: u64,
    #[serde(default)]
    apply_revision: u64,
    transport: Transport,
    codec: String,
    #[serde(default)]
    stream_mode: StreamMode,
    adaptive: bool,
    width: u16,
    height: u16,
    fps: u16,
    bitrate_bps: u32,
    #[serde(default = "default_gop_seconds")]
    gop_seconds: u8,
    #[serde(default)]
    wss_topology: Option<WssTopology>,
    #[serde(default)]
    hybrid_mode: HybridMode,
    #[serde(default)]
    hybrid_cache_mode: HybridCacheMode,
    encoder: BenchmarkEncoder,
}

impl StoredProfile {
    fn new(run_id: &str, expires_at_unix_ms: u64, profile: &BenchmarkProfile) -> Self {
        let mut encoder = profile.encoder.clone();
        encoder.roi_mode = profile.effective_roi_mode();
        Self {
            schema_version: profile.schema_version,
            implementation_variant: profile.implementation_variant,
            experiment_id: profile.experiment_id.clone(),
            run_id: run_id.to_owned(),
            expires_at_ms: expires_at_unix_ms,
            apply_revision: 0,
            transport: profile.transport,
            codec: profile.codec.clone(),
            stream_mode: profile.stream_mode,
            adaptive: profile.adaptive,
            width: profile.width,
            height: profile.height,
            fps: profile.fps,
            bitrate_bps: profile.bitrate_bps,
            gop_seconds: profile.gop_seconds,
            wss_topology: profile.wss_topology,
            hybrid_mode: profile.hybrid_mode,
            hybrid_cache_mode: profile.hybrid_cache_mode,
            encoder,
        }
    }

    fn disabled(run_id: &str, expires_at_unix_ms: u64) -> Self {
        Self {
            schema_version: 1,
            implementation_variant: ImplementationVariant::Production,
            experiment_id: None,
            run_id: run_id.to_owned(),
            expires_at_ms: expires_at_unix_ms,
            apply_revision: 0,
            transport: Transport::Wss,
            codec: "hevc".to_owned(),
            stream_mode: StreamMode::Work,
            adaptive: true,
            width: 1920,
            height: 1080,
            fps: 30,
            bitrate_bps: 4_000_000,
            gop_seconds: 2,
            wss_topology: None,
            hybrid_mode: HybridMode::Off,
            hybrid_cache_mode: HybridCacheMode::SuppressReactivate,
            encoder: BenchmarkEncoder {
                preset: Preset::P3,
                tuning: Tuning::UltraLowLatency,
                multipass: Multipass::Disabled,
                aq_strength: 8,
                temporal_aq: false,
                aq_mode: None,
                vbv_frames: 1,
                lookahead_depth: 0,
                weighted_prediction: false,
                dirty_qp_map: false,
                main10: false,
                roi_mode: RoiMode::Off,
                resource_mode: ResourceMode::PerFrame,
                async_depth: 0,
            },
        }
    }
}

impl BenchmarkProfile {
    const fn effective_roi_mode(&self) -> RoiMode {
        match self.encoder.roi_mode {
            RoiMode::Off if self.encoder.dirty_qp_map => RoiMode::Damage,
            mode => mode,
        }
    }

    fn uses_pipeline_experiment(&self) -> bool {
        self.effective_roi_mode() != RoiMode::Off
            || self.hybrid_mode != HybridMode::Off
            || self.encoder.resource_mode != ResourceMode::PerFrame
            || self.encoder.async_depth != 0
    }

    fn has_valid_pipeline_experiment(&self) -> bool {
        if !self.uses_pipeline_experiment() {
            return true;
        }
        let roi_active = self.effective_roi_mode() != RoiMode::Off;
        let hybrid_active = self.hybrid_mode != HybridMode::Off;
        let resource_active =
            self.encoder.resource_mode != ResourceMode::PerFrame || self.encoder.async_depth != 0;
        let active_axes =
            u8::from(roi_active) + u8::from(hybrid_active) + u8::from(resource_active);

        self.schema_version == 2
            && self.implementation_variant == ImplementationVariant::Candidate
            && self.transport == Transport::Wss
            && self.stream_mode == StreamMode::Work
            && self.width == 1920
            && self.height == 1080
            && self.fps == 30
            && self.bitrate_bps == 6_000_000
            && self
                .wss_topology
                .is_some_and(WssTopology::is_production_control)
            && !self.encoder.main10
            && active_axes == 1
            && self.encoder.async_depth <= 1
            && (self.encoder.async_depth == 0
                || self.encoder.resource_mode == ResourceMode::Persistent)
            && (!self.encoder.dirty_qp_map
                || matches!(self.encoder.roi_mode, RoiMode::Off | RoiMode::Damage))
            && (self.hybrid_cache_mode == HybridCacheMode::SuppressReactivate
                || self.hybrid_mode == HybridMode::PngQuiescentDetail)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Response<'a> {
    ok: bool,
    event: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at_unix_ms: Option<u64>,
}

fn validate_command(command: &LabCommand) -> Result<()> {
    validate_run_id(command.run_id())?;
    match command {
        LabCommand::Begin { lease_seconds, .. } | LabCommand::Renew { lease_seconds, .. } => {
            validate_lease(*lease_seconds)
        }
        LabCommand::Shape {
            lease_seconds,
            bandwidth_mbps,
            ..
        } => {
            validate_lease(*lease_seconds)?;
            validate_bandwidth(*bandwidth_mbps)
        }
        LabCommand::Apply {
            lease_seconds,
            bandwidth_mbps,
            profile,
            ..
        } => {
            validate_lease(*lease_seconds)?;
            validate_bandwidth(*bandwidth_mbps)?;
            if !matches!(profile.schema_version, 1 | 2) {
                bail!("profile schemaVersion must be 1 or 2");
            }
            if profile.codec != "hevc" || profile.adaptive {
                bail!("benchmarks require HEVC with adaptive control disabled");
            }
            if !(320..=3840).contains(&profile.width)
                || !(180..=2160).contains(&profile.height)
                || !(1..=profile.stream_mode.max_fps()).contains(&profile.fps)
            {
                bail!("profile dimensions or FPS are outside stream-mode bounds");
            }
            if !matches!(profile.gop_seconds, 1 | 2 | 4) {
                bail!("profile gopSeconds must be 1, 2, or 4");
            }
            if !(1..=15).contains(&profile.encoder.aq_strength) {
                bail!("profile aqStrength must be between 1 and 15");
            }
            if !(1..=4).contains(&profile.encoder.vbv_frames) {
                bail!("profile vbvFrames must be between 1 and 4");
            }
            if profile.schema_version == 1 {
                if profile.implementation_variant != ImplementationVariant::Production
                    || profile.experiment_id.is_some()
                    || profile.encoder.aq_mode.is_some()
                    || profile.encoder.lookahead_depth != 0
                    || profile.encoder.weighted_prediction
                    || profile.encoder.dirty_qp_map
                    || profile.encoder.main10
                    || profile.encoder.roi_mode != RoiMode::Off
                    || profile.encoder.resource_mode != ResourceMode::PerFrame
                    || profile.encoder.async_depth != 0
                    || profile.hybrid_mode != HybridMode::Off
                    || profile.hybrid_cache_mode != HybridCacheMode::SuppressReactivate
                    || profile.wss_topology.is_some()
                    || !(500_000..=40_000_000).contains(&profile.bitrate_bps)
                {
                    bail!("v1 profile contains v2 controls or an invalid bitrate");
                }
            } else if !profile
                .experiment_id
                .as_deref()
                .is_some_and(valid_experiment_id)
                || profile.encoder.aq_mode.is_none()
                || profile.encoder.temporal_aq
                || !(2_000_000..=12_000_000).contains(&profile.bitrate_bps)
                || profile.encoder.lookahead_depth
                    > match profile.stream_mode {
                        StreamMode::Work => 2,
                        StreamMode::Game => 1,
                    }
                || profile.wss_topology.is_some_and(|topology| {
                    profile.transport != Transport::Wss || !topology.is_valid()
                })
                || (profile.encoder.main10
                    && (profile.implementation_variant != ImplementationVariant::Candidate
                        || profile.transport != Transport::Wss
                        || profile.stream_mode != StreamMode::Work
                        || profile.width != 1920
                        || profile.height != 1080
                        || profile.fps != 30))
                || !profile.has_valid_pipeline_experiment()
            {
                bail!("v2 profile contains an invalid experiment or encoder control");
            }
            Ok(())
        }
        LabCommand::Clear { .. } | LabCommand::End { .. } => Ok(()),
    }
}

fn valid_experiment_id(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn validate_bandwidth(bandwidth_mbps: Option<u32>) -> Result<()> {
    if matches!(bandwidth_mbps, None | Some(2 | 4 | 6 | 8 | 12)) {
        Ok(())
    } else {
        bail!("bandwidthMbps must be null, 2, 4, 6, 8, or 12")
    }
}

fn validate_lease(seconds: u64) -> Result<()> {
    if (1..=MAX_LEASE_SECONDS).contains(&seconds) {
        Ok(())
    } else {
        bail!("leaseSeconds must be between 1 and {MAX_LEASE_SECONDS}")
    }
}

fn validate_run_id(run_id: &str) -> Result<()> {
    let bytes = run_id.as_bytes();
    let valid = bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        });
    if valid {
        Ok(())
    } else {
        bail!("runId must be a canonical UUID")
    }
}

fn qos_policy_name(run_id: &str) -> String {
    format!("BubblePlayLab-{}", run_id.replace('-', ""))
}

struct ActiveSession {
    run_id: String,
    expires_at: SystemTime,
    profile_path: PathBuf,
    powershell: PathBuf,
    qos_active: bool,
    watchdog_started: bool,
    vdd: Option<vdd::VddKeepalive>,
}

impl ActiveSession {
    fn clear(&mut self) -> Result<()> {
        let mut failure = None;
        if self.qos_active {
            if let Err(error) = remove_qos(&self.powershell, &qos_policy_name(&self.run_id)) {
                failure = Some(error.context("remove stream-lab QoS policy"));
            }
            self.qos_active = false;
        }
        if let Err(error) = remove_profile_if_owned(&self.profile_path, &self.run_id) {
            failure.get_or_insert_with(|| error.context("remove stream-lab profile"));
        }
        self.watchdog_started = false;
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl Drop for ActiveSession {
    fn drop(&mut self) {
        if let Err(error) = self.clear() {
            warn!(?error, run_id = %self.run_id, "stream-lab cleanup incomplete");
        }
        self.vdd.take();
    }
}

#[cfg(test)]
fn write_profile_atomic(path: &Path, stored: &StoredProfile) -> Result<()> {
    let parent = path.parent().context("benchmark profile has no parent")?;
    fs::create_dir_all(parent).context("create BubblePlay ProgramData directory")?;
    let _lock = acquire_profile_lock(path)?;
    write_profile_atomic_unlocked(path, stored)
}

fn publish_applied_profile(
    path: &Path,
    run_id: &str,
    expires_at_unix_ms: u64,
    profile: &BenchmarkProfile,
) -> Result<u64> {
    let parent = path.parent().context("benchmark profile has no parent")?;
    fs::create_dir_all(parent).context("create BubblePlay ProgramData directory")?;
    let _lock = acquire_profile_lock(path)?;
    let revision = owned_apply_revision_unlocked(path, run_id)?
        .checked_add(1)
        .context("benchmark applyRevision exhausted")?;
    let mut stored = StoredProfile::new(run_id, expires_at_unix_ms, profile);
    stored.apply_revision = revision;
    write_profile_atomic_unlocked(path, &stored)?;
    Ok(revision)
}

fn publish_disabled_profile(path: &Path, run_id: &str, expires_at_unix_ms: u64) -> Result<u64> {
    let parent = path.parent().context("benchmark profile has no parent")?;
    fs::create_dir_all(parent).context("create BubblePlay ProgramData directory")?;
    let _lock = acquire_profile_lock(path)?;
    let revision = owned_apply_revision_unlocked(path, run_id)?;
    let mut stored = StoredProfile::disabled(run_id, expires_at_unix_ms);
    stored.apply_revision = revision;
    write_profile_atomic_unlocked(path, &stored)?;
    Ok(revision)
}

fn owned_apply_revision_unlocked(path: &Path, run_id: &str) -> Result<u64> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error).context("read benchmark profile revision"),
    };
    let stored: StoredProfile =
        serde_json::from_slice(&bytes).context("parse benchmark profile revision")?;
    if stored.run_id != run_id {
        bail!("runId does not own the benchmark profile");
    }
    Ok(stored.apply_revision)
}

fn write_profile_atomic_unlocked(path: &Path, stored: &StoredProfile) -> Result<()> {
    let temp = profile_temp_path(path, &stored.run_id)?;
    let bytes = serde_json::to_vec_pretty(stored)?;
    fs::write(&temp, bytes).context("write temporary benchmark profile")?;
    let result = move_file_replace(&temp, path).context("publish benchmark profile atomically");
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn profile_temp_path(path: &Path, run_id: &str) -> Result<PathBuf> {
    let parent = path.parent().context("benchmark profile has no parent")?;
    let file_name = path
        .file_name()
        .context("benchmark profile has no file name")?;
    let mut temp_name = OsString::from(".");
    temp_name.push(file_name);
    temp_name.push(".");
    temp_name.push(run_id);
    temp_name.push(".tmp");
    Ok(parent.join(temp_name))
}

fn profile_lock_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(".lock");
    PathBuf::from(value)
}

fn acquire_profile_lock(path: &Path) -> Result<fs::File> {
    let lock_path = profile_lock_path(path);
    let started = Instant::now();
    loop {
        match fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .share_mode(0)
            .open(&lock_path)
        {
            Ok(file) => return Ok(file),
            Err(error)
                if matches!(error.raw_os_error(), Some(5 | 32))
                    && started.elapsed() < Duration::from_secs(2) =>
            {
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("acquire benchmark profile lock '{}'", lock_path.display())
                })
            }
        }
    }
}

fn move_file_replace(source: &Path, destination: &Path) -> io::Result<()> {
    let source = wide_path(source);
    let destination = wide_path(destination);
    let started = Instant::now();
    loop {
        if unsafe {
            MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } != 0
        {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        let transient = matches!(error.raw_os_error(), Some(5 | 32));
        if !transient || started.elapsed() >= Duration::from_secs(2) {
            return Err(error);
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn remove_profile_if_owned(path: &Path, run_id: &str) -> Result<()> {
    let _lock = acquire_profile_lock(path)?;
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("read benchmark profile for cleanup"),
    };
    let stored: StoredProfile = serde_json::from_slice(&bytes)
        .context("refusing to remove an invalid benchmark profile")?;
    if stored.run_id == run_id {
        fs::remove_file(path).context("remove benchmark profile")?;
    }
    Ok(())
}

fn renew_profile_expiry(path: &Path, run_id: &str, expires_at_ms: u64) -> Result<bool> {
    let _lock = acquire_profile_lock(path)?;
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("read benchmark profile for renewal"),
    };
    let mut stored: StoredProfile =
        serde_json::from_slice(&bytes).context("parse benchmark profile for renewal")?;
    if stored.run_id != run_id {
        bail!("runId does not own the benchmark profile");
    }
    stored.expires_at_ms = expires_at_ms;
    write_profile_atomic_unlocked(path, &stored)?;
    Ok(true)
}

fn unix_ms(time: SystemTime) -> Result<u64> {
    Ok(time.duration_since(UNIX_EPOCH)?.as_millis() as u64)
}

fn configured_powershell(
    powershell: &Path,
    script: &'static str,
    environment: &[(&'static str, &OsStr)],
) -> Command {
    let mut command = Command::new(powershell);
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            script,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (name, value) in environment {
        command.env(name, value);
    }
    command
}

fn run_powershell(
    powershell: &Path,
    script: &'static str,
    environment: &[(&'static str, &OsStr)],
) -> Result<()> {
    let status = configured_powershell(powershell, script, environment)
        .status()
        .context("launch fixed stream-lab PowerShell command")?;
    if status.success() {
        Ok(())
    } else {
        bail!("stream-lab PowerShell command failed with {status}")
    }
}

const APPLY_QOS_SCRIPT: &str = r#"$ErrorActionPreference='Stop'; $n=$env:BUBBLEPLAY_LAB_POLICY; $b=[uint64]$env:BUBBLEPLAY_LAB_BITS; Get-NetQosPolicy -Name $n -PolicyStore ActiveStore -ErrorAction SilentlyContinue | Remove-NetQosPolicy -PolicyStore ActiveStore -Confirm:$false; New-NetQosPolicy -Name $n -AppPathNameMatchCondition 'bubbleplay-host.exe' -ThrottleRateActionBitsPerSecond $b -PolicyStore ActiveStore | Out-Null"#;
const REMOVE_QOS_SCRIPT: &str = r#"$ErrorActionPreference='Stop'; Get-NetQosPolicy -Name $env:BUBBLEPLAY_LAB_POLICY -PolicyStore ActiveStore -ErrorAction SilentlyContinue | Remove-NetQosPolicy -PolicyStore ActiveStore -Confirm:$false"#;
const WATCHDOG_SCRIPT: &str = r#"$ErrorActionPreference='SilentlyContinue'; $p=$env:BUBBLEPLAY_LAB_PROFILE; $r=$env:BUBBLEPLAY_LAB_RUN; $n=$env:BUBBLEPLAY_LAB_POLICY; $poll=[int]$env:BUBBLEPLAY_LAB_POLL_MS; if($poll -lt 50){$poll=1000}; while($true){if(-not (Test-Path -LiteralPath $p)){exit 0}; try{$j=Get-Content -LiteralPath $p -Raw|ConvertFrom-Json}catch{Start-Sleep -Milliseconds $poll; continue}; if($j.runId -ne $r){exit 0}; $candidateExpiry=[int64]$j.expiresAtMs; $now=[DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds(); $remaining=$candidateExpiry-$now; if($remaining -le 0){$lock=$null; while($null -eq $lock){try{$lock=[IO.File]::Open($p+'.lock',[IO.FileMode]::OpenOrCreate,[IO.FileAccess]::ReadWrite,[IO.FileShare]::None)}catch{Start-Sleep -Milliseconds 50}}; try{try{$latest=Get-Content -LiteralPath $p -Raw|ConvertFrom-Json}catch{continue}; $latestNow=[DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds(); if($latest.runId -ne $r){exit 0}; if([int64]$latest.expiresAtMs -ne $candidateExpiry){continue}; if([int64]$latest.expiresAtMs -gt $latestNow){continue}; Remove-Item -LiteralPath $p -Force; Get-NetQosPolicy -Name $n -PolicyStore ActiveStore -ErrorAction SilentlyContinue|Remove-NetQosPolicy -PolicyStore ActiveStore -Confirm:$false; exit 0}finally{$lock.Dispose()}}; Start-Sleep -Milliseconds ([Math]::Min($poll,[Math]::Max(50,$remaining))) }"#;

fn apply_qos(powershell: &Path, policy_name: &str, bandwidth_mbps: u32) -> Result<()> {
    let bits = u64::from(bandwidth_mbps) * 1_000_000;
    let bits = bits.to_string();
    run_powershell(
        powershell,
        APPLY_QOS_SCRIPT,
        &[
            ("BUBBLEPLAY_LAB_POLICY", OsStr::new(policy_name)),
            ("BUBBLEPLAY_LAB_BITS", OsStr::new(&bits)),
        ],
    )
}

fn remove_qos(powershell: &Path, policy_name: &str) -> Result<()> {
    run_powershell(
        powershell,
        REMOVE_QOS_SCRIPT,
        &[("BUBBLEPLAY_LAB_POLICY", OsStr::new(policy_name))],
    )
}

fn spawn_expiry_watchdog(powershell: &Path, profile_path: &Path, run_id: &str) -> Result<()> {
    let policy_name = qos_policy_name(run_id);
    configured_powershell(
        powershell,
        WATCHDOG_SCRIPT,
        &[
            ("BUBBLEPLAY_LAB_PROFILE", profile_path.as_os_str()),
            ("BUBBLEPLAY_LAB_RUN", OsStr::new(run_id)),
            ("BUBBLEPLAY_LAB_POLICY", OsStr::new(&policy_name)),
            ("BUBBLEPLAY_LAB_POLL_MS", OsStr::new("1000")),
        ],
    )
    .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
    .spawn()
    .context("spawn independent stream-lab expiry watchdog")?;
    Ok(())
}

fn handle_command(
    command: LabCommand,
    session: &mut Option<ActiveSession>,
    program_data: &Path,
    powershell: &Path,
) -> Result<(&'static str, Option<u64>, bool)> {
    validate_command(&command)?;
    match command {
        LabCommand::Begin {
            run_id,
            lease_seconds,
            vdd: wants_vdd,
        } => {
            if session.is_some() {
                bail!("a stream-lab campaign is already active");
            }
            let expires_at = SystemTime::now() + Duration::from_secs(lease_seconds);
            let keepalive = if wants_vdd {
                Some(vdd::VddKeepalive::start().context("start Parsec VDD keepalive")?)
            } else {
                None
            };
            *session = Some(ActiveSession {
                run_id,
                expires_at,
                profile_path: program_data.join(PROFILE_FILE_NAME),
                powershell: powershell.to_owned(),
                qos_active: false,
                watchdog_started: false,
                vdd: keepalive,
            });
            Ok(("begun", Some(unix_ms(expires_at)?), false))
        }
        LabCommand::Apply {
            run_id,
            lease_seconds,
            bandwidth_mbps,
            profile,
        } => {
            let active = matching_session(session, &run_id)?;
            let expires_at = SystemTime::now() + Duration::from_secs(lease_seconds);
            let expires_at_unix_ms = unix_ms(expires_at)?;
            publish_applied_profile(&active.profile_path, &run_id, expires_at_unix_ms, &profile)?;
            if let Err(error) = secure_benchmark_profile(&active.powershell, &active.profile_path) {
                let _ = remove_profile_if_owned(&active.profile_path, &run_id);
                return Err(error);
            }
            if !active.watchdog_started {
                spawn_expiry_watchdog(&active.powershell, &active.profile_path, &run_id)?;
                active.watchdog_started = true;
            }
            match bandwidth_mbps {
                Some(limit) => {
                    apply_qos(&active.powershell, &qos_policy_name(&run_id), limit)?;
                    active.qos_active = true;
                }
                None if active.qos_active => {
                    remove_qos(&active.powershell, &qos_policy_name(&run_id))?;
                    active.qos_active = false;
                }
                None => {}
            }
            active.expires_at = expires_at;
            Ok(("applied", Some(expires_at_unix_ms), false))
        }
        LabCommand::Renew {
            run_id,
            lease_seconds,
        } => {
            let active = matching_session(session, &run_id)?;
            let expires_at = SystemTime::now() + Duration::from_secs(lease_seconds);
            let expires_at_unix_ms = unix_ms(expires_at)?;
            let profile_renewed =
                renew_profile_expiry(&active.profile_path, &run_id, expires_at_unix_ms)?;
            if active.watchdog_started && !profile_renewed {
                bail!("active benchmark profile disappeared before renewal");
            }
            if profile_renewed {
                if let Err(error) =
                    secure_benchmark_profile(&active.powershell, &active.profile_path)
                {
                    let _ = active.clear();
                    return Err(error);
                }
            }
            active.expires_at = expires_at;
            Ok(("renewed", Some(expires_at_unix_ms), false))
        }
        LabCommand::Shape {
            run_id,
            lease_seconds,
            bandwidth_mbps,
        } => {
            let active = matching_session(session, &run_id)?;
            let expires_at = SystemTime::now() + Duration::from_secs(lease_seconds);
            let expires_at_unix_ms = unix_ms(expires_at)?;

            // Keep the watchdog lease in the same protected, run-owned file,
            // but make it ineligible for host benchmark overrides. The host
            // therefore returns to its production adaptive controller on its
            // next poll without losing independent crash/expiry cleanup.
            publish_disabled_profile(&active.profile_path, &run_id, expires_at_unix_ms)?;
            if let Err(error) = secure_benchmark_profile(&active.powershell, &active.profile_path) {
                let _ = remove_profile_if_owned(&active.profile_path, &run_id);
                return Err(error);
            }
            if !active.watchdog_started {
                spawn_expiry_watchdog(&active.powershell, &active.profile_path, &run_id)?;
                active.watchdog_started = true;
            }

            match bandwidth_mbps {
                Some(limit) => {
                    apply_qos(&active.powershell, &qos_policy_name(&run_id), limit)?;
                    active.qos_active = true;
                }
                None if active.qos_active => {
                    remove_qos(&active.powershell, &qos_policy_name(&run_id))?;
                    active.qos_active = false;
                }
                None => {}
            }
            active.expires_at = expires_at;
            Ok(("shaped", Some(expires_at_unix_ms), false))
        }
        LabCommand::Clear { run_id } => {
            matching_session(session, &run_id)?.clear()?;
            Ok(("cleared", None, false))
        }
        LabCommand::End { run_id } => {
            matching_session(session, &run_id)?;
            session.take();
            Ok(("ended", None, true))
        }
    }
}

fn matching_session<'a>(
    session: &'a mut Option<ActiveSession>,
    run_id: &str,
) -> Result<&'a mut ActiveSession> {
    let active = session
        .as_mut()
        .context("no stream-lab campaign is active")?;
    if active.run_id != run_id {
        bail!("runId does not own the active campaign");
    }
    Ok(active)
}

const VALIDATE_DATA_ACL_SCRIPT: &str = r#"$ErrorActionPreference='Stop'; $p=$env:BUBBLEPLAY_LAB_DATA; $o=$env:BUBBLEPLAY_LAB_OWNER; if(-not (Test-Path -LiteralPath $p -PathType Container)){exit 10}; $i=Get-Item -Force -LiteralPath $p; if(($i.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0){exit 11}; $a=Get-Acl -LiteralPath $p; if(-not $a.AreAccessRulesProtected){exit 12}; $r=@($a.GetAccessRules($true,$false,[Security.Principal.SecurityIdentifier])); $e=@{'S-1-5-18'=[Security.AccessControl.FileSystemRights]::FullControl;'S-1-5-32-544'=[Security.AccessControl.FileSystemRights]::FullControl;'S-1-5-80-36460983-23004156-1113575813-106319344-2112134846'=[Security.AccessControl.FileSystemRights]::ReadAndExecute}; $e[$o]=[Security.AccessControl.FileSystemRights]::ReadAndExecute; if($r.Count -ne $e.Count){exit 13}; foreach($s in $e.Keys){$m=@($r|Where-Object{$_.IdentityReference.Value -eq $s}); $n=[Security.AccessControl.FileSystemAccessRule]::new([Security.Principal.SecurityIdentifier]::new($s),$e[$s],[Security.AccessControl.AccessControlType]::Allow).FileSystemRights; if($m.Count -ne 1 -or $m[0].AccessControlType -ne [Security.AccessControl.AccessControlType]::Allow -or [int64]$m[0].FileSystemRights -ne [int64]$n){exit 14}}"#;
const SET_PROFILE_ACL_SCRIPT: &str = r#"$ErrorActionPreference='Stop'; $p=$env:BUBBLEPLAY_LAB_PROFILE; $i=Get-Item -Force -LiteralPath $p; if(($i.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0){exit 20}; $e=@{'S-1-5-18'=[Security.AccessControl.FileSystemRights]::FullControl;'S-1-5-32-544'=[Security.AccessControl.FileSystemRights]::FullControl;'S-1-5-80-36460983-23004156-1113575813-106319344-2112134846'=[Security.AccessControl.FileSystemRights]::Read}; $a=[Security.AccessControl.FileSecurity]::new(); $a.SetAccessRuleProtection($true,$false); foreach($s in $e.Keys){$a.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new([Security.Principal.SecurityIdentifier]::new($s),$e[$s],[Security.AccessControl.AccessControlType]::Allow))}; Set-Acl -LiteralPath $p -AclObject $a; $b=Get-Acl -LiteralPath $p; if(-not $b.AreAccessRulesProtected){exit 21}; $r=@($b.GetAccessRules($true,$false,[Security.Principal.SecurityIdentifier])); if($r.Count -ne $e.Count){exit 22}; foreach($s in $e.Keys){$m=@($r|Where-Object{$_.IdentityReference.Value -eq $s}); $n=[Security.AccessControl.FileSystemAccessRule]::new([Security.Principal.SecurityIdentifier]::new($s),$e[$s],[Security.AccessControl.AccessControlType]::Allow).FileSystemRights; if($m.Count -ne 1 -or $m[0].AccessControlType -ne [Security.AccessControl.AccessControlType]::Allow -or [int64]$m[0].FileSystemRights -ne [int64]$n){exit 23}}"#;
const VALIDATE_SERVICE_CONFIG_ACL_SCRIPT: &str = r#"$ErrorActionPreference='Stop'; $p=$env:BUBBLEPLAY_LAB_CONFIG; $i=Get-Item -Force -LiteralPath $p; if(($i.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0){exit 30}; $a=Get-Acl -LiteralPath $p; if(-not $a.AreAccessRulesProtected){exit 31}; $r=@($a.GetAccessRules($true,$false,[Security.Principal.SecurityIdentifier])); $e=@{'S-1-5-18'=[Security.AccessControl.FileSystemRights]::FullControl;'S-1-5-32-544'=[Security.AccessControl.FileSystemRights]::FullControl}; if($r.Count -ne $e.Count){exit 32}; foreach($s in $e.Keys){$m=@($r|Where-Object{$_.IdentityReference.Value -eq $s}); $n=[Security.AccessControl.FileSystemAccessRule]::new([Security.Principal.SecurityIdentifier]::new($s),$e[$s],[Security.AccessControl.AccessControlType]::Allow).FileSystemRights; if($m.Count -ne 1 -or $m[0].AccessControlType -ne [Security.AccessControl.AccessControlType]::Allow -or [int64]$m[0].FileSystemRights -ne [int64]$n){exit 33}}"#;

fn validate_program_data_directory(path: &Path, powershell: &Path, owner_sid: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path).with_context(|| {
        format!(
            "BubblePlay DataDir '{}' is unavailable; run the service installer/repair",
            path.display()
        )
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("BubblePlay DataDir is not a normal directory; run installer/repair");
    }
    run_powershell(
        powershell,
        VALIDATE_DATA_ACL_SCRIPT,
        &[
            ("BUBBLEPLAY_LAB_DATA", path.as_os_str()),
            ("BUBBLEPLAY_LAB_OWNER", OsStr::new(owner_sid)),
        ],
    )
    .context("BubblePlay DataDir ACL drifted; run the service installer/repair")
}

fn secure_benchmark_profile(powershell: &Path, path: &Path) -> Result<()> {
    run_powershell(
        powershell,
        SET_PROFILE_ACL_SCRIPT,
        &[("BUBBLEPLAY_LAB_PROFILE", path.as_os_str())],
    )
    .context("protect benchmark-v1.json for the transport service")
}

fn load_service_config(path: &Path, powershell: &Path) -> Result<LabServiceConfig> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("read protected service config '{}'", path.display()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 4 * 1024 {
        bail!("lab-service.json is not a bounded normal file");
    }
    run_powershell(
        powershell,
        VALIDATE_SERVICE_CONFIG_ACL_SCRIPT,
        &[("BUBBLEPLAY_LAB_CONFIG", path.as_os_str())],
    )
    .context("lab-service.json ACL is not protected")?;
    parse_service_config(&fs::read(path).context("read lab-service.json")?)
}

fn parse_service_config(bytes: &[u8]) -> Result<LabServiceConfig> {
    let config: LabServiceConfig =
        serde_json::from_slice(bytes).context("parse lab-service.json")?;
    if config.schema_version != 1 {
        bail!("lab-service.json schemaVersion must be 1");
    }
    let canonical =
        canonical_sid(&config.owner_sid).context("lab-service.json ownerSid is invalid")?;
    if !canonical.eq_ignore_ascii_case(&config.owner_sid) {
        bail!("lab-service.json ownerSid must be canonical");
    }
    Ok(config)
}

fn canonical_sid(value: &str) -> io::Result<String> {
    let value = wide_null(value);
    let mut sid = ptr::null_mut();
    if unsafe { ConvertStringSidToSidW(value.as_ptr(), &mut sid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _sid_owner = LocalMemory(sid);
    sid_to_string(sid)
}

fn sid_to_string(sid: *mut c_void) -> io::Result<String> {
    let mut text = ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _text_owner = LocalMemory(text.cast());
    let mut len = 0;
    unsafe {
        while *text.add(len) != 0 {
            len += 1;
        }
        Ok(String::from_utf16_lossy(std::slice::from_raw_parts(
            text, len,
        )))
    }
}

enum Incoming {
    Command(String),
    Disconnected,
    Error(String),
}

fn main() -> Result<()> {
    let arguments: Vec<OsString> = std::env::args_os().skip(1).collect();
    let mode = process_mode(&arguments);
    init_logging(mode);
    match mode {
        ProcessMode::ScmService => service_dispatcher::start(SERVICE_NAME, ffi_service_main)
            .context("start BubblePlayLabHelper service dispatcher"),
        ProcessMode::Console => run_console(),
    }
}

fn init_logging(mode: ProcessMode) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    if mode == ProcessMode::ScmService {
        if let Ok(file) = open_lab_helper_log() {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(Mutex::new(file))
                .try_init();
            return;
        }
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .try_init();
}

fn open_lab_helper_log() -> Result<File> {
    let directory = program_data_directory()?
        .join("BubblePlay")
        .join("logs")
        .join("system");
    let metadata = fs::symlink_metadata(&directory)
        .with_context(|| format!("inspect lab helper log directory '{}'", directory.display()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("lab helper log directory is not a normal directory");
    }
    let path = directory.join(LAB_HELPER_LOG_NAME);
    let previous = directory.join("lab-helper.combined.previous.log");
    if fs::metadata(&previous).is_ok_and(|value| value.len() > MAX_LAB_HELPER_LOG_BYTES) {
        fs::remove_file(&previous).context("remove oversized previous lab helper log")?;
    }
    if fs::metadata(&path).is_ok_and(|value| value.len() >= MAX_LAB_HELPER_LOG_BYTES) {
        if previous.exists() {
            fs::remove_file(&previous).context("remove previous lab helper log")?;
        }
        fs::rename(&path, &previous).context("rotate lab helper log")?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open lab helper log '{}'", path.display()))
}

fn service_main(_arguments: Vec<OsString>) {
    if let Err(error) = run_service() {
        tracing::error!(?error, "BubblePlayLabHelper service stopped with an error");
    }
}

fn run_service() -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    let handler_stop = stop.clone();
    let handler = move |control: ServiceControl| match control {
        ServiceControl::Stop | ServiceControl::Shutdown | ServiceControl::Preshutdown => {
            handler_stop.store(true, Ordering::Release);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status = service_control_handler::register(SERVICE_NAME, handler)
        .context("register BubblePlayLabHelper service control handler")?;
    report_service_status(
        &status,
        ServiceState::StartPending,
        ServiceControlAccept::empty(),
        Duration::from_secs(10),
        1,
        0,
    )?;
    let result = run_one_client(stop, Some(&status));
    let exit_code = u32::from(result.is_err());
    let _ = report_service_status(
        &status,
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        Duration::ZERO,
        0,
        exit_code,
    );
    result
}

fn run_console() -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    let console_stop = stop.clone();
    thread::spawn(move || {
        let mut line = String::new();
        let _ = io::stdin().read_line(&mut line);
        console_stop.store(true, Ordering::Release);
    });
    info!("BubblePlayLabHelper console mode; press Enter to stop");
    run_one_client(stop, None)
}

fn report_service_status(
    handle: &ServiceStatusHandle,
    state: ServiceState,
    controls: ServiceControlAccept,
    wait_hint: Duration,
    checkpoint: u32,
    service_error: u32,
) -> Result<()> {
    handle
        .set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: controls,
            exit_code: if service_error == 0 {
                ServiceExitCode::Win32(0)
            } else {
                ServiceExitCode::ServiceSpecific(service_error)
            },
            checkpoint,
            wait_hint,
            process_id: None,
        })
        .context("report BubblePlayLabHelper service status")
}

fn run_one_client(stop: Arc<AtomicBool>, status: Option<&ServiceStatusHandle>) -> Result<()> {
    let program_data = program_data_directory()?.join("BubblePlay");
    let powershell = system_directory()?
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    let config = load_service_config(&program_data.join(SERVICE_CONFIG_FILE_NAME), &powershell)?;
    validate_program_data_directory(&program_data, &powershell, &config.owner_sid)?;
    let server = PipeServer::bind_for_sid(&config.owner_sid)?;
    if let Some(status) = status {
        report_service_status(
            status,
            ServiceState::Running,
            ServiceControlAccept::STOP
                | ServiceControlAccept::SHUTDOWN
                | ServiceControlAccept::PRESHUTDOWN,
            Duration::ZERO,
            0,
            0,
        )?;
    }
    info!(
        pipe = PIPE_NAME,
        owner_sid = %config.owner_sid,
        "stream-lab service waiting for its configured owner client"
    );
    let Some(pipe) = server.accept_for_sid_cancelable(
        &config.owner_sid,
        stop.clone(),
        CLIENT_CONNECT_TIMEOUT,
    )?
    else {
        if stop.load(Ordering::Acquire) {
            if let Some(status) = status {
                let _ = report_service_status(
                    status,
                    ServiceState::StopPending,
                    ServiceControlAccept::empty(),
                    Duration::from_secs(5),
                    1,
                    0,
                );
            }
            info!("stream-lab service stopped before a client connected");
        } else {
            info!("stream-lab service client connection window expired");
        }
        return Ok(());
    };

    let mut writer = pipe.try_clone()?;
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || pipe_reader(pipe, sender));
    info!("stream-lab pipe reader started");

    let mut session: Option<ActiveSession> = None;

    loop {
        if stop.load(Ordering::Acquire) {
            debug_assert!(service_exit_required(ServiceExitCause::ScmStop));
            if let Some(status) = status {
                let _ = report_service_status(
                    status,
                    ServiceState::StopPending,
                    ServiceControlAccept::empty(),
                    Duration::from_secs(5),
                    1,
                    0,
                );
            }
            break;
        }
        if session
            .as_ref()
            .is_some_and(|active| SystemTime::now() >= active.expires_at)
        {
            info!("stream-lab lease expired; cleaning up");
            session.take();
            debug_assert!(service_exit_required(ServiceExitCause::Expiry));
            break;
        }
        match receiver.recv_timeout(Duration::from_millis(250)) {
            Ok(Incoming::Command(line)) => {
                let result = serde_json::from_str::<LabCommand>(&line)
                    .context("invalid stream-lab command")
                    .and_then(|command| {
                        handle_command(command, &mut session, &program_data, &powershell)
                    });
                let (response, should_exit) = match result {
                    Ok((event, expiry, should_exit)) => (
                        Response {
                            ok: true,
                            event,
                            error: None,
                            expires_at_unix_ms: expiry,
                        },
                        should_exit,
                    ),
                    Err(error) => (
                        Response {
                            ok: false,
                            event: "error",
                            error: Some(format!("{error:#}")),
                            expires_at_unix_ms: None,
                        },
                        false,
                    ),
                };
                serde_json::to_writer(&mut writer, &response)?;
                writer.write_all(b"\n")?;
                writer.flush()?;
                if should_exit {
                    debug_assert!(service_exit_required(ServiceExitCause::End));
                    break;
                }
            }
            Ok(Incoming::Disconnected) => {
                debug_assert!(service_exit_required(ServiceExitCause::Disconnect));
                break;
            }
            Ok(Incoming::Error(error)) => {
                warn!(%error, "stream-lab pipe reader failed");
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    drop(session);
    info!("stream-lab helper stopped cleanly");
    Ok(())
}

fn pipe_reader(pipe: Pipe, sender: mpsc::Sender<Incoming>) {
    let mut reader = BufReader::new(pipe);
    loop {
        match read_line_limited(&mut reader, MAX_COMMAND_BYTES) {
            Ok(Some(line)) => {
                info!(bytes = line.len(), "stream-lab command received");
                if sender.send(Incoming::Command(line)).is_err() {
                    return;
                }
            }
            Ok(None) => {
                let _ = sender.send(Incoming::Disconnected);
                return;
            }
            Err(error) => {
                let _ = sender.send(Incoming::Error(error.to_string()));
                return;
            }
        }
    }
}

fn read_line_limited<R: BufRead>(reader: &mut R, limit: usize) -> io::Result<Option<String>> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if bytes.is_empty() {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "stream-lab command ended without newline",
            ));
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if bytes.len() + take > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "stream-lab command exceeds 64 KiB",
            ));
        }
        bytes.extend_from_slice(&available[..take]);
        reader.consume(take);
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
            return String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "command is not UTF-8"));
        }
    }
}

struct PipeServer(OwnedHandle);
unsafe impl Send for PipeServer {}

#[repr(C)]
struct NativeOverlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: RawHandle,
}

struct OverlappedOperation {
    native: NativeOverlapped,
    _event: OwnedHandle,
}

impl OverlappedOperation {
    fn new() -> io::Result<Self> {
        let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            native: NativeOverlapped {
                internal: 0,
                internal_high: 0,
                offset: 0,
                offset_high: 0,
                event,
            },
            _event: OwnedHandle(event),
        })
    }

    fn result(&mut self, handle: RawHandle) -> io::Result<u32> {
        let mut transferred = 0;
        if unsafe {
            GetOverlappedResult(
                handle,
                (&mut self.native as *mut NativeOverlapped).cast(),
                &mut transferred,
                0,
            )
        } == 0
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(transferred)
        }
    }

    fn poll(&mut self, handle: RawHandle, timeout: Duration) -> io::Result<Option<u32>> {
        match unsafe {
            WaitForSingleObject(
                self.native.event,
                timeout.as_millis().min(u32::MAX as u128) as u32,
            )
        } {
            WAIT_OBJECT_0 => self.result(handle).map(Some),
            WAIT_TIMEOUT => Ok(None),
            WAIT_FAILED => Err(io::Error::last_os_error()),
            value => Err(io::Error::other(format!(
                "unexpected overlapped wait result {value}"
            ))),
        }
    }

    fn wait(&mut self, handle: RawHandle) -> io::Result<u32> {
        match unsafe { WaitForSingleObject(self.native.event, INFINITE) } {
            WAIT_OBJECT_0 => self.result(handle),
            WAIT_FAILED => Err(io::Error::last_os_error()),
            value => Err(io::Error::other(format!(
                "unexpected overlapped wait result {value}"
            ))),
        }
    }

    fn cancel_and_drain(&mut self, handle: RawHandle) -> io::Result<()> {
        if unsafe { CancelIoEx(handle, (&mut self.native as *mut NativeOverlapped).cast()) } == 0
            && last_error() != ERROR_NOT_FOUND
        {
            return Err(io::Error::last_os_error());
        }
        match unsafe { WaitForSingleObject(self.native.event, INFINITE) } {
            WAIT_OBJECT_0 => {
                let mut transferred = 0;
                if unsafe {
                    GetOverlappedResult(
                        handle,
                        (&mut self.native as *mut NativeOverlapped).cast(),
                        &mut transferred,
                        0,
                    )
                } == 0
                    && last_error() != ERROR_OPERATION_ABORTED
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            }
            WAIT_FAILED => Err(io::Error::last_os_error()),
            value => Err(io::Error::other(format!(
                "unexpected cancelled-overlapped wait result {value}"
            ))),
        }
    }
}

impl PipeServer {
    fn bind_for_sid(sid: &str) -> io::Result<Self> {
        Self::bind_named_for_sid(PIPE_NAME, sid)
    }

    fn bind_named_for_sid(name: &str, sid: &str) -> io::Result<Self> {
        let descriptor = SecurityDescriptor::from_sddl(&format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid})"))?;
        let attributes = SecurityAttributes {
            length: std::mem::size_of::<SecurityAttributes>() as u32,
            security_descriptor: descriptor.0,
            inherit_handle: 0,
        };
        let name = wide_null(name);
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                pipe_open_mode(),
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                64 * 1024,
                64 * 1024,
                0,
                &attributes,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(OwnedHandle(handle)))
        }
    }

    fn authorize_connected(self, expected_sid: &str) -> io::Result<Pipe> {
        let mut process_id = 0;
        if unsafe { GetNamedPipeClientProcessId(self.0 .0, &mut process_id) } == 0 {
            return Err(io::Error::last_os_error());
        }
        debug!(process_id, "stream-lab pipe client process identified");
        let actual_sid = process_user_sid(process_id)?;
        if !client_is_authorized(expected_sid, &actual_sid) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "stream-lab pipe client belongs to another Windows user",
            ));
        }
        info!(process_id, %actual_sid, "stream-lab pipe client authorized");
        Ok(Pipe(self.0))
    }

    fn accept_for_sid_cancelable(
        self,
        expected_sid: &str,
        stop: Arc<AtomicBool>,
        timeout: Duration,
    ) -> io::Result<Option<Pipe>> {
        let mut operation = OverlappedOperation::new()?;
        debug!("stream-lab pipe beginning overlapped ConnectNamedPipe");
        if unsafe {
            ConnectNamedPipe(
                self.0 .0,
                (&mut operation.native as *mut NativeOverlapped).cast(),
            )
        } != 0
        {
            return self.authorize_connected(expected_sid).map(Some);
        }
        match last_error() {
            ERROR_PIPE_CONNECTED => return self.authorize_connected(expected_sid).map(Some),
            ERROR_IO_PENDING => {}
            error => return Err(io::Error::from_raw_os_error(error as i32)),
        }
        let started = Instant::now();
        loop {
            match operation.poll(self.0 .0, Duration::from_millis(50))? {
                Some(_) => return self.authorize_connected(expected_sid).map(Some),
                None if stop.load(Ordering::Acquire) || started.elapsed() >= timeout => {
                    operation.cancel_and_drain(self.0 .0)?;
                    return Ok(None);
                }
                None => {}
            }
        }
    }
}

struct Pipe(OwnedHandle);
unsafe impl Send for Pipe {}

impl Pipe {
    fn try_clone(&self) -> io::Result<Self> {
        let process = unsafe { GetCurrentProcess() };
        let mut duplicate = ptr::null_mut();
        if unsafe {
            DuplicateHandle(
                process,
                self.0 .0,
                process,
                &mut duplicate,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        } == 0
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(OwnedHandle(duplicate)))
        }
    }
}

impl Read for Pipe {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut operation = OverlappedOperation::new()?;
        let immediate = unsafe {
            ReadFile(
                self.0 .0,
                buffer.as_mut_ptr().cast(),
                buffer.len().min(u32::MAX as usize) as u32,
                ptr::null_mut(),
                (&mut operation.native as *mut NativeOverlapped).cast(),
            )
        } != 0;
        if !immediate {
            match last_error() {
                ERROR_IO_PENDING => {}
                ERROR_BROKEN_PIPE | ERROR_NO_DATA => return Ok(0),
                error => return Err(io::Error::from_raw_os_error(error as i32)),
            }
        }
        if immediate {
            operation.result(self.0 .0).map(|bytes| bytes as usize)
        } else {
            operation.wait(self.0 .0).map(|bytes| bytes as usize)
        }
    }
}

impl Write for Pipe {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let mut operation = OverlappedOperation::new()?;
        let immediate = unsafe {
            WriteFile(
                self.0 .0,
                buffer.as_ptr().cast(),
                buffer.len().min(u32::MAX as usize) as u32,
                ptr::null_mut(),
                (&mut operation.native as *mut NativeOverlapped).cast(),
            )
        } != 0;
        if !immediate {
            match last_error() {
                ERROR_IO_PENDING => {}
                ERROR_BROKEN_PIPE | ERROR_NO_DATA => return Ok(0),
                error => return Err(io::Error::from_raw_os_error(error as i32)),
            }
        }
        if immediate {
            operation.result(self.0 .0).map(|bytes| bytes as usize)
        } else {
            operation.wait(self.0 .0).map(|bytes| bytes as usize)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if unsafe { FlushFileBuffers(self.0 .0) } != 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

struct OwnedHandle(RawHandle);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

struct SecurityDescriptor(*mut c_void);
impl SecurityDescriptor {
    fn from_sddl(sddl: &str) -> io::Result<Self> {
        let sddl = wide_null(sddl);
        let mut descriptor = ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } == 0
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(descriptor))
        }
    }
}
impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

#[repr(C)]
struct SecurityAttributes {
    length: u32,
    security_descriptor: *mut c_void,
    inherit_handle: i32,
}

#[cfg(test)]
fn current_user_sid() -> io::Result<String> {
    let mut token = ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = OwnedHandle(token);
    sid_from_token(token.0)
}

fn process_user_sid(process_id: u32) -> io::Result<String> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    let process = OwnedHandle(process);
    let mut token = ptr::null_mut();
    if unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = OwnedHandle(token);
    sid_from_token(token.0)
}

fn sid_from_token(token: RawHandle) -> io::Result<String> {
    let mut needed = 0;
    unsafe {
        GetTokenInformation(token, TOKEN_USER_CLASS, ptr::null_mut(), 0, &mut needed);
    }
    if needed == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0u8; needed as usize];
    if unsafe {
        GetTokenInformation(
            token,
            TOKEN_USER_CLASS,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let sid = unsafe { *(buffer.as_ptr() as *const *mut c_void) };
    let mut text = ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _text_owner = LocalMemory(text.cast());
    let mut len = 0;
    unsafe {
        while *text.add(len) != 0 {
            len += 1;
        }
        Ok(String::from_utf16_lossy(std::slice::from_raw_parts(
            text, len,
        )))
    }
}

struct LocalMemory(*mut c_void);
impl Drop for LocalMemory {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

fn program_data_directory() -> io::Result<PathBuf> {
    const FOLDERID_PROGRAM_DATA: GUID = GUID::from_u128(0x62ab5d82_fdc1_4dc3_a9dd_070d1d495d97);
    let mut path = ptr::null_mut();
    let result =
        unsafe { SHGetKnownFolderPath(&FOLDERID_PROGRAM_DATA, 0, ptr::null_mut(), &mut path) };
    if result < 0 {
        return Err(io::Error::from_raw_os_error(result));
    }
    let _owner = CoTaskMemory(path.cast());
    let mut len = 0;
    unsafe {
        while *path.add(len) != 0 {
            len += 1;
        }
        Ok(PathBuf::from(OsString::from_wide(
            std::slice::from_raw_parts(path, len),
        )))
    }
}

struct CoTaskMemory(*mut c_void);
impl Drop for CoTaskMemory {
    fn drop(&mut self) {
        unsafe {
            CoTaskMemFree(self.0);
        }
    }
}

fn system_directory() -> io::Result<PathBuf> {
    let mut buffer = vec![0u16; 32_768];
    let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
    if length == 0 || length as usize >= buffer.len() {
        return Err(io::Error::last_os_error());
    }
    buffer.truncate(length as usize);
    Ok(PathBuf::from(OsString::from_wide(&buffer)))
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_path(value: &Path) -> Vec<u16> {
    value
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn last_error() -> u32 {
    unsafe { GetLastError() }
}

type RawHandle = *mut c_void;
const INVALID_HANDLE_VALUE: RawHandle = -1isize as RawHandle;
const PIPE_ACCESS_DUPLEX: u32 = 3;
const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
const PIPE_TYPE_BYTE: u32 = 0;
const PIPE_READMODE_BYTE: u32 = 0;
const PIPE_WAIT: u32 = 0;
const PIPE_REJECT_REMOTE_CLIENTS: u32 = 8;
const DUPLICATE_SAME_ACCESS: u32 = 2;
const TOKEN_QUERY: u32 = 8;
const TOKEN_USER_CLASS: u32 = 1;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const ERROR_BROKEN_PIPE: u32 = 109;
const ERROR_NO_DATA: u32 = 232;
const ERROR_PIPE_CONNECTED: u32 = 535;
const ERROR_OPERATION_ABORTED: u32 = 995;
const ERROR_IO_PENDING: u32 = 997;
const ERROR_NOT_FOUND: u32 = 1168;
const WAIT_OBJECT_0: u32 = 0;
const WAIT_FAILED: u32 = u32::MAX;
const WAIT_TIMEOUT: u32 = 258;
const INFINITE: u32 = u32::MAX;
const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

fn pipe_open_mode() -> u32 {
    PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateNamedPipeW(
        name: *const u16,
        open_mode: u32,
        pipe_mode: u32,
        max_instances: u32,
        out_buffer_size: u32,
        in_buffer_size: u32,
        default_timeout_ms: u32,
        security_attributes: *const SecurityAttributes,
    ) -> RawHandle;
    fn ConnectNamedPipe(pipe: RawHandle, overlapped: *mut c_void) -> i32;
    fn GetNamedPipeClientProcessId(pipe: RawHandle, process_id: *mut u32) -> i32;
    fn ReadFile(
        file: RawHandle,
        buffer: *mut c_void,
        bytes_to_read: u32,
        bytes_read: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn WriteFile(
        file: RawHandle,
        buffer: *const c_void,
        bytes_to_write: u32,
        bytes_written: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn FlushFileBuffers(file: RawHandle) -> i32;
    fn DuplicateHandle(
        source_process: RawHandle,
        source_handle: RawHandle,
        target_process: RawHandle,
        target_handle: *mut RawHandle,
        desired_access: u32,
        inherit_handle: i32,
        options: u32,
    ) -> i32;
    fn CloseHandle(handle: RawHandle) -> i32;
    fn CancelIoEx(handle: RawHandle, overlapped: *mut c_void) -> i32;
    fn CreateEventW(
        event_attributes: *const c_void,
        manual_reset: i32,
        initial_state: i32,
        name: *const u16,
    ) -> RawHandle;
    fn WaitForSingleObject(handle: RawHandle, milliseconds: u32) -> u32;
    fn GetOverlappedResult(
        file: RawHandle,
        overlapped: *mut c_void,
        bytes_transferred: *mut u32,
        wait: i32,
    ) -> i32;
    fn GetCurrentProcess() -> RawHandle;
    fn GetLastError() -> u32;
    fn GetSystemDirectoryW(buffer: *mut u16, size: u32) -> u32;
    fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> RawHandle;
    fn LocalFree(memory: *mut c_void) -> *mut c_void;
    fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        source: *const u16,
        revision: u32,
        descriptor: *mut *mut c_void,
        size: *mut u32,
    ) -> i32;
    fn ConvertStringSidToSidW(source: *const u16, sid: *mut *mut c_void) -> i32;
    fn OpenProcessToken(process: RawHandle, desired_access: u32, token: *mut RawHandle) -> i32;
    fn GetTokenInformation(
        token: RawHandle,
        class: u32,
        information: *mut c_void,
        information_length: u32,
        return_length: *mut u32,
    ) -> i32;
    fn ConvertSidToStringSidW(sid: *mut c_void, text: *mut *mut u16) -> i32;
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn SHGetKnownFolderPath(
        folder_id: *const GUID,
        flags: u32,
        token: RawHandle,
        path: *mut *mut u16,
    ) -> i32;
}

#[link(name = "ole32")]
unsafe extern "system" {
    fn CoTaskMemFree(memory: *mut c_void);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_launch_uses_the_scm_and_console_is_explicit() {
        assert_eq!(process_mode(&[]), ProcessMode::ScmService);
        assert_eq!(
            process_mode(&[OsString::from("--console")]),
            ProcessMode::Console
        );
    }

    #[test]
    fn service_config_is_strict_and_supplies_the_only_client_owner_sid() {
        let config =
            parse_service_config(br#"{"schemaVersion":1,"ownerSid":"S-1-5-21-100-200-300-1001"}"#)
                .unwrap();
        assert_eq!(config.schema_version, 1);
        assert_eq!(config.owner_sid, "S-1-5-21-100-200-300-1001");
        assert!(serde_json::from_str::<LabServiceConfig>(
            r#"{"schemaVersion":1,"ownerSid":"S-1-5-21-100-200-300-1001","admin":true}"#
        )
        .is_err());
        assert!(client_is_authorized(
            &config.owner_sid,
            "s-1-5-21-100-200-300-1001"
        ));
        assert!(!client_is_authorized(&config.owner_sid, "S-1-5-18"));
        assert!(parse_service_config(
            br#"{"schemaVersion":2,"ownerSid":"S-1-5-21-100-200-300-1001"}"#
        )
        .is_err());
        assert!(parse_service_config(br#"{"schemaVersion":1,"ownerSid":"not-a-sid"}"#).is_err());
    }

    #[test]
    fn every_terminal_service_event_requires_cleanup_and_exit() {
        for cause in [
            ServiceExitCause::End,
            ServiceExitCause::Disconnect,
            ServiceExitCause::Expiry,
            ServiceExitCause::ScmStop,
        ] {
            assert!(service_exit_required(cause), "{cause:?}");
        }
    }

    fn profile() -> BenchmarkProfile {
        BenchmarkProfile {
            schema_version: 1,
            implementation_variant: ImplementationVariant::Production,
            experiment_id: None,
            transport: Transport::Wss,
            codec: "hevc".into(),
            stream_mode: StreamMode::Work,
            adaptive: false,
            width: 1920,
            height: 1080,
            fps: 30,
            bitrate_bps: 4_000_000,
            gop_seconds: 2,
            wss_topology: None,
            hybrid_mode: HybridMode::Off,
            hybrid_cache_mode: HybridCacheMode::SuppressReactivate,
            encoder: BenchmarkEncoder {
                preset: Preset::P3,
                tuning: Tuning::UltraLowLatency,
                multipass: Multipass::Quarter,
                aq_strength: 8,
                temporal_aq: false,
                aq_mode: None,
                vbv_frames: 2,
                lookahead_depth: 0,
                weighted_prediction: false,
                dirty_qp_map: false,
                main10: false,
                roi_mode: RoiMode::Off,
                resource_mode: ResourceMode::PerFrame,
                async_depth: 0,
            },
        }
    }

    #[test]
    fn lease_is_strictly_bounded_to_ten_minutes() {
        let valid = LabCommand::Begin {
            run_id: "bb66c7b7-0981-44cf-8122-3d3449e5fd47".into(),
            lease_seconds: MAX_LEASE_SECONDS,
            vdd: true,
        };
        assert!(validate_command(&valid).is_ok());
        let overlong = LabCommand::Apply {
            run_id: "bb66c7b7-0981-44cf-8122-3d3449e5fd47".into(),
            lease_seconds: MAX_LEASE_SECONDS + 1,
            bandwidth_mbps: Some(4),
            profile: profile(),
        };
        assert!(validate_command(&overlong).is_err());
    }

    #[test]
    fn renew_is_a_bounded_matching_run_command() {
        let valid = r#"{"command":"renew","runId":"bb66c7b7-0981-44cf-8122-3d3449e5fd47","leaseSeconds":600}"#;
        let command = serde_json::from_str::<LabCommand>(valid).expect("renew command");
        assert!(validate_command(&command).is_ok());

        let overlong = r#"{"command":"renew","runId":"bb66c7b7-0981-44cf-8122-3d3449e5fd47","leaseSeconds":601}"#;
        let command = serde_json::from_str::<LabCommand>(overlong).expect("renew command");
        assert!(validate_command(&command).is_err());
    }

    #[test]
    fn shape_is_a_bounded_allowlisted_command_without_a_profile_or_process_surface() {
        for bandwidth in ["2", "4", "6", "8", "12", "null"] {
            let json = format!(
                r#"{{"command":"shape","runId":"bb66c7b7-0981-44cf-8122-3d3449e5fd47","leaseSeconds":600,"bandwidthMbps":{bandwidth}}}"#
            );
            let command = serde_json::from_str::<LabCommand>(&json).expect("shape command");
            assert!(validate_command(&command).is_ok(), "bandwidth {bandwidth}");
        }

        for json in [
            r#"{"command":"shape","runId":"bb66c7b7-0981-44cf-8122-3d3449e5fd47","leaseSeconds":601,"bandwidthMbps":4}"#,
            r#"{"command":"shape","runId":"bb66c7b7-0981-44cf-8122-3d3449e5fd47","leaseSeconds":60,"bandwidthMbps":3}"#,
            r#"{"command":"shape","runId":"bb66c7b7-0981-44cf-8122-3d3449e5fd47","leaseSeconds":60,"bandwidthMbps":4,"process":"cmd.exe"}"#,
        ] {
            if let Ok(command) = serde_json::from_str::<LabCommand>(json) {
                assert!(validate_command(&command).is_err(), "accepted {json}");
            }
        }
    }

    #[test]
    fn shape_requires_the_matching_active_run_before_touching_state() {
        let json = r#"{"command":"shape","runId":"bb66c7b7-0981-44cf-8122-3d3449e5fd47","leaseSeconds":60,"bandwidthMbps":null}"#;
        let command = serde_json::from_str::<LabCommand>(json).unwrap();
        let mut no_session = None;
        assert!(handle_command(
            command,
            &mut no_session,
            Path::new("unused"),
            Path::new("must-not-launch")
        )
        .is_err());

        let path = std::env::temp_dir().join(format!(
            "bubbleplay-shape-mismatch-{}-{}.json",
            std::process::id(),
            unix_ms(SystemTime::now()).unwrap()
        ));
        let old_expiry = SystemTime::now() + Duration::from_secs(5);
        let mut wrong_session = Some(ActiveSession {
            run_id: "aa66c7b7-0981-44cf-8122-3d3449e5fd47".into(),
            expires_at: old_expiry,
            profile_path: path.clone(),
            powershell: PathBuf::from("must-not-launch"),
            qos_active: false,
            watchdog_started: false,
            vdd: None,
        });
        let command = serde_json::from_str::<LabCommand>(json).unwrap();
        assert!(handle_command(
            command,
            &mut wrong_session,
            Path::new("unused"),
            Path::new("must-not-launch")
        )
        .is_err());
        assert_eq!(wrong_session.as_ref().unwrap().expires_at, old_expiry);
        assert!(!path.exists());
    }

    #[test]
    fn game_profile_allows_sixty_fps_and_allowlisted_gop() {
        let json = r#"{"command":"apply","runId":"bb66c7b7-0981-44cf-8122-3d3449e5fd47","leaseSeconds":60,"bandwidthMbps":8,"profile":{"schemaVersion":1,"transport":"wss","codec":"hevc","streamMode":"game","adaptive":false,"width":1920,"height":1080,"fps":60,"bitrateBps":8000000,"gopSeconds":1,"encoder":{"preset":"p3","tuning":"ultra-low-latency","multipass":"disabled","aqStrength":8,"temporalAq":false,"vbvFrames":1}}}"#;
        let command = serde_json::from_str::<LabCommand>(json).expect("game apply command");
        assert!(validate_command(&command).is_ok());
    }

    #[test]
    fn v2_candidate_profile_accepts_the_full_bounded_encoder_surface() {
        let json = r#"{"command":"apply","runId":"bb66c7b7-0981-44cf-8122-3d3449e5fd47","leaseSeconds":60,"bandwidthMbps":12,"profile":{"schemaVersion":2,"implementationVariant":"candidate","experimentId":"nvenc-full_01","transport":"wss","codec":"hevc","streamMode":"work","adaptive":false,"width":1920,"height":1080,"fps":30,"bitrateBps":12000000,"gopSeconds":4,"encoder":{"preset":"p7","tuning":"low-latency","multipass":"full","aqMode":"temporal","aqStrength":15,"vbvFrames":4,"lookaheadDepth":2,"weightedPrediction":true}}}"#;
        let command = serde_json::from_str::<LabCommand>(json).expect("v2 apply command");
        assert!(validate_command(&command).is_ok());
    }

    #[test]
    fn v2_pipeline_controls_round_trip_into_a_host_accepted_normalized_lease() {
        let json = r#"{"command":"apply","runId":"bb66c7b7-0981-44cf-8122-3d3449e5fd47","leaseSeconds":60,"bandwidthMbps":null,"profile":{"schemaVersion":2,"implementationVariant":"candidate","experimentId":"roi-text-edge","transport":"wss","codec":"hevc","streamMode":"work","adaptive":false,"width":1920,"height":1080,"fps":30,"bitrateBps":6000000,"gopSeconds":2,"wssTopology":{"laneCount":16,"dataShards":15,"parityShards":1},"hybridMode":"off","encoder":{"preset":"p3","tuning":"ultra-low-latency","multipass":"disabled","aqMode":"spatial","aqStrength":8,"vbvFrames":1,"lookaheadDepth":0,"weightedPrediction":false,"dirtyQpMap":false,"main10":false,"roiMode":"text-edge","resourceMode":"per-frame","asyncDepth":0}}}"#;
        let command = serde_json::from_str::<LabCommand>(json).expect("pipeline apply command");
        assert!(validate_command(&command).is_ok());
        let LabCommand::Apply { profile, .. } = command else {
            unreachable!()
        };
        let path = std::env::temp_dir().join(format!(
            "bubbleplay-pipeline-profile-{}-{}.json",
            std::process::id(),
            unix_ms(SystemTime::now()).unwrap()
        ));
        write_profile_atomic(
            &path,
            &StoredProfile::new("bb66c7b7-0981-44cf-8122-3d3449e5fd47", 1_300_000, &profile),
        )
        .unwrap();
        let loaded =
            host_benchmark::load_profile(&path, 1_000_000).expect("host accepts helper lease");
        assert_eq!(
            loaded.effective_roi_mode(),
            host_benchmark::BenchmarkRoiMode::TextEdge
        );
        assert_eq!(
            loaded.encoder.resource_mode,
            host_benchmark::BenchmarkResourceMode::PerFrame
        );
        assert_eq!(loaded.hybrid_mode, host_benchmark::BenchmarkHybridMode::Off);
        fs::remove_file(path).ok();
    }

    #[test]
    fn quiescent_cache_policy_round_trips_through_the_helper_contract() {
        let json = r#"{"command":"apply","runId":"bb66c7b7-0981-44cf-8122-3d3449e5fd47","leaseSeconds":60,"bandwidthMbps":null,"profile":{"schemaVersion":2,"implementationVariant":"candidate","experimentId":"hybrid-png-quiescent-detail-dda-gated","transport":"wss","codec":"hevc","streamMode":"work","adaptive":false,"width":1920,"height":1080,"fps":30,"bitrateBps":6000000,"gopSeconds":2,"wssTopology":{"laneCount":16,"dataShards":15,"parityShards":1},"hybridMode":"png-quiescent-detail","hybridCacheMode":"dda-gated","encoder":{"preset":"p3","tuning":"ultra-low-latency","multipass":"disabled","aqMode":"spatial","aqStrength":8,"vbvFrames":1,"lookaheadDepth":0,"weightedPrediction":false,"dirtyQpMap":false,"main10":false,"roiMode":"off","resourceMode":"per-frame","asyncDepth":0}}}"#;
        let invalid_static = json.replace(
            r#""hybridMode":"png-quiescent-detail""#,
            r#""hybridMode":"png-static-roi""#,
        );
        let invalid_command =
            serde_json::from_str::<LabCommand>(&invalid_static).expect("static hybrid apply");
        assert!(validate_command(&invalid_command).is_err());

        let defaulted = json.replace(r#","hybridCacheMode":"dda-gated""#, "");
        let defaulted_command =
            serde_json::from_str::<LabCommand>(&defaulted).expect("default cache policy apply");
        let LabCommand::Apply {
            profile: defaulted_profile,
            ..
        } = defaulted_command
        else {
            unreachable!()
        };
        assert_eq!(
            defaulted_profile.hybrid_cache_mode,
            HybridCacheMode::SuppressReactivate
        );
        assert!(defaulted_profile.has_valid_pipeline_experiment());

        let command = serde_json::from_str::<LabCommand>(json).expect("quiescent cache apply");
        assert!(validate_command(&command).is_ok());
        let LabCommand::Apply { profile, .. } = command else {
            unreachable!()
        };
        let path = std::env::temp_dir().join(format!(
            "bubbleplay-quiescent-cache-profile-{}-{}.json",
            std::process::id(),
            unix_ms(SystemTime::now()).unwrap()
        ));
        write_profile_atomic(
            &path,
            &StoredProfile::new("bb66c7b7-0981-44cf-8122-3d3449e5fd47", 1_300_000, &profile),
        )
        .unwrap();
        let loaded =
            host_benchmark::load_profile(&path, 1_000_000).expect("host accepts cache policy");
        assert_eq!(
            loaded.hybrid_mode,
            host_benchmark::BenchmarkHybridMode::PngQuiescentDetail
        );
        assert_eq!(
            loaded.hybrid_cache_mode,
            host_benchmark::BenchmarkHybridCacheMode::DdaGated
        );
        fs::remove_file(path).ok();
    }

    #[test]
    fn independent_profile_destinations_never_share_an_atomic_temp_file() {
        let parent = std::env::temp_dir();
        let run_id = "bb66c7b7-0981-44cf-8122-3d3449e5fd47";
        let first = profile_temp_path(&parent.join("first-profile.json"), run_id).unwrap();
        let second = profile_temp_path(&parent.join("second-profile.json"), run_id).unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn v2_profile_fails_closed_outside_experiment_bitrate_and_mode_bounds() {
        let valid = r#"{"command":"apply","runId":"bb66c7b7-0981-44cf-8122-3d3449e5fd47","leaseSeconds":60,"bandwidthMbps":8,"profile":{"schemaVersion":2,"implementationVariant":"candidate","experimentId":"candidate-01","transport":"wss","codec":"hevc","streamMode":"game","adaptive":false,"width":1920,"height":1080,"fps":60,"bitrateBps":8000000,"gopSeconds":1,"encoder":{"preset":"p6","tuning":"ultra-low-latency","multipass":"quarter","aqMode":"spatial","aqStrength":8,"vbvFrames":2,"lookaheadDepth":1,"weightedPrediction":false}}}"#;
        for invalid in [
            valid.replace("candidate-01", "bad experiment"),
            valid.replace("8000000", "1999999"),
            valid.replace("\"lookaheadDepth\":1", "\"lookaheadDepth\":2"),
        ] {
            let command =
                serde_json::from_str::<LabCommand>(&invalid).expect("well-formed command");
            assert!(validate_command(&command).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn stored_profile_revision_is_strict_and_legacy_defaults_to_zero() {
        let with_revision = r#"{"schemaVersion":1,"runId":"bb66c7b7-0981-44cf-8122-3d3449e5fd47","expiresAtMs":1300000,"applyRevision":7,"transport":"wss","codec":"hevc","streamMode":"work","adaptive":false,"width":1920,"height":1080,"fps":30,"bitrateBps":4000000,"gopSeconds":2,"encoder":{"preset":"p3","tuning":"ultra-low-latency","multipass":"disabled","aqStrength":8,"temporalAq":false,"vbvFrames":1}}"#;
        let stored: StoredProfile =
            serde_json::from_str(with_revision).expect("revision-aware stored profile");
        assert_eq!(stored.apply_revision, 7);

        let legacy = with_revision.replace("\"applyRevision\":7,", "");
        let stored: StoredProfile = serde_json::from_str(&legacy).expect("legacy stored profile");
        assert_eq!(stored.apply_revision, 0);
    }

    #[test]
    fn protocol_rejects_unknown_fields_and_noncanonical_run_ids() {
        let unknown = r#"{"command":"clear","runId":"bb66c7b7-0981-44cf-8122-3d3449e5fd47","path":"C:\\Windows"}"#;
        assert!(serde_json::from_str::<LabCommand>(unknown).is_err());
        let command = LabCommand::Clear {
            run_id: "../../unsafe".into(),
        };
        assert!(validate_command(&command).is_err());
    }

    #[test]
    fn profile_and_bandwidth_bounds_are_enforced() {
        let command = LabCommand::Apply {
            run_id: "bb66c7b7-0981-44cf-8122-3d3449e5fd47".into(),
            lease_seconds: 60,
            bandwidth_mbps: Some(3),
            profile: profile(),
        };
        assert!(validate_command(&command).is_err());
        let mut invalid = profile();
        invalid.encoder.vbv_frames = 5;
        let command = LabCommand::Apply {
            run_id: "bb66c7b7-0981-44cf-8122-3d3449e5fd47".into(),
            lease_seconds: 60,
            bandwidth_mbps: Some(4),
            profile: invalid,
        };
        assert!(validate_command(&command).is_err());
    }

    #[test]
    fn mode_fps_caps_and_all_gop_options_are_enforced() {
        for gop_seconds in [1, 2, 4] {
            let mut valid = profile();
            valid.gop_seconds = gop_seconds;
            let command = LabCommand::Apply {
                run_id: "bb66c7b7-0981-44cf-8122-3d3449e5fd47".into(),
                lease_seconds: 60,
                bandwidth_mbps: Some(4),
                profile: valid,
            };
            assert!(validate_command(&command).is_ok(), "gop {gop_seconds}");
        }

        let mut work_too_fast = profile();
        work_too_fast.fps = 31;
        let mut game_too_fast = profile();
        game_too_fast.stream_mode = StreamMode::Game;
        game_too_fast.fps = 61;
        let mut invalid_gop = profile();
        invalid_gop.gop_seconds = 3;
        for invalid in [work_too_fast, game_too_fast, invalid_gop] {
            let command = LabCommand::Apply {
                run_id: "bb66c7b7-0981-44cf-8122-3d3449e5fd47".into(),
                lease_seconds: 60,
                bandwidth_mbps: Some(4),
                profile: invalid,
            };
            assert!(validate_command(&command).is_err());
        }
    }

    #[test]
    fn profile_cleanup_cannot_delete_another_runs_file() {
        let root = std::env::temp_dir().join(format!(
            "bubbleplay-lab-helper-{}-{}",
            std::process::id(),
            unix_ms(SystemTime::now()).unwrap()
        ));
        let path = root.join(PROFILE_FILE_NAME);
        let owner = "bb66c7b7-0981-44cf-8122-3d3449e5fd47";
        let other = "aa66c7b7-0981-44cf-8122-3d3449e5fd47";
        write_profile_atomic(&path, &StoredProfile::new(owner, 1234, &profile())).unwrap();
        remove_profile_if_owned(&path, other).unwrap();
        assert!(path.exists());
        remove_profile_if_owned(&path, owner).unwrap();
        assert!(!path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn renewal_changes_only_the_owned_profiles_expiry() {
        let root = std::env::temp_dir().join(format!(
            "bubbleplay-lab-renew-{}-{}",
            std::process::id(),
            unix_ms(SystemTime::now()).unwrap()
        ));
        let path = root.join(PROFILE_FILE_NAME);
        let run_id = "bb66c7b7-0981-44cf-8122-3d3449e5fd47";
        let original = StoredProfile::new(run_id, 1_000, &profile());
        write_profile_atomic(&path, &original).unwrap();

        assert!(renew_profile_expiry(&path, run_id, 2_000).unwrap());
        let renewed: StoredProfile = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let mut expected = original.clone();
        expected.expires_at_ms = 2_000;
        assert_eq!(renewed, expected);

        let before_mismatch = fs::read(&path).unwrap();
        assert!(
            renew_profile_expiry(&path, "aa66c7b7-0981-44cf-8122-3d3449e5fd47", 3_000).is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), before_mismatch);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn every_identical_apply_advances_revision_while_renew_preserves_it() {
        let root = std::env::temp_dir().join(format!(
            "bubbleplay-lab-apply-revision-{}-{}",
            std::process::id(),
            unix_ms(SystemTime::now()).unwrap()
        ));
        let path = root.join(PROFILE_FILE_NAME);
        let run_id = "bb66c7b7-0981-44cf-8122-3d3449e5fd47";

        assert_eq!(
            publish_applied_profile(&path, run_id, 1_000, &profile()).unwrap(),
            1
        );
        assert_eq!(
            publish_applied_profile(&path, run_id, 2_000, &profile()).unwrap(),
            2
        );
        let applied: StoredProfile = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(applied.apply_revision, 2);

        assert!(renew_profile_expiry(&path, run_id, 3_000).unwrap());
        let renewed: StoredProfile = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(renewed.apply_revision, 2);
        assert_eq!(renewed.expires_at_ms, 3_000);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn profile_renewal_serializes_with_expiry_cleanup_lock() {
        use std::os::windows::fs::OpenOptionsExt;
        use std::sync::mpsc;

        let root = std::env::temp_dir().join(format!(
            "bubbleplay-lab-lock-{}-{}",
            std::process::id(),
            unix_ms(SystemTime::now()).unwrap()
        ));
        let path = root.join(PROFILE_FILE_NAME);
        let lock_path = PathBuf::from(format!("{}.lock", path.display()));
        let run_id = "bb66c7b7-0981-44cf-8122-3d3449e5fd47";
        write_profile_atomic(&path, &StoredProfile::new(run_id, 1_000, &profile())).unwrap();
        let held = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .share_mode(0)
            .open(&lock_path)
            .unwrap();
        let (tx, rx) = mpsc::channel();
        let renew_path = path.clone();
        let worker = thread::spawn(move || {
            tx.send(renew_profile_expiry(&renew_path, run_id, 2_000))
                .unwrap();
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(75)).is_err(),
            "renewal bypassed the cleanup lock"
        );
        drop(held);
        assert!(rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap());
        worker.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn disabled_shape_profile_keeps_the_lease_but_restores_host_adaptive_policy() {
        let root = std::env::temp_dir().join(format!(
            "bubbleplay-lab-shape-{}-{}",
            std::process::id(),
            unix_ms(SystemTime::now()).unwrap()
        ));
        let path = root.join(PROFILE_FILE_NAME);
        let run_id = "bb66c7b7-0981-44cf-8122-3d3449e5fd47";
        let now = unix_ms(SystemTime::now()).unwrap();
        let expiry = now + 60_000;
        assert_eq!(
            publish_applied_profile(&path, run_id, now + 30_000, &profile()).unwrap(),
            1
        );
        assert_eq!(publish_disabled_profile(&path, run_id, expiry).unwrap(), 1);

        let stored: StoredProfile = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(stored.run_id, run_id);
        assert_eq!(stored.expires_at_ms, expiry);
        assert_eq!(stored.apply_revision, 1);
        assert!(stored.adaptive);
        assert!(
            host_benchmark::load_profile(&path, now).is_none(),
            "disabled lease marker must not override production adaptation"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn renew_extends_only_the_matching_active_session_without_an_applied_profile() {
        let run_id = "bb66c7b7-0981-44cf-8122-3d3449e5fd47";
        let old_expiry = SystemTime::now() + Duration::from_secs(5);
        let mut session = Some(ActiveSession {
            run_id: run_id.into(),
            expires_at: old_expiry,
            profile_path: std::env::temp_dir().join(format!(
                "bubbleplay-no-applied-profile-{}-{}.json",
                std::process::id(),
                unix_ms(SystemTime::now()).unwrap()
            )),
            powershell: PathBuf::from("must-not-be-launched"),
            qos_active: false,
            watchdog_started: false,
            vdd: None,
        });
        let result = handle_command(
            LabCommand::Renew {
                run_id: run_id.into(),
                lease_seconds: 600,
            },
            &mut session,
            Path::new("unused"),
            Path::new("unused"),
        )
        .unwrap();
        assert_eq!(result.0, "renewed");
        assert!(session.as_ref().unwrap().expires_at > old_expiry);

        let renewed_expiry = session.as_ref().unwrap().expires_at;
        assert!(handle_command(
            LabCommand::Renew {
                run_id: "aa66c7b7-0981-44cf-8122-3d3449e5fd47".into(),
                lease_seconds: 600,
            },
            &mut session,
            Path::new("unused"),
            Path::new("unused"),
        )
        .is_err());
        assert_eq!(session.as_ref().unwrap().expires_at, renewed_expiry);
    }

    #[test]
    fn dropping_a_session_removes_its_owned_profile() {
        let root = std::env::temp_dir().join(format!(
            "bubbleplay-lab-drop-{}-{}",
            std::process::id(),
            unix_ms(SystemTime::now()).unwrap()
        ));
        let path = root.join(PROFILE_FILE_NAME);
        let run_id = "bb66c7b7-0981-44cf-8122-3d3449e5fd47";
        write_profile_atomic(&path, &StoredProfile::new(run_id, 1234, &profile())).unwrap();
        let session = ActiveSession {
            run_id: run_id.into(),
            expires_at: SystemTime::now() + Duration::from_secs(60),
            profile_path: path.clone(),
            powershell: PathBuf::from("unused-because-qos-is-inactive"),
            qos_active: false,
            vdd: None,
            watchdog_started: false,
        };
        drop(session);
        assert!(!path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cleanup_removes_the_profile_even_when_qos_cleanup_fails() {
        let root = std::env::temp_dir().join(format!(
            "bubbleplay-lab-cleanup-{}-{}",
            std::process::id(),
            unix_ms(SystemTime::now()).unwrap()
        ));
        let path = root.join(PROFILE_FILE_NAME);
        let run_id = "dd88e9d9-2193-46ef-a344-5f566bf71f69";
        write_profile_atomic(&path, &StoredProfile::new(run_id, 1234, &profile())).unwrap();
        let mut session = ActiveSession {
            run_id: run_id.into(),
            expires_at: SystemTime::now() + Duration::from_secs(60),
            profile_path: path.clone(),
            powershell: root.join("missing-powershell.exe"),
            qos_active: true,
            vdd: None,
            watchdog_started: false,
        };
        assert!(session.clear().is_err());
        assert!(!path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn emitted_profile_is_accepted_by_the_host_lease_reader() {
        let root = std::env::temp_dir().join(format!(
            "bubbleplay-lab-contract-{}-{}",
            std::process::id(),
            unix_ms(SystemTime::now()).unwrap()
        ));
        let path = root.join(PROFILE_FILE_NAME);
        let run_id = "bb66c7b7-0981-44cf-8122-3d3449e5fd47";
        let now = unix_ms(SystemTime::now()).unwrap();
        write_profile_atomic(&path, &StoredProfile::new(run_id, now + 60_000, &profile())).unwrap();
        let loaded = host_benchmark::load_profile(&path, now).expect("host accepts helper lease");
        assert_eq!(loaded.run_id, run_id);
        assert_eq!(loaded.encoder.vbv_frames, 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repeated_renewals_never_expose_a_missing_or_invalid_destination() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Barrier};

        let root = std::env::temp_dir().join(format!(
            "bubbleplay-lab-renewal-{}-{}",
            std::process::id(),
            unix_ms(SystemTime::now()).unwrap()
        ));
        let path = root.join(PROFILE_FILE_NAME);
        let run_id = "bb66c7b7-0981-44cf-8122-3d3449e5fd47";
        let expiry = unix_ms(SystemTime::now()).unwrap() + 300_000;
        write_profile_atomic(&path, &StoredProfile::new(run_id, expiry, &profile())).unwrap();

        let barrier = Arc::new(Barrier::new(2));
        let done = Arc::new(AtomicBool::new(false));
        let reader_path = path.clone();
        let reader_barrier = barrier.clone();
        let reader_done = done.clone();
        let reader = thread::spawn(move || {
            reader_barrier.wait();
            let mut observed = 0;
            while !reader_done.load(Ordering::Acquire) || observed < 100 {
                let bytes = fs::read(&reader_path).expect("renewal destination never disappears");
                let stored: StoredProfile =
                    serde_json::from_slice(&bytes).expect("renewal destination remains valid JSON");
                assert_eq!(stored.run_id, run_id);
                observed += 1;
            }
        });

        barrier.wait();
        for index in 0..200 {
            let mut next = profile();
            next.bitrate_bps = if index % 2 == 0 { 4_000_000 } else { 6_000_000 };
            write_profile_atomic(&path, &StoredProfile::new(run_id, expiry, &next)).unwrap();
        }
        done.store(true, Ordering::Release);
        reader.join().unwrap();

        let final_profile: StoredProfile =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(final_profile.run_id, run_id);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn line_protocol_rejects_oversize_and_requires_newline() {
        let mut valid = io::Cursor::new(b"{}\n".to_vec());
        assert_eq!(
            read_line_limited(&mut valid, 10).unwrap(),
            Some("{}".into())
        );
        let mut over = io::Cursor::new(b"123456\n".to_vec());
        assert!(read_line_limited(&mut over, 4).is_err());
        let mut truncated = io::Cursor::new(b"{}".to_vec());
        assert!(read_line_limited(&mut truncated, 10).is_err());
    }

    #[test]
    fn qos_identity_is_derived_only_from_a_validated_run_id() {
        let run_id = "bb66c7b7-0981-44cf-8122-3d3449e5fd47";
        validate_run_id(run_id).unwrap();
        assert_eq!(
            qos_policy_name(run_id),
            "BubblePlayLab-bb66c7b7098144cf81223d3449e5fd47"
        );
        assert!(APPLY_QOS_SCRIPT.contains("bubbleplay-host.exe"));
        assert!(APPLY_QOS_SCRIPT.contains("$env:BUBBLEPLAY_LAB_POLICY"));
        assert!(!APPLY_QOS_SCRIPT.contains("$args"));
        assert!(!WATCHDOG_SCRIPT.contains("$args"));
        assert!(!APPLY_QOS_SCRIPT.contains("Invoke-Expression"));
        assert!(!WATCHDOG_SCRIPT.contains("Invoke-Expression"));
    }

    #[test]
    fn watchdog_rechecks_run_and_expiry_immediately_before_destructive_cleanup() {
        let profile_reads = WATCHDOG_SCRIPT
            .matches("Get-Content -LiteralPath $p -Raw|ConvertFrom-Json")
            .count();
        assert!(profile_reads >= 2, "watchdog must re-read before cleanup");
        assert!(WATCHDOG_SCRIPT.contains("[IO.FileShare]::None"));
        assert!(WATCHDOG_SCRIPT.contains("$latest.runId -ne $r"));
        assert!(WATCHDOG_SCRIPT.contains("[int64]$latest.expiresAtMs -ne $candidateExpiry"));
        assert!(WATCHDOG_SCRIPT.contains("[int64]$latest.expiresAtMs -gt $latestNow"));
    }

    #[test]
    fn fixed_powershell_receives_values_without_command_interpolation() {
        const CHECK: &str = "if($env:BUBBLEPLAY_LAB_TEST_VALUE -ne 'first second'){exit 9}";
        let powershell = system_directory()
            .unwrap()
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        run_powershell(
            &powershell,
            CHECK,
            &[("BUBBLEPLAY_LAB_TEST_VALUE", OsStr::new("first second"))],
        )
        .unwrap();
    }

    #[test]
    fn independent_watchdog_script_removes_an_expired_owned_profile() {
        let root = std::env::temp_dir().join(format!(
            "bubbleplay-lab-watchdog-{}-{}",
            std::process::id(),
            unix_ms(SystemTime::now()).unwrap()
        ));
        let path = root.join(PROFILE_FILE_NAME);
        let run_id = "bb66c7b7-0981-44cf-8122-3d3449e5fd47";
        write_profile_atomic(&path, &StoredProfile::new(run_id, 1, &profile())).unwrap();
        let powershell = system_directory()
            .unwrap()
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        let policy = qos_policy_name(run_id);
        let mut command = configured_powershell(
            &powershell,
            WATCHDOG_SCRIPT,
            &[
                ("BUBBLEPLAY_LAB_PROFILE", path.as_os_str()),
                ("BUBBLEPLAY_LAB_RUN", OsStr::new(run_id)),
                ("BUBBLEPLAY_LAB_POLICY", OsStr::new(&policy)),
                ("BUBBLEPLAY_LAB_DELAY", OsStr::new("0")),
            ],
        );
        let output = command.stderr(Stdio::piped()).output().unwrap();
        assert!(
            output.status.success(),
            "watchdog stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn one_watchdog_tracks_a_renewed_lease_until_the_final_expiry() {
        let root = std::env::temp_dir().join(format!(
            "bubbleplay-lab-watchdog-renew-{}-{}",
            std::process::id(),
            unix_ms(SystemTime::now()).unwrap()
        ));
        let path = root.join(PROFILE_FILE_NAME);
        let run_id = "cc77d8c8-1092-45df-9233-4e455af60e58";
        let first_expiry = unix_ms(SystemTime::now() + Duration::from_millis(300)).unwrap();
        write_profile_atomic(&path, &StoredProfile::new(run_id, first_expiry, &profile())).unwrap();
        let powershell = system_directory()
            .unwrap()
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        let policy = qos_policy_name(run_id);
        let mut watchdog = configured_powershell(
            &powershell,
            WATCHDOG_SCRIPT,
            &[
                ("BUBBLEPLAY_LAB_PROFILE", path.as_os_str()),
                ("BUBBLEPLAY_LAB_RUN", OsStr::new(run_id)),
                ("BUBBLEPLAY_LAB_POLICY", OsStr::new(&policy)),
                ("BUBBLEPLAY_LAB_POLL_MS", OsStr::new("50")),
            ],
        )
        .spawn()
        .unwrap();
        thread::sleep(Duration::from_millis(100));
        let renewed_expiry = unix_ms(SystemTime::now() + Duration::from_millis(700)).unwrap();
        write_profile_atomic(
            &path,
            &StoredProfile::new(run_id, renewed_expiry, &profile()),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(300));
        assert!(path.exists(), "watchdog ignored the renewed lease");
        let status = watchdog.wait().unwrap();
        assert!(status.success());
        assert!(!path.exists(), "watchdog did not clean the final expiry");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn current_user_pipe_acl_excludes_world_and_authenticated_users() {
        let sid = current_user_sid().unwrap();
        let sddl = format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid})");
        assert!(!sddl.contains(";;;WD"));
        assert!(!sddl.contains(";;;AU"));
        assert!(SecurityDescriptor::from_sddl(&sddl).is_ok());
    }

    #[test]
    fn current_user_pipe_can_be_bound_as_a_single_local_instance() {
        let sid = current_user_sid().unwrap();
        let name = format!(r"\\.\pipe\BubblePlay.StreamLab.test.{}", std::process::id());
        let server = PipeServer::bind_named_for_sid(&name, &sid).unwrap();
        assert!(PipeServer::bind_named_for_sid(&name, &sid).is_err());
        drop(server);
    }

    #[test]
    fn pipe_accept_is_cancelable_without_a_client() {
        let sid = current_user_sid().unwrap();
        let name = format!(
            r"\\.\pipe\BubblePlay.StreamLab.cancel.{}",
            std::process::id()
        );
        let server = PipeServer::bind_named_for_sid(&name, &sid).unwrap();
        let stop = Arc::new(AtomicBool::new(true));
        let started = Instant::now();
        assert!(server
            .accept_for_sid_cancelable(&sid, stop, Duration::from_secs(5))
            .unwrap()
            .is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn production_pipe_accept_is_genuinely_overlapped() {
        assert_ne!(pipe_open_mode() & FILE_FLAG_OVERLAPPED, 0);
    }

    #[test]
    fn connected_client_line_is_received_before_the_client_disconnects() {
        let sid = current_user_sid().unwrap();
        let name = format!(
            r"\\.\pipe\BubblePlay.StreamLab.roundtrip.{}.{}",
            std::process::id(),
            unix_ms(SystemTime::now()).unwrap()
        );
        let server = PipeServer::bind_named_for_sid(&name, &sid).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();
        let server_sid = sid.clone();
        let (line_tx, line_rx) = mpsc::sync_channel(1);
        let server_thread = thread::spawn(move || {
            let accepted = server
                .accept_for_sid_cancelable(&server_sid, server_stop, Duration::from_secs(5))
                .unwrap();
            let mut reader = BufReader::new(accepted.expect("client accepted"));
            let line = read_line_limited(&mut reader, MAX_COMMAND_BYTES).unwrap();
            let _ = line_tx.send(line);
        });

        const CLIENT_SCRIPT: &str = r#"$ErrorActionPreference='Stop'; $p=[IO.Pipes.NamedPipeClientStream]::new('.', $env:BUBBLEPLAY_TEST_PIPE, [IO.Pipes.PipeDirection]::InOut); $p.Connect(3000); $w=[IO.StreamWriter]::new($p); $w.AutoFlush=$true; $w.WriteLine('{"command":"ping"}'); Start-Sleep -Seconds 5"#;
        let powershell = system_directory()
            .unwrap()
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        let pipe_leaf = name.rsplit("\\").next().unwrap();
        let mut client = configured_powershell(
            &powershell,
            CLIENT_SCRIPT,
            &[("BUBBLEPLAY_TEST_PIPE", OsStr::new(pipe_leaf))],
        )
        .spawn()
        .unwrap();
        let received = line_rx.recv_timeout(Duration::from_secs(2));
        if received.is_err() {
            stop.store(true, Ordering::Release);
        }
        let _ = client.kill();
        let _ = client.wait();
        server_thread.join().unwrap();
        assert_eq!(received.unwrap(), Some(r#"{"command":"ping"}"#.to_owned()));
    }

    #[test]
    fn installed_data_acl_is_validated_not_rewritten_and_profile_is_service_readable() {
        const TRANSPORT_SID: &str = "S-1-5-80-36460983-23004156-1113575813-106319344-2112134846";
        assert!(VALIDATE_DATA_ACL_SCRIPT.contains(TRANSPORT_SID));
        assert!(VALIDATE_DATA_ACL_SCRIPT.contains("BUBBLEPLAY_LAB_OWNER"));
        assert!(VALIDATE_DATA_ACL_SCRIPT
            .contains("$e[$o]=[Security.AccessControl.FileSystemRights]::ReadAndExecute"));
        assert!(!VALIDATE_DATA_ACL_SCRIPT.contains("Set-Acl"));
        assert!(SET_PROFILE_ACL_SCRIPT.contains(TRANSPORT_SID));
        assert!(SET_PROFILE_ACL_SCRIPT.contains("FileSystemRights]::Read"));
        assert!(!SET_PROFILE_ACL_SCRIPT.contains("S-1-5-19"));
        assert!(VALIDATE_SERVICE_CONFIG_ACL_SCRIPT.contains("S-1-5-18"));
        assert!(VALIDATE_SERVICE_CONFIG_ACL_SCRIPT.contains("S-1-5-32-544"));
        assert!(!VALIDATE_SERVICE_CONFIG_ACL_SCRIPT.contains("Set-Acl"));
    }
}
