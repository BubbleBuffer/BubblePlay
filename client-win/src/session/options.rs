use std::{path::PathBuf, time::Duration};

use anyhow::{anyhow, bail, Context, Result};
use bp_device_key::DeviceId;

use crate::net::TransportChoice;

/// Upper bound for `--ice-timeout-ms`: a development override, never a wait for the UI.
pub const MAX_ICE_TIMEOUT_MS: u64 = 60_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CodecChoice {
    #[default]
    Auto,
    H264,
    Hevc,
}

/// Built-in synthetic input scripts. Each covers the packet types of one mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputScript {
    Work,
    Game,
}

/// Development-only connection controls. The regular UI/demo path keeps its
/// existing argument handling. The relay admits the viewer by its device key
/// (`--key` or the stored key); no credentials are accepted on the command line.
#[derive(Clone, Debug)]
pub struct DevOptions {
    pub url: String,
    pub codec: CodecChoice,
    pub exit_after: Option<Duration>,
    pub stats_path: Option<PathBuf>,
    pub capture_path: Option<PathBuf>,
    pub mode: crate::input::Mode,
    /// Separate decode-free input qualification window (VM only).
    pub input_only: bool,
    /// Decode-free, window-free WSS audio harness; fake sink unless explicitly selected.
    pub audio_only: bool,
    pub audio_wasapi: bool,
    pub audio_volume: f32,
    pub audio_muted: bool,
    /// Window-free local WASAPI tone test, reserved for VM qualification.
    pub audio_render_test: bool,
    /// Window-free hardware decoder capability probe.
    pub decoder_probe: bool,
    /// Transport for `--dev-wss`/`--dev-relay`; the legacy input and audio harnesses default to WSS.
    pub transport: TransportChoice,
    /// Development override of the Auto ICE checking deadline (1 to 60000 ms).
    pub ice_timeout: Option<Duration>,
    /// Window-free transport session with real decode; no shell or presentation.
    pub headless: bool,
    /// Explicit decode-free counters inside a headless session.
    pub count_only: bool,
    /// Synthetic input through InputLink; headless sessions only.
    pub input_script: Option<InputScript>,
    /// Exercise live monitor/mode controls with decoded readiness and synthetic input.
    pub session_script: bool,
    /// `--key <file.bpkey>`: a plain key file instead of the stored DPAPI-wrapped key.
    pub key: Option<PathBuf>,
    /// `--pc <id>`: the PC to reach through the relay (required with `--dev-relay`).
    pub pc: Option<DeviceId>,
    pub stats_out_path: Option<PathBuf>,
}

impl DevOptions {
    pub fn from_args(args: impl IntoIterator<Item = String>) -> Result<Option<Self>> {
        let args: Vec<_> = args.into_iter().collect();
        if !args.iter().any(|a| is_dev_flag(a)) {
            return Ok(None);
        }
        let mut url = None;
        let mut codec = CodecChoice::Auto;
        let mut exit_after = None;
        let mut stats_path = None;
        let mut capture_path = None;
        let mut mode = crate::input::Mode::Work;
        let mut input_only = false;
        let mut audio_only = false;
        let mut audio_wasapi = false;
        let mut audio_volume = 1.0f32;
        let mut audio_muted = false;
        let mut audio_render_test = false;
        let mut decoder_probe = false;
        let mut transport = None;
        let mut ice_timeout = None;
        let mut headless = false;
        let mut count_only = false;
        let mut input_script = None;
        let mut session_script = false;
        let mut relay = None;
        let mut key = None;
        let mut pc = None;
        let mut stats_out_path = None;
        let mut iter = args.into_iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--mode" => {
                    mode = match iter
                        .next()
                        .context("--mode requires work or game")?
                        .as_str()
                    {
                        "work" => crate::input::Mode::Work,
                        "game" => crate::input::Mode::Game,
                        value => bail!("unsupported mode: {value}"),
                    }
                }
                "--dev-input-only" => input_only = true,
                "--dev-audio-only" => audio_only = true,
                "--dev-audio-wasapi" => audio_wasapi = true,
                "--audio-mute" => audio_muted = true,
                "--audio-volume" => {
                    audio_volume = iter
                        .next()
                        .context("--audio-volume requires a value from 0 to 1")?
                        .parse()
                        .context("invalid audio volume")?;
                    if !audio_volume.is_finite() || !(0.0..=1.0).contains(&audio_volume) {
                        bail!("--audio-volume must be from 0 to 1");
                    }
                }
                "--dev-audio-render-test" => audio_render_test = true,
                "--dev-decoder-probe" => decoder_probe = true,
                "--dev-relay" => {
                    relay = Some(
                        iter.next()
                            .context("--dev-relay requires an http/https origin")?,
                    )
                }
                "--key" => {
                    key = Some(PathBuf::from(
                        iter.next().context("--key requires a .bpkey file")?,
                    ))
                }
                "--pc" => {
                    let id = iter.next().context("--pc requires a PC ID")?;
                    pc = Some(DeviceId::parse(&id).map_err(|_| anyhow!("invalid PC ID {id:?}"))?);
                }
                "--stats-out" => {
                    stats_out_path = Some(PathBuf::from(
                        iter.next().context("--stats-out requires a path")?,
                    ))
                }
                "--dev-wss" => url = Some(iter.next().context("--dev-wss requires a URL")?),
                "--codec" => {
                    codec = match iter
                        .next()
                        .context("--codec requires h264, hevc or auto")?
                        .as_str()
                    {
                        "auto" => CodecChoice::Auto,
                        "h264" => CodecChoice::H264,
                        "hevc" => CodecChoice::Hevc,
                        value => bail!("unsupported codec: {value}"),
                    };
                }
                "--exit-after" => {
                    let seconds: f64 = iter
                        .next()
                        .context("--exit-after requires seconds")?
                        .parse()
                        .context("invalid --exit-after seconds")?;
                    if !seconds.is_finite() || seconds <= 0.0 || seconds > 86400.0 {
                        bail!("--exit-after must be positive and at most 86400 seconds");
                    }
                    exit_after = Some(Duration::from_secs_f64(seconds));
                }
                "--stats" => {
                    stats_path = Some(PathBuf::from(
                        iter.next().context("--stats requires a file path")?,
                    ))
                }
                "--capture" => {
                    capture_path = Some(PathBuf::from(
                        iter.next().context("--capture requires a PNG file path")?,
                    ))
                }
                "--dev-headless" => headless = true,
                "--dev-session-script" => session_script = true,
                "--dev-count-only" => count_only = true,
                "--dev-input-script" => {
                    input_script = Some(
                        match iter
                            .next()
                            .context("--dev-input-script requires work or game")?
                            .as_str()
                        {
                            "work" => InputScript::Work,
                            "game" => InputScript::Game,
                            value => bail!("unsupported input script: {value}"),
                        },
                    )
                }
                "--transport" => {
                    transport = Some(
                        match iter
                            .next()
                            .context("--transport requires auto, webrtc or wss")?
                            .as_str()
                        {
                            "auto" => TransportChoice::Auto,
                            "webrtc" => TransportChoice::WebRtc,
                            "wss" => TransportChoice::Wss,
                            value => bail!("unsupported transport: {value}"),
                        },
                    )
                }
                "--ice-timeout-ms" => {
                    let millis: u64 = iter
                        .next()
                        .context("--ice-timeout-ms requires milliseconds")?
                        .parse()
                        .context("invalid --ice-timeout-ms")?;
                    if !(1..=MAX_ICE_TIMEOUT_MS).contains(&millis) {
                        bail!("--ice-timeout-ms must be from 1 to {MAX_ICE_TIMEOUT_MS}");
                    }
                    ice_timeout = Some(Duration::from_millis(millis));
                }
                "--ui-demo" | "--shots" => {
                    bail!("--dev-wss cannot be combined with UI demo options")
                }
                _ => bail!("unknown development argument: {arg}"),
            }
        }
        if let Some(origin) = relay {
            if url.is_some() {
                bail!("--dev-relay and --dev-wss are mutually exclusive");
            }
            if codec == CodecChoice::Hevc {
                bail!("--dev-relay supports --codec auto or h264");
            }
            if input_only || audio_only || audio_render_test || decoder_probe {
                bail!("development harnesses cannot be combined with --dev-relay");
            }
            if pc.is_none() {
                bail!("--dev-relay requires --pc <id>");
            }
            url = Some(relay_websocket_url(&origin)?);
        }
        if [
            input_only,
            audio_only,
            audio_render_test,
            decoder_probe,
            headless,
        ]
        .into_iter()
        .filter(|v| *v)
        .count()
            > 1
        {
            bail!("development harness modes are mutually exclusive");
        }
        if audio_wasapi && !audio_only {
            bail!("--dev-audio-wasapi requires --dev-audio-only");
        }
        if (audio_only || audio_render_test || decoder_probe || headless) && capture_path.is_some()
        {
            bail!("headless modes cannot capture a window");
        }
        let local_probe = audio_render_test || decoder_probe;
        if local_probe && (transport.is_some() || ice_timeout.is_some()) {
            bail!("local device probes take no --transport or --ice-timeout-ms");
        }
        if input_only && transport.is_some_and(|t| t != TransportChoice::Wss) {
            bail!("--dev-input-only is WSS-only");
        }
        if count_only && !headless {
            bail!("--dev-count-only requires --dev-headless");
        }
        if session_script
            && (!headless
                || count_only
                || input_script.is_some()
                || mode != crate::input::Mode::Work)
        {
            bail!("--dev-session-script requires decoded --dev-headless in Work and supplies its own synthetic input");
        }
        if count_only && input_script.is_some() {
            bail!(
                "--dev-input-script needs Decoded readiness and cannot run with --dev-count-only"
            );
        }
        if let Some(script) = input_script {
            if !headless {
                bail!("--dev-input-script requires --dev-headless");
            }
            let (name, required) = match script {
                InputScript::Work => ("work", crate::input::Mode::Work),
                InputScript::Game => ("game", crate::input::Mode::Game),
            };
            if mode != required {
                bail!("--dev-input-script {name} requires --mode {name}");
            }
        }
        let url = if local_probe {
            if url.is_some() {
                bail!("local device probes do not accept a host URL");
            }
            "ws://127.0.0.1".to_string()
        } else {
            url.context("missing --dev-wss URL")?
        };
        if headless {
            require_loopback_websocket(&url)?;
        }
        let transport = transport.unwrap_or(if input_only || audio_only {
            TransportChoice::Wss
        } else {
            TransportChoice::Auto
        });
        Ok(Some(Self {
            url,
            codec,
            exit_after,
            stats_path,
            capture_path,
            mode,
            input_only,
            audio_only,
            audio_wasapi,
            audio_volume,
            audio_muted,
            audio_render_test,
            decoder_probe,
            transport,
            ice_timeout,
            headless,
            count_only,
            input_script,
            session_script,
            key,
            pc,
            stats_out_path,
        }))
    }
}

/// The `ws`/`wss` URL for a `--dev-relay` origin: `https://host[:port]`, or `http://` for a
/// loopback relay only (the network layer refuses plaintext elsewhere as well).
fn relay_websocket_url(origin: &str) -> Result<String> {
    let mut parsed = url::Url::parse(origin)
        .map_err(|_| anyhow!("--dev-relay requires an http/https origin"))?;
    let loopback = match parsed.host() {
        Some(url::Host::Ipv4(ip)) => ip == std::net::Ipv4Addr::LOCALHOST,
        Some(url::Host::Ipv6(ip)) => ip == std::net::Ipv6Addr::LOCALHOST,
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        None => false,
    };
    let scheme = match parsed.scheme() {
        "https" => "wss",
        "http" if loopback => "ws",
        "http" => bail!("--dev-relay accepts http:// only for a loopback relay"),
        _ => bail!("--dev-relay requires an http/https origin"),
    };
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.path() != "/"
    {
        bail!("--dev-relay takes an origin without credentials, path, query or fragment");
    }
    parsed
        .set_scheme(scheme)
        .map_err(|_| anyhow!("cannot derive the WebSocket origin"))?;
    Ok(parsed.to_string())
}

/// Headless sessions connect only to loopback, checked before any network,
/// credential or device work starts.
pub(super) fn require_loopback_websocket(value: &str) -> Result<()> {
    let url = url::Url::parse(value).context("invalid --dev-wss URL")?;
    if !matches!(url.scheme(), "ws" | "wss") {
        bail!("headless sessions require a ws:// or wss:// URL");
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("headless URLs take no credentials, query or fragment");
    }
    let loopback = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip == std::net::Ipv4Addr::LOCALHOST,
        Some(url::Host::Ipv6(ip)) => ip == std::net::Ipv6Addr::LOCALHOST,
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        None => false,
    };
    if !loopback {
        bail!("headless sessions require a loopback host: 127.0.0.1, ::1 or localhost");
    }
    Ok(())
}

fn is_dev_flag(value: &str) -> bool {
    matches!(
        value,
        "--dev-wss"
            | "--dev-audio-only"
            | "--dev-audio-render-test"
            | "--dev-decoder-probe"
            | "--dev-headless"
            | "--dev-count-only"
            | "--dev-input-script"
            | "--dev-session-script"
            | "--transport"
            | "--ice-timeout-ms"
            | "--dev-relay"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(args: &[&str]) -> Result<Option<DevOptions>> {
        DevOptions::from_args(args.iter().map(|a| a.to_string()))
    }
    #[test]
    fn demo_is_untouched_and_auto_is_default() {
        assert!(parse(&["--ui-demo", "--shots", "shots"]).unwrap().is_none());
        assert_eq!(
            parse(&["--dev-wss", "ws://127.0.0.1:9876"])
                .unwrap()
                .unwrap()
                .codec,
            CodecChoice::Auto
        );
        assert_eq!(
            parse(&["--dev-wss", "ws://127.0.0.1"])
                .unwrap()
                .unwrap()
                .mode,
            crate::input::Mode::Work
        );
    }
    #[test]
    fn explicit_codec_and_bounded_exit() {
        let options = parse(&[
            "--dev-wss",
            "ws://127.0.0.1:9876",
            "--codec",
            "hevc",
            "--exit-after",
            "30",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(options.codec, CodecChoice::Hevc);
        assert_eq!(options.exit_after, Some(Duration::from_secs(30)));
        assert_eq!(
            parse(&["--dev-wss", "ws://127.0.0.1", "--mode", "game"])
                .unwrap()
                .unwrap()
                .mode,
            crate::input::Mode::Game
        );
    }
    #[test]
    fn refuses_bad_or_missing_values() {
        for args in [
            vec!["--dev-wss"],
            vec!["--dev-wss", "ws://127.0.0.1", "--codec", "vp9"],
            vec!["--dev-wss", "ws://127.0.0.1", "--exit-after", "NaN"],
            vec!["--dev-wss", "ws://127.0.0.1", "--exit-after", "0"],
            vec!["--dev-wss", "ws://127.0.0.1", "--ui-demo"],
            vec!["--dev-wss", "ws://127.0.0.1", "--mode", "other"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn headless_modes_are_explicit_and_cannot_start_input_or_capture_a_window() {
        let fake = parse(&[
            "--dev-wss",
            "ws://127.0.0.1:19764",
            "--dev-audio-only",
            "--stats-out",
            "audio.json",
        ])
        .unwrap()
        .unwrap();
        assert!(fake.audio_only);
        assert!(!fake.audio_wasapi && !fake.input_only);
        assert_eq!(fake.stats_out_path, Some(PathBuf::from("audio.json")));
        assert!(
            parse(&["--dev-decoder-probe", "--stats-out", "probe.json"])
                .unwrap()
                .unwrap()
                .decoder_probe
        );
        assert!(
            parse(&["--dev-audio-render-test"])
                .unwrap()
                .unwrap()
                .audio_render_test
        );
        for args in [
            vec![
                "--dev-wss",
                "ws://127.0.0.1",
                "--dev-audio-only",
                "--dev-input-only",
            ],
            vec![
                "--dev-wss",
                "ws://127.0.0.1",
                "--dev-audio-only",
                "--capture",
                "bad.png",
            ],
            vec!["--dev-decoder-probe", "--dev-wss", "ws://127.0.0.1"],
            vec!["--dev-audio-only", "--dev-audio-render-test"],
            vec!["--dev-wss", "ws://127.0.0.1", "--dev-audio-wasapi"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn audio_volume_and_mute_are_local_bounded_controls() {
        let options = parse(&[
            "--dev-wss",
            "ws://127.0.0.1",
            "--audio-volume",
            "0.25",
            "--audio-mute",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(options.audio_volume, 0.25);
        assert!(options.audio_muted);
        for value in ["NaN", "inf", "-0.1", "1.1", "bad"] {
            assert!(parse(&["--dev-wss", "ws://127.0.0.1", "--audio-volume", value]).is_err());
        }
    }
    #[test]
    fn dev_relay_derives_the_websocket_url_and_needs_a_pc() {
        const PC: &str = "77VH-7M56-8GJK-Y12J";
        for (origin, websocket) in [
            ("http://127.0.0.1:8788", "ws://127.0.0.1:8788/"),
            ("https://localhost:8788", "wss://localhost:8788/"),
            ("https://relay.example", "wss://relay.example/"),
        ] {
            let options = parse(&[
                "--dev-relay",
                origin,
                "--pc",
                &PC.to_lowercase(),
                "--key",
                "viewer.bpkey",
                "--stats-out",
                "stats.json",
            ])
            .unwrap()
            .unwrap();
            assert_eq!(options.url, websocket);
            assert_eq!(options.pc.unwrap().to_string(), PC);
            assert_eq!(options.key, Some(PathBuf::from("viewer.bpkey")));
            assert_eq!(options.stats_out_path, Some(PathBuf::from("stats.json")));
        }
        assert!(is_dev_flag("--dev-relay"));
        assert!(parse(&["--dev-relay", "https://relay.example"]).is_err());
        assert!(parse(&["--dev-relay", "http://relay.example", "--pc", PC]).is_err());
        assert!(parse(&["--dev-relay", "https://relay.example/path", "--pc", PC]).is_err());
        assert!(parse(&["--dev-relay", "https://relay.example", "--pc", "not-an-id"]).is_err());
        assert!(parse(&[
            "--dev-relay",
            "https://relay.example",
            "--pc",
            PC,
            "--codec",
            "hevc"
        ])
        .is_err());
        assert!(parse(&[
            "--dev-relay",
            "https://relay.example",
            "--pc",
            PC,
            "--dev-wss",
            "ws://127.0.0.1:8788"
        ])
        .is_err());
        // --pc is optional for the loopback fake host.
        let options = parse(&["--dev-wss", "ws://127.0.0.1"]).unwrap().unwrap();
        assert_eq!((options.pc, options.key), (None, None));
    }

    #[test]
    fn transport_and_ice_knobs_are_explicit_bounded_and_harness_scoped() {
        let normal = parse(&["--dev-wss", "ws://127.0.0.1:19764"])
            .unwrap()
            .unwrap();
        assert_eq!(normal.transport, TransportChoice::Auto);
        assert_eq!(normal.ice_timeout, None);
        for (value, expected) in [
            ("auto", TransportChoice::Auto),
            ("webrtc", TransportChoice::WebRtc),
            ("wss", TransportChoice::Wss),
        ] {
            let options = parse(&["--dev-wss", "ws://127.0.0.1", "--transport", value])
                .unwrap()
                .unwrap();
            assert_eq!(options.transport, expected);
        }
        // Legacy harnesses stay on WSS unless the caller asks otherwise.
        let audio = parse(&["--dev-audio-only", "--dev-wss", "ws://127.0.0.1"])
            .unwrap()
            .unwrap();
        assert_eq!(audio.transport, TransportChoice::Wss);
        let audio_webrtc = parse(&[
            "--dev-audio-only",
            "--dev-wss",
            "ws://127.0.0.1",
            "--transport",
            "webrtc",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(audio_webrtc.transport, TransportChoice::WebRtc);
        for (value, millis) in [("1", 1), ("60000", 60_000), ("+250", 250)] {
            let options = parse(&["--dev-wss", "ws://127.0.0.1", "--ice-timeout-ms", value])
                .unwrap()
                .unwrap();
            assert_eq!(options.ice_timeout, Some(Duration::from_millis(millis)));
        }
        for value in ["0", "60001", "-1", "1.5", "bad", ""] {
            assert!(
                parse(&["--dev-wss", "ws://127.0.0.1", "--ice-timeout-ms", value]).is_err(),
                "{value}"
            );
        }
        for args in [
            vec!["--dev-wss", "ws://127.0.0.1", "--transport", "tcp"],
            vec!["--dev-wss", "ws://127.0.0.1", "--transport"],
            vec!["--dev-wss", "ws://127.0.0.1", "--ice-timeout-ms"],
            // Without a host URL, these knobs would silently configure nothing.
            vec!["--transport", "wss"],
            vec!["--ice-timeout-ms", "5"],
            vec!["--dev-decoder-probe", "--transport", "wss"],
            vec!["--dev-audio-render-test", "--ice-timeout-ms", "5"],
            vec![
                "--dev-wss",
                "ws://127.0.0.1",
                "--dev-input-only",
                "--transport",
                "webrtc",
            ],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn headless_is_loopback_only_and_refuses_window_and_harness_mixing() {
        let ok = parse(&[
            "--dev-headless",
            "--dev-wss",
            "ws://127.0.0.1:19764",
            "--stats-out",
            "headless.json",
        ])
        .unwrap()
        .unwrap();
        assert!(ok.headless && !ok.count_only);
        assert_eq!(ok.input_script, None);
        assert_eq!(ok.transport, TransportChoice::Auto);
        for url in [
            "ws://[::1]:19764",
            "ws://localhost:19764",
            "wss://localhost:443",
        ] {
            assert!(
                parse(&["--dev-headless", "--dev-wss", url]).is_ok(),
                "{url}"
            );
        }
        for url in [
            "wss://example.com",
            "ws://192.168.1.2:19764",
            "ws://127.0.0.2:19764",
            "http://127.0.0.1:19764",
            "ws://user@127.0.0.1:19764",
            "ws://127.0.0.1:19764/?x=1",
        ] {
            assert!(
                parse(&["--dev-headless", "--dev-wss", url]).is_err(),
                "{url}"
            );
        }
        assert!(parse(&["--dev-headless"]).is_err());
        for args in [
            vec![
                "--dev-headless",
                "--dev-wss",
                "ws://127.0.0.1",
                "--capture",
                "x.png",
            ],
            vec![
                "--dev-headless",
                "--dev-wss",
                "ws://127.0.0.1",
                "--dev-audio-only",
            ],
            vec![
                "--dev-headless",
                "--dev-wss",
                "ws://127.0.0.1",
                "--dev-input-only",
            ],
            vec!["--dev-headless", "--dev-decoder-probe"],
            vec!["--dev-headless", "--dev-audio-render-test"],
            // Count-only is decode-free and must be asked for explicitly.
            vec!["--dev-count-only", "--dev-wss", "ws://127.0.0.1"],
            // Synthetic input is headless-only and must match the mode.
            vec!["--dev-input-script", "work", "--dev-wss", "ws://127.0.0.1"],
            vec![
                "--dev-headless",
                "--dev-wss",
                "ws://127.0.0.1",
                "--dev-input-script",
                "game",
            ],
            vec![
                "--dev-headless",
                "--dev-wss",
                "ws://127.0.0.1",
                "--dev-input-script",
                "fire",
            ],
            // Input needs Decoded readiness, which a count-only run never reaches.
            vec![
                "--dev-headless",
                "--dev-count-only",
                "--dev-wss",
                "ws://127.0.0.1",
                "--dev-input-script",
                "work",
            ],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
        let count = parse(&[
            "--dev-headless",
            "--dev-count-only",
            "--dev-wss",
            "ws://127.0.0.1",
        ])
        .unwrap()
        .unwrap();
        assert!(count.count_only);
        let work = parse(&[
            "--dev-headless",
            "--dev-wss",
            "ws://127.0.0.1",
            "--dev-input-script",
            "work",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(work.input_script, Some(InputScript::Work));
        let game = parse(&[
            "--dev-headless",
            "--dev-wss",
            "ws://127.0.0.1",
            "--mode",
            "game",
            "--dev-input-script",
            "game",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(game.input_script, Some(InputScript::Game));
    }

    #[test]
    fn live_controls_script_requires_decoded_loopback_headless() {
        let options = parse(&[
            "--dev-wss",
            "ws://127.0.0.1:19764",
            "--dev-headless",
            "--dev-session-script",
        ])
        .unwrap()
        .unwrap();
        assert!(options.session_script);
        for extra in [
            vec![],
            vec!["--dev-headless", "--dev-count-only"],
            vec!["--dev-headless", "--dev-input-script", "work"],
        ] {
            let mut args = vec!["--dev-wss", "ws://127.0.0.1", "--dev-session-script"];
            args.extend(extra);
            assert!(parse(&args).is_err());
        }
        assert!(parse(&[
            "--dev-wss",
            "wss://example.com",
            "--dev-headless",
            "--dev-session-script",
        ])
        .is_err());
    }

    #[test]
    fn headless_accepts_loopback_dev_relay_and_refuses_remote_origins() {
        let options = parse(&[
            "--dev-headless",
            "--dev-relay",
            "http://127.0.0.1:8788",
            "--pc",
            "77VH-7M56-8GJK-Y12J",
        ])
        .unwrap()
        .unwrap();
        assert!(options.headless && options.pc.is_some());
        assert!(parse(&[
            "--dev-headless",
            "--dev-relay",
            "https://relay.example",
            "--pc",
            "77VH-7M56-8GJK-Y12J",
        ])
        .is_err());
    }
}
