//! Text-cell bookkeeping under protocol images: diff options and snapshots
//! of what was last flushed beneath each image.

use ratatui::{
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
      &overlay.placement,
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
    if changed && overlay.mode != RenderMode::Kitty {
      repaint.push(index);
    }
  }
  repaint
}

#[cfg(test)]
mod tests {
  use ratatui::style::{Color, Style};

  use super::*;

  #[test]
  fn stale_cells_under_unchanged_overlay_are_flushed() {
    let area = Rect::new(0, 0, 2, 1);
    let mut flushed_frame = Buffer::empty(area);
    flushed_frame[(0, 0)].set_symbol("a");
    flushed_frame[(1, 0)].set_symbol("b");
    let flushed = snapshot_protocol_areas(&flushed_frame, [area]);

    // Desired frame: first cell restyled (hover background), second cell
    // identical; both marked Skip because an overlay covers them.
    let mut desired = Buffer::empty(area);
    desired[(0, 0)].set_symbol("a");
    desired[(0, 0)].set_style(Style::default().bg(Color::Yellow));
    desired[(1, 0)].set_symbol("b");
    skip_protocol_areas(&mut desired, [area]);

    let overlay = |mode: RenderMode| ProtocolOverlay {
      area,
      mode,
      data: "payload".to_string(),
      refresh: None,
      placement: None,
      fingerprint: 7,
      erase: None,
    };

    let mut sixel_buffer = desired.clone();
    let repaint =
      flush_stale_overlay_cells(&mut sixel_buffer, &[overlay(RenderMode::Sixel)], &flushed);
    assert_eq!(repaint, vec![0]);
    assert!(matches!(
      sixel_buffer[(0, 0)].diff_option,
      CellDiffOption::AlwaysUpdate
    ));
    assert!(matches!(
      sixel_buffer[(1, 0)].diff_option,
      CellDiffOption::Skip
    ));

    // Kitty composites above text, so the flushed cells cannot damage the
    // image and no re-placement is needed.
    let mut kitty_buffer = desired;
    let repaint =
      flush_stale_overlay_cells(&mut kitty_buffer, &[overlay(RenderMode::Kitty)], &flushed);
    assert!(repaint.is_empty());
    assert!(matches!(
      kitty_buffer[(0, 0)].diff_option,
      CellDiffOption::AlwaysUpdate
    ));
  }

  #[test]
  fn stale_cells_without_snapshot_are_left_alone() {
    let area = Rect::new(0, 0, 2, 1);
    let mut buffer = Buffer::empty(area);
    buffer[(0, 0)].set_style(Style::default().bg(Color::Red));
    skip_protocol_areas(&mut buffer, [area]);
    let overlay = ProtocolOverlay {
      area,
      mode: RenderMode::Iterm2,
      data: "payload".to_string(),
      refresh: None,
      placement: None,
      fingerprint: 1,
      erase: None,
    };

    let repaint = flush_stale_overlay_cells(&mut buffer, &[overlay], &[]);
    assert!(repaint.is_empty());
    assert!(matches!(buffer[(0, 0)].diff_option, CellDiffOption::Skip));
  }

  #[test]
  fn protected_cells_are_restored_before_preserve_skip() {
    let area = Rect::new(0, 0, 3, 1);
    let mut committed = Buffer::empty(area);
    committed[(0, 0)].set_symbol("a");
    committed[(1, 0)].set_symbol("b");
    committed[(2, 0)].set_symbol("c");
    skip_protocol_areas(&mut committed, [area]);
    let protected = snapshot_protocol_areas(&committed, [area]);

    let mut next = Buffer::empty(area);
    next[(0, 0)].set_symbol("x");
    next[(1, 0)].set_symbol("y");
    next[(2, 0)].set_symbol("z");
    restore_protected_areas(&mut next, &protected, [area]);

    assert_eq!(next[(0, 0)].symbol(), "a");
    assert_eq!(next[(1, 0)].symbol(), "b");
    assert_eq!(next[(2, 0)].symbol(), "c");
    assert!(
      next
        .content()
        .iter()
        .all(|cell| matches!(cell.diff_option, CellDiffOption::Skip))
    );
  }

  #[test]
  fn diff_options_ignore_cells_outside_the_buffer() {
    let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 2));
    force_update_areas(&mut buffer, &[Rect::new(1, 1, 40, 40)]);
    skip_protocol_areas(&mut buffer, [Rect::new(u16::MAX - 1, 0, 1, 1)]);

    assert!(matches!(
      buffer[(1, 1)].diff_option,
      CellDiffOption::AlwaysUpdate
    ));
    assert!(matches!(buffer[(0, 0)].diff_option, CellDiffOption::None));
  }
}
