//! Text-cell bookkeeping under protocol images: diff options and snapshots
//! of what was last flushed beneath each image.

use ratatui::{
  Frame,
  buffer::{Buffer, Cell, CellDiffOption},
  layout::Rect,
};

use super::rect::{rect_bottom, rect_intersection, rect_right};
use crate::{ProtocolOverlay, ProtocolPlacement, RenderMode};

/// Mark every cell of `areas` to be redrawn by the next flush even if it did
/// not change since the previous frame.
pub fn force_update_areas(buffer: &mut Buffer, areas: &[Rect]) {
  for area in areas {
    set_diff_option(buffer, *area, CellDiffOption::AlwaysUpdate);
  }
}

/// Mark every cell of `areas` to be left untouched by the next flush, so a
/// protocol image drawn there is not overwritten by text.
pub fn skip_protocol_areas(buffer: &mut Buffer, areas: impl IntoIterator<Item = Rect>) {
  for area in areas {
    set_diff_option(buffer, area, CellDiffOption::Skip);
  }
}

/// Keep the frame's text from being drawn over the protocol image at `area`
/// ([`skip_protocol_areas`] for one area). Call it for each overlay while
/// drawing the frame.
pub fn reserve_protocol_area(frame: &mut Frame, area: Rect) {
  skip_protocol_areas(frame.buffer_mut(), [area]);
}

fn set_diff_option(buffer: &mut Buffer, area: Rect, option: CellDiffOption) {
  let Some(area) = rect_intersection(area, buffer.area) else {
    return;
  };
  for y in area.y..rect_bottom(area) {
    for x in area.x..rect_right(area) {
      buffer[(x, y)].set_diff_option(option);
    }
  }
}

/// Copy of the cells flushed beneath a protocol image.
#[derive(Debug, Clone)]
pub(super) struct ProtectedArea {
  area: Rect,
  cells: Vec<Cell>,
}

impl ProtectedArea {
  fn cell(&self, x: u16, y: u16) -> Option<&Cell> {
    if x < self.area.x
      || y < self.area.y
      || x >= rect_right(self.area)
      || y >= rect_bottom(self.area)
    {
      return None;
    }
    let local_x = usize::from(x - self.area.x);
    let local_y = usize::from(y - self.area.y);
    self
      .cells
      .get(local_y * usize::from(self.area.width) + local_x)
  }
}

pub(super) fn snapshot_protocol_areas(
  buffer: &Buffer,
  areas: impl IntoIterator<Item = Rect>,
) -> Vec<ProtectedArea> {
  areas
    .into_iter()
    .map(|area| {
      let mut cells = Vec::with_capacity(usize::from(area.width) * usize::from(area.height));
      for y in area.y..rect_bottom(area) {
        for x in area.x..rect_right(area) {
          cells.push(buffer.cell((x, y)).cloned().unwrap_or_default());
        }
      }
      ProtectedArea { area, cells }
    })
    .collect()
}

/// Put the snapshot cells back into `areas` and keep the flush away from
/// them, so a preserved image and the text beneath it stay as they are.
pub(super) fn restore_protected_areas(
  buffer: &mut Buffer,
  protected: &[ProtectedArea],
  areas: impl IntoIterator<Item = Rect>,
) {
  for area in areas {
    skip_protocol_areas(buffer, [area]);
    for snapshot in protected {
      let Some(overlap) = rect_intersection(snapshot.area, area) else {
        continue;
      };
      for y in overlap.y..rect_bottom(overlap) {
        for x in overlap.x..rect_right(overlap) {
          let (Some(snapshot_cell), Some(cell)) = (snapshot.cell(x, y), buffer.cell_mut((x, y)))
          else {
            continue;
          };
          *cell = snapshot_cell.clone();
          cell.set_diff_option(CellDiffOption::Skip);
        }
      }
    }
  }
}

/// Cells under live overlays are diff-skipped, so styling changes beneath an
/// unchanged image (hover/selection backgrounds) would never reach the
/// terminal, and stale styling can leak through transparent pixels. Compare
/// the desired cells with what we last flushed under each overlay and
/// force-update the changed ones. Returns indices of overlays whose pixels
/// must be re-placed afterwards because flushing cells damages them
/// (sixel / iTerm2 composite below text).
pub(super) fn flush_stale_overlay_cells(
  buffer: &mut Buffer,
  overlays: &[ProtocolOverlay],
  flushed: &[ProtectedArea],
) -> Vec<usize> {
  let mut repaint = Vec::new();
  for (index, overlay) in overlays.iter().enumerate() {
    // U=1 placeholders live in the text buffer; there is no post-flush
    // damage to repair.
    if matches!(
      overlay.image.placement,
      Some(ProtocolPlacement::KittyUnicode { .. })
    ) {
      continue;
    }
    let Some(snapshot) = flushed.iter().find(|snap| snap.area == overlay.area) else {
      continue;
    };
    let mut changed = false;
    for y in overlay.area.y..rect_bottom(overlay.area) {
      for x in overlay.area.x..rect_right(overlay.area) {
        let (Some(desired), Some(known)) = (buffer.cell((x, y)), snapshot.cell(x, y)) else {
          continue;
        };
        if desired.symbol() == known.symbol() && desired.style() == known.style() {
          continue;
        }
        buffer[(x, y)].set_diff_option(CellDiffOption::AlwaysUpdate);
        changed = true;
      }
    }
    if changed && overlay.image.mode != RenderMode::Kitty {
      repaint.push(index);
    }
  }
  repaint
}

/// Snapshot opaque text surfaces for repaint after pixel-protocol uploads.
/// Buffer::diff handles wide characters so continuation cells are not emitted
/// over the preceding glyph. Only surfaces intersecting legacy images need it.
pub(super) fn occluder_updates(
  buffer: &Buffer,
  occluders: &[Rect],
  image_areas: &[Rect],
) -> Vec<(u16, u16, Cell)> {
  let mut updates = Vec::new();
  for area in occluders {
    let Some(area) = rect_intersection(*area, buffer.area) else {
      continue;
    };
    if !image_areas
      .iter()
      .any(|image| rect_intersection(*image, area).is_some())
    {
      continue;
    }
    let empty = Buffer::empty(area);
    let mut text = Buffer::empty(area);
    for y in area.y..rect_bottom(area) {
      for x in area.x..rect_right(area) {
        text[(x, y)] = buffer[(x, y)].clone();
        text[(x, y)].set_diff_option(CellDiffOption::AlwaysUpdate);
      }
    }
    updates.extend(
      empty
        .diff(&text)
        .into_iter()
        .map(|(x, y, cell)| (x, y, cell.clone())),
    );
  }
  updates
}
