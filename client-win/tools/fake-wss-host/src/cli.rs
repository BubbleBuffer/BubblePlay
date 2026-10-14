//! Command-line parsing. Every bad value is an error, never a panic: numbers
//! are range-checked before they become durations, and the injection window
//! is computed with checked arithmetic.

use std::path::PathBuf;
use std::time::Duration;

use crate::model::Injections;
use crate::stream::Codec;

pub const DEFAULT_STREAMS_DIR: &str = "F:/build/spike-streams";
/// The longest run and the latest injection time, in seconds.
pub const MAX_SECONDS: f64 = 3600.0;

pub const USAGE: &str = "\
usage: fake-wss-host [--stream h264|hevc] [options]

  --stream h264|hevc        only serve this recording (default: every recording
                            found, and the codec is chosen by media-control start)
  --streams-dir DIR         spike streams and evidence.json (default F:/build/spike-streams)
  --exit-after SECONDS      stop after this long, 0 to 3600 (default 30)
  --drain-ms MS             stop sending new frames this long before exit (default 1000)
  --port PORT               loopback port, 0 = any free port (default 8787)
  --seq-gap-at SECONDS      skip one sequence number once, at or after this time
  --generation-at SECONDS   bump the generation once, at or after this time
  --socket-drop-at SECONDS  close both sockets once; the next hello is a new epoch
  --stats PATH              also write the JSON report to PATH
  --input-record PATH       write each binary message received on /ws as a JSON line

Injections must fire at least one second before the drain window ends.
The JSON report goes to stdout. Listen address and progress go to stderr.
Exit codes: 0 every check passed, 1 a check failed, 2 usage or startup error.";

#[derive(Debug, Clone)]
pub struct Args {
    /// Only serve this codec; `None` serves every recording found.
    pub constraint: Option<Codec>,
    pub streams_dir: PathBuf,
    pub exit_after: Duration,
    pub drain_ms: u64,
    pub port: u16,
    pub injections: Injections,
    pub stats: Option<PathBuf>,
    /// Where binary inputs received on `/ws` are recorded, one JSON line each.
    pub input_record: Option<PathBuf>,
}

/// `Ok(None)` for `--help`.
pub fn parse(raw: &[String]) -> Result<Option<Args>, String> {
    let mut constraint = None;
    let mut streams_dir = PathBuf::from(DEFAULT_STREAMS_DIR);
    let mut exit_after = Duration::from_secs(30);
    let mut drain_ms = 1000u64;
    let mut port = 8787u16;
    let mut injections = Injections::default();
    let mut stats = None;
    let mut input_record = None;

    let mut iter = raw.iter();
    while let Some(flag) = iter.next() {
        if matches!(flag.as_str(), "-h" | "--help") {
            return Ok(None);
        }
        let mut value = || {
            iter.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag.as_str() {
            "--stream" => {
                let v = value()?;
                constraint = Some(
                    Codec::parse(&v)
                        .ok_or_else(|| format!("unknown stream '{v}' (use h264 or hevc)"))?,
                );
            }
            "--streams-dir" => streams_dir = PathBuf::from(value()?),
            "--exit-after" => exit_after = seconds("--exit-after", &value()?)?,
            "--drain-ms" => {
                drain_ms = value()?
                    .parse()
                    .map_err(|_| "--drain-ms needs a whole number".to_owned())?
            }
            "--port" => {
                port = value()?
                    .parse()
                    .map_err(|_| "--port needs 0 to 65535".to_owned())?
            }
            "--seq-gap-at" => injections.seq_gap_at = Some(seconds("--seq-gap-at", &value()?)?),
            "--generation-at" => {
                injections.generation_at = Some(seconds("--generation-at", &value()?)?)
            }
            "--socket-drop-at" => {
                injections.socket_drop_at = Some(seconds("--socket-drop-at", &value()?)?)
            }
            "--stats" => stats = Some(PathBuf::from(value()?)),
            "--input-record" => input_record = Some(PathBuf::from(value()?)),
            other => return Err(format!("unknown option {other}")),
        }
    }

    if exit_after.is_zero() {
        return Err("--exit-after must be above 0 seconds".into());
    }
    let drain = Duration::from_millis(drain_ms);
    if drain >= exit_after {
        return Err(format!(
            "--drain-ms ({drain_ms}) must be shorter than --exit-after"
        ));
    }
    // Injections must fire before the drain window, with one second to spare.
    let tail = drain.saturating_add(Duration::from_secs(1));
    let latest = exit_after.checked_sub(tail);
    for (name, at) in [
        ("--seq-gap-at", injections.seq_gap_at),
        ("--generation-at", injections.generation_at),
        ("--socket-drop-at", injections.socket_drop_at),
    ] {
        let Some(at) = at else { continue };
        if !latest.is_some_and(|latest| at <= latest) {
            let latest_s = latest.map_or(0.0, |l| l.as_secs_f64());
            return Err(format!(
                "{name} must be at most {latest_s:.3} s for a {:.3} s run with --drain-ms {drain_ms}",
                exit_after.as_secs_f64()
            ));
        }
    }
    Ok(Some(Args {
        constraint,
        streams_dir,
        exit_after,
        drain_ms,
        port,
        injections,
        stats,
        input_record,
    }))
}

/// A number of seconds in `0..=MAX_SECONDS`. Anything else, including NaN,
/// infinity and huge values, is refused before it reaches `Duration`.
pub fn seconds(name: &str, value: &str) -> Result<Duration, String> {
    let parsed: f64 = value
        .parse()
        .map_err(|_| format!("{name} needs a number of seconds"))?;
    if !parsed.is_finite() || !(0.0..=MAX_SECONDS).contains(&parsed) {
        return Err(format!(
            "{name} must be between 0 and {MAX_SECONDS} seconds"
        ));
    }
    Ok(Duration::from_secs_f64(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Result<Option<Args>, String> {
        parse(&list.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    fn ok(list: &[&str]) -> Args {
        args(list).expect("parses").expect("not help")
    }

    #[test]
    fn with_no_stream_flag_every_recording_is_on_offer() {
        let parsed = ok(&["--exit-after", "5"]);
        assert!(parsed.constraint.is_none());
        assert_eq!(parsed.exit_after, Duration::from_secs(5));
        assert_eq!(parsed.drain_ms, 1000);
        assert_eq!(parsed.port, 8787);
    }

    #[test]
    fn a_stream_flag_constrains_the_run_to_one_codec() {
        assert!(matches!(
            ok(&["--stream", "hevc"]).constraint,
            Some(Codec::Hevc)
        ));
        assert!(matches!(
            ok(&["--stream", "h264"]).constraint,
            Some(Codec::H264)
        ));
        assert!(args(&["--stream", "vp9"]).is_err());
        assert!(
            args(&["--stream"]).is_err(),
            "a flag with no value is an error"
        );
    }

    #[test]
    fn help_is_not_an_error() {
        assert!(matches!(args(&["--help"]), Ok(None)));
        assert!(matches!(args(&["-h"]), Ok(None)));
    }

    #[test]
    fn seconds_refuse_nonsense_without_panicking() {
        for bad in [
            "1e300", "1e308", "nan", "NaN", "inf", "-inf", "-1", "3600.5", "abc", "",
        ] {
            assert!(
                seconds("--exit-after", bad).is_err(),
                "{bad} must be refused"
            );
        }
        assert_eq!(seconds("--exit-after", "0").unwrap(), Duration::ZERO);
        assert_eq!(
            seconds("--exit-after", "3600").unwrap(),
            Duration::from_secs(3600)
        );
        assert_eq!(
            seconds("--exit-after", "2.5").unwrap(),
            Duration::from_millis(2500)
        );
    }

    #[test]
    fn exit_after_zero_and_huge_are_refused() {
        assert!(args(&["--exit-after", "0"]).is_err());
        assert!(args(&["--exit-after", "1e300"]).is_err());
        assert!(args(&["--exit-after", "99999999999999999999999"]).is_err());
        assert!(args(&["--exit-after", "3601"]).is_err());
    }

    #[test]
    fn drain_at_or_past_the_run_is_refused_without_panicking() {
        assert!(args(&["--exit-after", "2", "--drain-ms", "2000"]).is_err());
        assert!(args(&["--exit-after", "30", "--drain-ms", "18446744073709551615"]).is_err());
        assert!(args(&[
            "--exit-after",
            "30",
            "--drain-ms",
            "99999999999999999999999"
        ])
        .is_err());
        assert!(args(&["--exit-after", "30", "--drain-ms", "-5"]).is_err());
    }

    #[test]
    fn injections_must_leave_a_second_before_the_drain_window() {
        // 3 s run, 1 s drain: an injection needs to be at or before 1.0 s.
        assert!(args(&["--exit-after", "3", "--socket-drop-at", "2.5"]).is_err());
        assert!(args(&["--exit-after", "3", "--socket-drop-at", "1.0"]).is_ok());
        // A run shorter than the tail cannot carry any injection, and must not panic.
        assert!(args(&[
            "--exit-after",
            "0.5",
            "--drain-ms",
            "100",
            "--seq-gap-at",
            "0.1"
        ])
        .is_err());
        assert!(args(&[
            "--exit-after",
            "1.5",
            "--drain-ms",
            "100",
            "--generation-at",
            "0.2"
        ])
        .is_ok());
        // Without injections the same short run is fine.
        assert!(args(&["--exit-after", "0.5", "--drain-ms", "100"]).is_ok());
    }

    #[test]
    fn bad_ports_and_unknown_flags_are_errors() {
        assert!(args(&["--port", "70000"]).is_err());
        assert_eq!(ok(&["--port", "0"]).port, 0);
        assert!(args(&["--bogus"]).is_err());
    }

    #[test]
    fn stats_path_is_kept() {
        assert_eq!(
            ok(&["--stats", "out.json"]).stats.as_deref(),
            Some(std::path::Path::new("out.json"))
        );
    }

    #[test]
    fn input_record_path_is_kept_and_needs_a_value() {
        assert!(ok(&[]).input_record.is_none());
        assert_eq!(
            ok(&["--input-record", "inputs.jsonl"]).input_record.as_deref(),
            Some(std::path::Path::new("inputs.jsonl"))
        );
        assert!(args(&["--input-record"]).is_err());
    }
}
