//! CPU hashes of damaged capture tiles for WSS v4 refinement.
//!
//! Only the damaged tiles are copied (`CopySubresourceRegion`, one copy per
//! horizontal run of dirty tiles) into a reusable CPU-readable staging
//! texture, so the bus traffic is the damage, not the frame. The staging
//! texture is allocated once at capture size to avoid a `CreateTexture2D` per
//! frame; only the copied regions are ever read.
use std::time::Instant;

use anyhow::{bail, Context, Result};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BOX, D3D11_CPU_ACCESS_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
    DXGI_FORMAT_R10G10B10A2_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R8G8B8A8_UNORM,
    DXGI_FORMAT_R8G8B8A8_UNORM_SRGB, DXGI_SAMPLE_DESC,
};

use crate::scroll_detect::TileRows;
use crate::tile_quality::{hash_rows, TileGrid};

/// Hash only while the dirty tiles cover at most this share of the frame (per
/// mille); larger damage is large motion and its tiles stay unhashed.
pub const MAX_HASHED_PER_MILLE: u64 = 400;

fn bytes_per_pixel(format: DXGI_FORMAT) -> Option<u32> {
    match format {
        DXGI_FORMAT_B8G8R8A8_UNORM
        | DXGI_FORMAT_B8G8R8A8_UNORM_SRGB
        | DXGI_FORMAT_R8G8B8A8_UNORM
        | DXGI_FORMAT_R8G8B8A8_UNORM_SRGB
        | DXGI_FORMAT_R10G10B10A2_UNORM => Some(4),
        DXGI_FORMAT_R16G16B16A16_FLOAT => Some(8),
        _ => None,
    }
}

/// Horizontal runs `(row, first_column, last_column)` of the given tiles.
pub fn tile_runs(grid: TileGrid, tiles: &[usize]) -> Vec<(u32, u32, u32)> {
    let mut sorted: Vec<usize> = tiles.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut runs: Vec<(u32, u32, u32)> = Vec::new();
    for index in sorted {
        let row = index as u32 / grid.columns;
        let column = index as u32 % grid.columns;
        match runs.last_mut() {
            Some((run_row, _, last)) if *run_row == row && *last + 1 == column => *last = column,
            _ => runs.push((row, column, column)),
        }
    }
    runs
}

/// `(tile, hash)` pairs for the dirty tiles.
pub type TileHashes = Vec<(usize, Option<u64>)>;

pub struct TileHasher {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    staging: ID3D11Texture2D,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    bytes_per_pixel: u32,
}

impl TileHasher {
    pub fn new(device: &ID3D11Device, texture: &ID3D11Texture2D) -> Result<Self> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        let Some(bytes_per_pixel) = bytes_per_pixel(desc.Format) else {
            bail!(
                "tile hashing does not support capture format {:?}",
                desc.Format
            );
        };
        if desc.SampleDesc.Count != 1 {
            bail!("tile hashing needs a single-sample capture texture");
        }
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: desc.Width,
            Height: desc.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: desc.Format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) }
            .context("CreateTexture2D(tile hash staging)")?;
        let staging = staging.context("CreateTexture2D(tile hash staging) returned no texture")?;
        let context = unsafe { device.GetImmediateContext() }.context("tile hash context")?;
        Ok(Self {
            device: device.clone(),
            context,
            staging,
            width: desc.Width,
            height: desc.Height,
            format: desc.Format,
            bytes_per_pixel,
        })
    }

    pub fn is_for(&self, device: &ID3D11Device, texture: &ID3D11Texture2D) -> bool {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        self.device.as_raw() == device.as_raw()
            && desc.Width == self.width
            && desc.Height == self.height
            && desc.Format == self.format
    }

    /// Hash `tiles` of `texture`; returns `(tile, hash)` pairs, the per-row
    /// hashes they were built from (for scroll detection), and the time spent
    /// (copy + map + hash) in microseconds.
    pub fn hash_tiles(
        &mut self,
        texture: &ID3D11Texture2D,
        grid: TileGrid,
        tiles: &[usize],
    ) -> Result<(TileHashes, TileRows, u64)> {
        let started = Instant::now();
        if grid.width != self.width || grid.height != self.height {
            bail!("tile grid does not match the capture texture");
        }
        if tiles.is_empty() {
            return Ok((Vec::new(), Vec::new(), 0));
        }
        for (row, first, last) in tile_runs(grid, tiles) {
            let left = grid.tile_rect((row * grid.columns + first) as usize);
            let right = grid.tile_rect((row * grid.columns + last) as usize);
            let source_box = D3D11_BOX {
                left: left.left,
                top: left.top,
                front: 0,
                right: right.right,
                bottom: left.bottom,
                back: 1,
            };
            unsafe {
                self.context.CopySubresourceRegion(
                    &self.staging,
                    0,
                    left.left,
                    left.top,
                    0,
                    texture,
                    0,
                    Some(&source_box),
                );
            }
        }
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }
        .context("map tile hash staging")?;
        let pitch = mapped.RowPitch as usize;
        let data = unsafe {
            std::slice::from_raw_parts(mapped.pData.cast::<u8>(), pitch * self.height as usize)
        };
        let bpp = self.bytes_per_pixel as usize;
        let rows: TileRows = tiles
            .iter()
            .map(|&index| {
                let rect = grid.tile_rect(index);
                let start = rect.left as usize * bpp;
                let end = rect.right as usize * bpp;
                let rows = (rect.top as usize..rect.bottom as usize)
                    .map(|y| hash_rows(std::iter::once(&data[y * pitch + start..y * pitch + end])))
                    .collect();
                (index, rows)
            })
            .collect();
        unsafe { self.context.Unmap(&self.staging, 0) };
        let hashes = rows
            .iter()
            .map(|(index, rows)| (*index, Some(tile_hash_from_rows(rows))))
            .collect();
        Ok((hashes, rows, started.elapsed().as_micros() as u64))
    }
}

/// A tile's hash from its row hashes (equal tiles hash equal).
pub fn tile_hash_from_rows(rows: &[u64]) -> u64 {
    let bytes: Vec<u8> = rows.iter().flat_map(|hash| hash.to_le_bytes()).collect();
    hash_rows(std::iter::once(bytes.as_slice()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirty_tiles_are_copied_as_horizontal_runs() {
        let grid = TileGrid::new(256, 128);
        assert_eq!(
            tile_runs(grid, &[5, 0, 1, 3, 4, 1]),
            vec![(0, 0, 1), (0, 3, 3), (1, 0, 1)]
        );
        assert!(tile_runs(grid, &[]).is_empty());
    }
}
