//! Mapping a cell viewport onto the pixels of a prepared image.

use anyhow::{Result, bail};
use image::DynamicImage;

use super::NativeImageViewport;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PixelViewport {
  pub(super) x: u32,
  pub(super) y: u32,
  pub(super) width: u32,
  pub(super) height: u32,
}

/// Pixel rectangle of an image `pixel_width` x `pixel_height` that the
/// visible part of `viewport` shows. The image is assumed to span the
/// viewport's full cell size, so cells map to pixels proportionally.
pub(super) fn pixel_viewport(
  (pixel_width, pixel_height): (u32, u32),
  viewport: NativeImageViewport,
) -> Result<PixelViewport> {
  if pixel_width == 0 || pixel_height == 0 {
    bail!("image is empty");
  }
  if viewport.visible_width_cells == 0 || viewport.visible_height_cells == 0 {
    bail!("viewport is empty");
  }

  let full_width_cells = u32::from(viewport.full_width_cells.max(1));
  let full_height_cells = u32::from(viewport.full_height_cells.max(1));
  let left_cells = viewport.left_cells.min(full_width_cells);
  let top_cells = viewport.top_cells.min(full_height_cells);
  let right_cells = left_cells
    .saturating_add(u32::from(viewport.visible_width_cells))
    .min(full_width_cells);
  let bottom_cells = top_cells
    .saturating_add(u32::from(viewport.visible_height_cells))
    .min(full_height_cells);

  let x = scale_cells_to_pixels_floor(left_cells, full_width_cells, pixel_width);
  let y = scale_cells_to_pixels_floor(top_cells, full_height_cells, pixel_height);
  if x >= pixel_width || y >= pixel_height {
    bail!("viewport starts outside image bounds");
  }

  let end_x = scale_cells_to_pixels_ceil(right_cells, full_width_cells, pixel_width);
  let end_y = scale_cells_to_pixels_ceil(bottom_cells, full_height_cells, pixel_height);
  Ok(PixelViewport {
    x,
    y,
    width: end_x.saturating_sub(x).max(1),
    height: end_y.saturating_sub(y).max(1),
  })
}

/// The part of `image` that `viewport` shows.
pub(super) fn crop(image: &DynamicImage, viewport: NativeImageViewport) -> Result<DynamicImage> {
  let pixels = pixel_viewport((image.width(), image.height()), viewport)?;
  Ok(image.crop_imm(pixels.x, pixels.y, pixels.width, pixels.height))
}

fn scale_cells_to_pixels_floor(cells: u32, full_cells: u32, pixels: u32) -> u32 {
  let scaled = u64::from(cells) * u64::from(pixels) / u64::from(full_cells.max(1));
  scaled.min(u64::from(pixels)) as u32
}

fn scale_cells_to_pixels_ceil(cells: u32, full_cells: u32, pixels: u32) -> u32 {
  let scaled = (u64::from(cells) * u64::from(pixels)).div_ceil(u64::from(full_cells.max(1)));
  scaled.min(u64::from(pixels)) as u32
}
