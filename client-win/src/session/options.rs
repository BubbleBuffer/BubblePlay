use std::{path::PathBuf, time::Duration};

use anyhow::{bail, Context, Result};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CodecChoice {
    #[default]
    Auto,
    H264,
    Hevc,
}

/// Development-only connection controls. The regular UI/demo path keeps its
/// existing argument handling; no credentials are accepted on the command line.
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
    #[cfg(feature = "dev-login")]
    pub login: Option<super::dev_login::DevLogin>,
    #[cfg(feature = "dev-login")]
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
        #[cfg(feature = "dev-login")]
        let mut relay = None;
        #[cfg(feature = "dev-login")]
        let mut password_file = None;
        #[cfg(feature = "dev-login")]
        let mut provision_file = None;
        #[cfg(feature = "dev-login")]
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
                #[cfg(feature = "dev-login")]
                "--dev-relay" => {
                    relay = Some(
                        iter.next()
                            .context("--dev-relay requires an http/https origin")?,
                    )
                }
                #[cfg(feature = "dev-login")]
                "--password-file" => {
                    password_file = Some(PathBuf::from(
                        iter.next().context("--password-file requires a path")?,
                    ))
                }
                #[cfg(feature = "dev-login")]
                "--totp-provision" => {
                    provision_file = Some(PathBuf::from(
                        iter.next().context("--totp-provision requires a path")?,
                    ))
                }
                #[cfg(feature = "dev-login")]
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
                "--ui-demo" | "--shots" => {
                    bail!("--dev-wss cannot be combined with UI demo options")
                }
                _ => bail!("unknown development argument: {arg}"),
            }
        }
        #[cfg(feature = "dev-login")]
        let login = if let Some(origin) = relay {
            if url.is_some() {
                bail!("--dev-relay and --dev-wss are mutually exclusive");
            }
            if codec == CodecChoice::Hevc {
                bail!("--dev-relay supports --codec auto or h264");
            }
            if input_only {
                bail!("--dev-input-only is a fake-host fixture, not an authenticated relay path");
            }
            let mut parsed = super::dev_login::validate_origin(&origin)?;
            let normalized = parsed.origin().ascii_serialization();
            let scheme = if parsed.scheme() == "https" {
                "wss"
            } else {
                "ws"
            };
            parsed
                .set_scheme(scheme)
                .map_err(|_| anyhow::anyhow!("cannot derive WebSocket origin"))?;
            url = Some(parsed.to_string());
            Some(super::dev_login::DevLogin {
                origin: normalized,
                password_file: password_file.context("--dev-relay requires --password-file")?,
                provision_file: provision_file.context("--dev-relay requires --totp-provision")?,
            })
        } else {
            if password_file.is_some() || provision_file.is_some() {
                bail!("credential files require --dev-relay");
            }
            None
        };
        Ok(Some(Self {
            url: url.context("missing --dev-wss URL")?,
            codec,
            exit_after,
            stats_path,
            capture_path,
            mode,
            input_only,
            #[cfg(feature = "dev-login")]
            login,
            #[cfg(feature = "dev-login")]
            stats_out_path,
        }))
    }
}

fn is_dev_flag(value: &str) -> bool {
    if value == "--dev-wss" {
        return true;
    }
    #[cfg(feature = "dev-login")]
    if value == "--dev-relay" {
        return true;
    }
    false
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
    #[cfg(feature = "dev-login")]
    fn authenticated_relay_derives_websocket_scheme_without_reading_credentials() {
        for (origin, websocket) in [
            ("http://127.0.0.1:8788", "ws://127.0.0.1:8788/"),
            ("https://localhost:8788", "wss://localhost:8788/"),
        ] {
            let options = parse(&[
                "--dev-relay",
                origin,
                "--password-file",
                "fake-password",
                "--totp-provision",
                "fake-provision",
                "--stats-out",
                "stats.json",
            ])
            .unwrap()
            .unwrap();
            assert_eq!(options.url, websocket);
            assert_eq!(options.login.unwrap().origin, origin);
            assert_eq!(options.stats_out_path, Some(PathBuf::from("stats.json")));
        }
        assert!(parse(&["--dev-relay", "http://127.0.0.1:8788"]).is_err());
        assert!(parse(&[
            "--dev-relay",
            "http://127.0.0.1:8788",
            "--password-file",
            "p",
            "--totp-provision",
            "t",
            "--codec",
            "hevc"
        ])
        .is_err());
        assert!(parse(&["--dev-wss", "ws://127.0.0.1:8788", "--password-file", "p"]).is_err());
    }
    #[cfg(not(feature = "dev-login"))]
    #[test]
    fn dev_login_module_and_flags_are_disabled_by_default() {
        assert!(!cfg!(feature = "dev-login"));
        // The auth module declaration and its option fields are cfg-gated;
        // compiling this default build cannot resolve or include that module.
        assert!(!is_dev_flag("--dev-relay"));
        assert!(parse(&["--dev-relay", "http://127.0.0.1:8788"])
            .unwrap()
            .is_none());
        assert!(parse(&["--dev-wss", "ws://127.0.0.1", "--password-file", "p"]).is_err());
        let options = parse(&["--dev-wss", "ws://127.0.0.1"]).unwrap().unwrap();
        let DevOptions {
            url: _,
            codec: _,
            exit_after: _,
            stats_path: _,
            capture_path: _,
            mode: _,
            input_only: _,
        } = options;
    }
}
