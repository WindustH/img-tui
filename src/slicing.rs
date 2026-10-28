//! Image-local slicing shared by drawing, preloading, and raster producers.
//! Applications choose the grid and visible window; no layout or scrolling policy
//! lives here. Pixel edges use one rounding rule, so neighbors never overlap.
use crate::NativeImageViewport;
use anyhow::{Result, bail};
use image::DynamicImage;
use ratatui::layout::Rect;

/// Fixed-height or evenly divided horizontal slicing grid.
#[derive(Debug, Clone, Copy)]
pub enum SliceGrid {
  Fixed {
    height: u16,
  },
  /// Useful when several images share boundaries. The application chooses
  /// the common height and count; shorter images simply stop on that grid.
  Even {
    height: u16,
    count: u16,
  },
}
impl SliceGrid {
  pub fn span(self, index: u16, image_height: u16) -> (u32, u16) {
    let (start, end) = match self {
      Self::Fixed { height } => {
        let start = u32::from(index) * u32::from(height.max(1));
        (start, start + u32::from(height.max(1)))
      }
      Self::Even { height, count } => {
        let count = u32::from(count.max(1));
        let index = u32::from(index).min(count);
        (
          u32::from(height) * index / count,
          u32::from(height) * (index + 1).min(count) / count,
        )
      }
    };
    let start = start.min(u32::from(image_height));
    let end = end.min(u32::from(image_height));
    (start, (end - start) as u16)
  }
}

impl NativeImageViewport {
  /// Intersect the window with the full image; empty/outside windows yield None.
  pub fn clipped(self) -> Option<Self> {
    let width = u32::from(self.full_width_cells)
      .saturating_sub(self.left_cells)
      .min(u32::from(self.visible_width_cells)) as u16;
    let height = u32::from(self.full_height_cells)
      .saturating_sub(self.top_cells)
      .min(u32::from(self.visible_height_cells)) as u16;
    (width > 0 && height > 0).then_some(Self {
      visible_width_cells: width,
      visible_height_cells: height,
      ..self
    })
  }
  /// Split on fixed image-local boundaries, clipping only at the viewport edges.
  pub fn slices(self, height: u16) -> impl Iterator<Item = Self> {
    let viewport = self.clipped();
    let mut top = self.top_cells;
    let limit = u32::from(height.max(1));
    std::iter::from_fn(move || {
      let viewport = viewport?;
      let end = viewport.top_cells + u32::from(viewport.visible_height_cells);
      if top >= end {
        return None;
      }
      let bottom = ((top / limit + 1) * limit).min(end);
      let slice = Self {
        top_cells: top,
        visible_height_cells: (bottom - top) as u16,
        ..viewport
      };
      top = bottom;
      Some(slice)
    })
  }
  /// Position this slice relative to its containing visible window on screen.
  pub fn screen_area(self, window: Self, screen: Rect) -> Option<Rect> {
    let x = self.left_cells.checked_sub(window.left_cells)?;
    let y = self.top_cells.checked_sub(window.top_cells)?;
    if x >= u32::from(screen.width) || y >= u32::from(screen.height) {
      return None;
    }
    Some(Rect::new(
      screen.x.saturating_add(x as u16),
      screen.y.saturating_add(y as u16),
      self.visible_width_cells.min(screen.width - x as u16),
      self.visible_height_cells.min(screen.height - y as u16),
    ))
  }
  /// Geometry-only cache suffix. Prefix with source identity and encoding settings.
  pub fn cache_fragment(self) -> String {
    format!(
      "slice-v1-{}x{}-{}-{}-{}x{}",
      self.full_width_cells,
      self.full_height_cells,
      self.left_cells,
      self.top_cells,
      self.visible_width_cells,
      self.visible_height_cells
    )
  }
  pub fn pixels(self, width: u32, height: u32) -> Result<PixelRegion> {
    let viewport = self
      .clipped()
      .ok_or_else(|| anyhow::anyhow!("viewport is empty or outside image"))?;
    if width == 0 || height == 0 {
      bail!("image is empty");
    }
    let edge = |cell: u32, cells: u16, pixels: u32| {
      (u64::from(cell) * u64::from(pixels) / u64::from(cells)) as u32
    };
    let x = edge(viewport.left_cells, viewport.full_width_cells, width);
    let y = edge(viewport.top_cells, viewport.full_height_cells, height);
    let end_x = edge(
      viewport.left_cells + u32::from(viewport.visible_width_cells),
      viewport.full_width_cells,
      width,
    );
    let end_y = edge(
      viewport.top_cells + u32::from(viewport.visible_height_cells),
      viewport.full_height_cells,
      height,
    );
    if end_x == x || end_y == y {
      bail!("viewport is smaller than one source pixel; prepare on the cell grid first");
    }
    Ok(PixelRegion {
      x,
      y,
      width: end_x - x,
      height: end_y - y,
    })
  }
}

/// Exact pixel crop, also usable by rasterizers that already know pixel coordinates.
#[derive(Debug, Clone, Copy)]
pub struct PixelRegion {
  pub x: u32,
  pub y: u32,
  pub width: u32,
  pub height: u32,
}
impl PixelRegion {
  pub fn crop(self, image: &DynamicImage) -> Result<DynamicImage> {
    if self.width == 0
      || self.height == 0
      || self.x.saturating_add(self.width) > image.width()
      || self.y.saturating_add(self.height) > image.height()
    {
      bail!("pixel crop is empty or outside image");
    }
    Ok(image.crop_imm(self.x, self.y, self.width, self.height))
  }
}

#[cfg(feature = "cache")]
mod source;
#[cfg(feature = "cache")]
pub use source::PreparedImageCache;
