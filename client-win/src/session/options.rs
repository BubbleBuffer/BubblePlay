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
}

impl DevOptions {
    pub fn from_args(args: impl IntoIterator<Item = String>) -> Result<Option<Self>> {
        let args: Vec<_> = args.into_iter().collect();
        if !args.iter().any(|a| a == "--dev-wss") {
            return Ok(None);
        }
        let mut url = None;
        let mut codec = CodecChoice::Auto;
        let mut exit_after = None;
        let mut stats_path = None;
        let mut capture_path = None;
        let mut iter = args.into_iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
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
        Ok(Some(Self {
            url: url.context("missing --dev-wss URL")?,
            codec,
            exit_after,
            stats_path,
            capture_path,
        }))
    }
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
    }
    #[test]
    fn refuses_bad_or_missing_values() {
        for args in [
            vec!["--dev-wss"],
            vec!["--dev-wss", "ws://127.0.0.1", "--codec", "vp9"],
            vec!["--dev-wss", "ws://127.0.0.1", "--exit-after", "NaN"],
            vec!["--dev-wss", "ws://127.0.0.1", "--exit-after", "0"],
            vec!["--dev-wss", "ws://127.0.0.1", "--ui-demo"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }
}
