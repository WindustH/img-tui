//! Clip regular Kitty placements around floating text without rescaling or
//! re-encoding their uploaded pixels. Unicode placements clip via text cells.

use ratatui::layout::Rect;

use super::rect::{rect_intersects_any, subtract_rects};
use crate::{ProtocolOverlay, ProtocolPlacement, slicing::PixelRegion};

pub(super) fn clip_overlays(
  overlays: Vec<ProtocolOverlay>,
  covers: &[Rect],
) -> Vec<ProtocolOverlay> {
  overlays
    .into_iter()
    .flat_map(|overlay| clip_overlay(overlay, covers))
    .collect()
}

fn clip_overlay(overlay: ProtocolOverlay, covers: &[Rect]) -> Vec<ProtocolOverlay> {
  if !matches!(
    overlay.image.placement,
    Some(ProtocolPlacement::KittyPlacement { .. })
  ) || !rect_intersects_any(overlay.area, covers)
  {
    return vec![overlay];
  }
  let Some(geometry) = overlay.image.kitty_geometry else {
    tracing::warn!("hiding occluded Kitty image without validated placement geometry");
    return Vec::new();
  };
  let PixelRegion {
    x,
    y,
    width,
    height,
  } = geometry.pixels;
  let area = overlay.area;
  if area.width == 0 || area.height == 0 {
    return Vec::new();
  }
  // Both ends use the same integer mapping, so adjacent pieces share a pixel
  // boundary even when a cell covers a fractional number of source pixels.
  let pixel = |offset: u16, pixels: u32, cells: u16| -> u32 {
    (u64::from(offset) * u64::from(pixels) / u64::from(cells)) as u32
  };
  subtract_rects(area, covers)
    .into_iter()
    .enumerate()
    .filter_map(|(index, visible)| {
      let left = pixel(visible.x - area.x, width, area.width);
      let top = pixel(visible.y - area.y, height, area.height);
      let right = pixel(visible.right() - area.x, width, area.width);
      let bottom = pixel(visible.bottom() - area.y, height, area.height);
      if right == left || bottom == top {
        return None;
      }
      let cropped = geometry.crop(
        PixelRegion {
          x: x + left,
          y: y + top,
          width: right - left,
          height: bottom - top,
        },
        visible.width,
        visible.height,
        index,
      );
      let mut image = overlay.image.clone();
      image.refresh = Some(cropped.refresh().into());
      image.erase = Some(cropped.erase().into());
      image.kitty_geometry = Some(cropped);
      image.placement = Some(ProtocolPlacement::KittyPlacement {
        image_id: cropped.image_id,
        placement_id: cropped.placement_id,
      });
      Some(ProtocolOverlay {
        area: visible,
        image,
      })
    })
    .collect()
}
