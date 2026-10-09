//! Bounded conversion of Windows packed DIB clipboard images to browser PNG.
use anyhow::{bail, Context, Result};

pub const MAX_PNG_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_PNG_WIRE_BYTES: usize = MAX_PNG_BYTES.div_ceil(3) * 4;

struct BoundedPngWriter(Vec<u8>);
impl std::io::Write for BoundedPngWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_PNG_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("PNG clipboard exceeds encoded limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn valid_png(bytes: &[u8]) -> bool {
    if bytes.len() < 33
        || bytes.len() > MAX_PNG_BYTES
        || &bytes[..8] != b"\x89PNG\r\n\x1a\n"
        || bytes[8..12] != 13u32.to_be_bytes()
        || &bytes[12..16] != b"IHDR"
    {
        return false;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
    width > 0
        && height > 0
        && u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|pixels| pixels.checked_mul(4))
            .is_some_and(|bytes| bytes <= MAX_IMAGE_BYTES as u64)
}

pub fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>> {
    if dib.len() < 40 {
        bail!("truncated DIB header");
    }
    let u32_at = |at: usize| u32::from_le_bytes(dib[at..at + 4].try_into().unwrap());
    let header = u32_at(0) as usize;
    if !matches!(header, 40 | 108 | 124) || header > dib.len() {
        bail!("unsupported DIB header");
    }
    let width = u32_at(4) as i32;
    let signed_height = u32_at(8) as i32;
    let height = signed_height.unsigned_abs();
    let bits = u16::from_le_bytes(dib[14..16].try_into().unwrap());
    let compression = u32_at(16);
    if width <= 0
        || height == 0
        || dib[12..14] != 1u16.to_le_bytes()
        || !matches!(bits, 24 | 32)
        || (compression != 0 && !(compression == 3 && bits == 32))
    {
        bail!("unsupported DIB dimensions or pixel format");
    }
    let width = width as usize;
    let height = height as usize;
    let rgba_len = width
        .checked_mul(height)
        .and_then(|n| n.checked_mul(4))
        .context("DIB dimensions overflow")?;
    if rgba_len > MAX_IMAGE_BYTES {
        bail!("DIB image exceeds pixel limit");
    }
    let masks = if compression == 3 {
        let start = if header == 40 { header } else { 40 };
        if start + 12 > dib.len() {
            bail!("truncated DIB masks");
        }
        let alpha = if header >= 108 { u32_at(52) } else { 0 };
        if [u32_at(start), u32_at(start + 4), u32_at(start + 8)]
            != [0x00ff0000, 0x0000ff00, 0x000000ff]
            || !matches!(alpha, 0 | 0xff000000)
        {
            bail!("unsupported DIB channel masks");
        }
        alpha != 0
    } else {
        false
    };
    if header == 124 && (u32_at(112) != 0 || u32_at(116) != 0) {
        bail!("DIB embedded color profiles are unsupported");
    }
    let colors = u32_at(32) as usize;
    let offset = header
        .checked_add(if compression == 3 && header == 40 {
            12
        } else {
            0
        })
        .and_then(|n| colors.checked_mul(4).and_then(|c| n.checked_add(c)))
        .context("DIB offset overflow")?;
    let stride = (width * bits as usize).div_ceil(32) * 4;
    let end = offset
        .checked_add(stride.checked_mul(height).context("DIB size overflow")?)
        .context("DIB size overflow")?;
    if end > dib.len() {
        bail!("truncated DIB pixels");
    }
    let mut rgba = vec![0; rgba_len];
    for y in 0..height {
        let source_y = if signed_height < 0 { y } else { height - 1 - y };
        let row = &dib[offset + source_y * stride..];
        for x in 0..width {
            let source = x * (bits as usize / 8);
            let target = (y * width + x) * 4;
            rgba[target..target + 4].copy_from_slice(&[
                row[source + 2],
                row[source + 1],
                row[source],
                if masks { row[source + 3] } else { 255 },
            ]);
        }
    }
    let mut out = BoundedPngWriter(Vec::new());
    {
        let mut encoder = png::Encoder::new(&mut out, width as u32, height as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        encoder.write_header()?.write_image_data(&rgba)?;
    }
    Ok(out.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn dib(height: i32, bits: u16, pixels: &[u8]) -> Vec<u8> {
        let mut out = vec![0; 40];
        out[..4].copy_from_slice(&40u32.to_le_bytes());
        out[4..8].copy_from_slice(&1i32.to_le_bytes());
        out[8..12].copy_from_slice(&height.to_le_bytes());
        out[12..14].copy_from_slice(&1u16.to_le_bytes());
        out[14..16].copy_from_slice(&bits.to_le_bytes());
        out.extend_from_slice(pixels);
        out
    }
    fn pixels(png: &[u8]) -> Vec<u8> {
        let mut reader = png::Decoder::new(std::io::Cursor::new(png))
            .read_info()
            .unwrap();
        let mut bytes = vec![0; reader.output_buffer_size().unwrap()];
        reader.next_frame(&mut bytes).unwrap();
        bytes
    }
    #[test]
    fn bottom_up_padded_bgr_becomes_top_down_rgba() {
        let png = dib_to_png(&dib(2, 24, &[255, 0, 0, 0, 0, 0, 255, 0])).unwrap();
        assert!(valid_png(&png));
        assert_eq!(pixels(&png), [255, 0, 0, 255, 0, 0, 255, 255]);
    }
    #[test]
    fn top_down_rgb32_unused_alpha_is_opaque() {
        assert_eq!(
            pixels(&dib_to_png(&dib(-1, 32, &[3, 2, 1, 0])).unwrap()),
            [1, 2, 3, 255]
        );
    }
    #[test]
    fn v5_explicit_alpha_is_preserved() {
        let mut image = dib(-1, 32, &[]);
        image.resize(124, 0);
        image[..4].copy_from_slice(&124u32.to_le_bytes());
        image[16..20].copy_from_slice(&3u32.to_le_bytes());
        for (i, mask) in [0x00ff0000u32, 0x0000ff00, 0x000000ff, 0xff000000]
            .iter()
            .enumerate()
        {
            image[40 + i * 4..44 + i * 4].copy_from_slice(&mask.to_le_bytes());
        }
        image.extend_from_slice(&[3, 2, 1, 128]);
        assert_eq!(pixels(&dib_to_png(&image).unwrap()), [1, 2, 3, 128]);
        image[112..116].copy_from_slice(&124u32.to_le_bytes());
        assert!(
            dib_to_png(&image).is_err(),
            "profile offsets must never be misread as pixels"
        );
    }
    #[test]
    fn png_dimensions_that_overflow_rgba_size_are_rejected() {
        let mut image = vec![0; 33];
        image[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        image[8..12].copy_from_slice(&13u32.to_be_bytes());
        image[12..16].copy_from_slice(b"IHDR");
        image[16..20].copy_from_slice(&0x80000000u32.to_be_bytes());
        image[20..24].copy_from_slice(&0x80000000u32.to_be_bytes());
        assert!(!valid_png(&image));
    }
    #[test]
    fn malformed_or_oversized_images_are_rejected_before_allocation() {
        assert!(dib_to_png(&dib(1, 24, &[])).is_err());
        assert!(dib_to_png(&dib(i32::MIN, 32, &[])).is_err());
        assert!(dib_to_png(&dib(1, 16, &[0; 4])).is_err());
        assert!(!valid_png(&[0; 33]));
    }
}
