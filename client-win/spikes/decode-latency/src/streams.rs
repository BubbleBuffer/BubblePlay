use crate::backend::Codec;
use anyhow::{bail, Result};
pub fn access_units(data: &[u8], codec: Codec) -> Result<Vec<&[u8]>> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 4 < data.len() {
        let prefix = if data[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if data[i..].starts_with(&[0, 0, 1]) {
            3
        } else {
            i += 1;
            continue;
        };
        let nal = data[i + prefix];
        let aud = match codec {
            Codec::H264 => nal & 31 == 9,
            Codec::Hevc => (nal >> 1) & 63 == 35,
        };
        if aud {
            starts.push(i);
        }
        i += prefix + 1;
    }
    if starts.is_empty() {
        bail!("stream has no access-unit delimiters; use make-streams.ps1");
    }
    starts[0] = 0;
    starts.push(data.len());
    Ok(starts.windows(2).map(|r| &data[r[0]..r[1]]).collect())
}
