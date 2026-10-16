//! The recording's `evidence.json` (written by the decode-latency spike's
//! `make-streams.ps1`). The fake host takes its header dimensions, frame
//! rate and IDR layout from there and refuses a stream that disagrees.

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::stream::{Codec, Stream};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    pub file: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub frames: usize,
    pub gop: usize,
    pub bitrate_bps: u64,
    pub idr_au_indices: Vec<usize>,
}

pub fn load(dir: &Path, codec: Codec) -> Result<Evidence> {
    let path = dir.join("evidence.json");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    parse(&text, codec)
}

pub fn parse(text: &str, codec: Codec) -> Result<Evidence> {
    let root: Value = serde_json::from_str(text).context("evidence.json is not JSON")?;
    let expected = &root["expected"];
    let entry = root["streams"]
        .as_array()
        .context("evidence.json has no streams")?
        .iter()
        .find(|entry| entry["file"].as_str() == Some(codec.file_name()))
        .with_context(|| format!("evidence.json has no entry for {}", codec.file_name()))?;
    if entry["pass"].as_bool() != Some(true) {
        bail!("evidence.json marks {} as failed", codec.file_name());
    }
    let uint = |value: &Value, key: &str| -> Result<u64> {
        value[key]
            .as_u64()
            .with_context(|| format!("evidence {key} missing or not an integer"))
    };
    let idr_au_indices = entry["idr_au_indices"]
        .as_array()
        .context("evidence idr_au_indices missing")?
        .iter()
        .map(|v| {
            v.as_u64()
                .map(|n| n as usize)
                .context("idr index not an integer")
        })
        .collect::<Result<Vec<usize>>>()?;
    let evidence = Evidence {
        file: codec.file_name().to_owned(),
        width: uint(entry, "width")? as u32,
        height: uint(entry, "height")? as u32,
        fps: uint(expected, "fps")? as u32,
        frames: uint(entry, "decoded_frames")? as usize,
        gop: uint(expected, "gop")? as usize,
        bitrate_bps: uint(expected, "bitrate_bps")?,
        idr_au_indices,
    };
    if evidence.width != uint(expected, "width")? as u32
        || evidence.height != uint(expected, "height")? as u32
    {
        bail!("evidence stream dimensions disagree with its expected block");
    }
    Ok(evidence)
}

/// Fail unless the parsed stream matches what the recording's evidence says.
pub fn verify(stream: &Stream, evidence: &Evidence) -> Result<()> {
    if (stream.width, stream.height) != (evidence.width, evidence.height) {
        bail!(
            "{}: SPS is {}x{} but evidence.json says {}x{}",
            evidence.file,
            stream.width,
            stream.height,
            evidence.width,
            evidence.height
        );
    }
    if stream.len() != evidence.frames {
        bail!(
            "{}: {} access units but evidence.json says {}",
            evidence.file,
            stream.len(),
            evidence.frames
        );
    }
    if stream.idr_indices() != evidence.idr_au_indices {
        bail!("{}: IDR positions differ from evidence.json", evidence.file);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "expected": {"width": 1920, "height": 1080, "fps": 60, "frames": 1200, "gop": 120,
                   "idr_au_indices": [0, 120], "bitrate_bps": 20000000},
      "streams": [
        {"file": "h264.h264", "width": 1920, "height": 1080, "decoded_frames": 1200,
         "idr_au_indices": [0, 120], "pass": true}
      ]
    }"#;

    #[test]
    fn reads_the_stream_entry_and_expected_block() {
        let evidence = parse(SAMPLE, Codec::H264).unwrap();
        assert_eq!(
            (evidence.width, evidence.height, evidence.fps),
            (1920, 1080, 60)
        );
        assert_eq!(evidence.frames, 1200);
        assert_eq!(evidence.gop, 120);
        assert_eq!(evidence.bitrate_bps, 20_000_000);
        assert_eq!(evidence.idr_au_indices, vec![0, 120]);
    }

    #[test]
    fn refuses_a_missing_or_failed_entry() {
        assert!(parse(SAMPLE, Codec::Hevc).is_err());
        let failed = SAMPLE.replace(r#""pass": true"#, r#""pass": false"#);
        assert!(parse(&failed, Codec::H264).is_err());
    }

    #[test]
    fn refuses_entry_dimensions_that_disagree_with_expected() {
        let skewed = SAMPLE.replace(
            r#""width": 1920, "height": 1080, "decoded"#,
            r#""width": 1280, "height": 720, "decoded"#,
        );
        assert!(parse(&skewed, Codec::H264).is_err());
    }
}
