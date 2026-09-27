//! Kitty Unicode placeholders (`U=1`): images drawn through text cells.
//!
//! Each cell of the image area holds U+10EEEE followed by diacritics for its
//! row, its column and the high byte of the image id; the foreground color
//! carries the low 24 bits of the id. Because the image is plain text to the
//! terminal, text drawn over part of it (a dialog) hides it cell by cell.

use ratatui::{
  buffer::{Buffer, CellDiffOption},
  layout::Rect,
  style::Color,
};
use unicode_width::UnicodeWidthStr;

use super::rect::{
  rect_bottom, rect_contains_position, rect_intersection, rect_is_empty, rect_right,
};
use crate::{ProtocolOverlay, ProtocolPlacement};

/// Fill U=1 kitty overlay areas with unicode placeholder *text cells*.
///
/// Cells covered by a modal occluder keep the modal content (the image is
/// clipped there); when the modal closes, the pane repaints blank cells and
/// the next frame re-fills them, the same way yazi handles image/dialog
/// overlap.
pub(super) fn fill_kitty_unicode_placeholders(
  buffer: &mut Buffer,
  overlays: &[ProtocolOverlay],
  occluders: &[Rect],
) {
  let mut symbol = String::with_capacity(16);
  let mut text_owned = Vec::new();
  for overlay in overlays {
    let Some(ProtocolPlacement::KittyUnicode { image_id }) = overlay.image.placement else {
      continue;
    };
    let area = overlay.area;
    // Row and column diacritics count from the overlay's origin; only the
    // part inside the buffer is visited.
    let Some(visible) = rect_intersection(area, buffer.area) else {
      continue;
    };
    let color = Color::Rgb(
      ((image_id >> 16) & 0xff) as u8,
      ((image_id >> 8) & 0xff) as u8,
      (image_id & 0xff) as u8,
    );
    let id_high_byte = kitty_placeholder_diacritic(((image_id >> 24) & 0xff) as u16);
    for y in visible.y..rect_bottom(visible) {
      mark_wide_text_owned_cells(buffer, area, visible, occluders, y, &mut text_owned);
      let row = kitty_placeholder_diacritic(y - area.y);
      // Whether the cell to the left holds a placeholder of this image, so
      // the column can be inferred from it.
      let mut continues_left = false;
      for x in visible.x..rect_right(visible) {
        let column = x - area.x;
        let cell = &mut buffer[(x, y)];
        let occluded = occluders
          .iter()
          .any(|occluder| rect_contains_position(*occluder, x, y));
        if occluded || text_owned[usize::from(column)] {
          cell.set_diff_option(CellDiffOption::None);
          continues_left = false;
          continue;
        }
        symbol.clear();
        symbol.push(KITTY_PLACEHOLDER);
        symbol.push(row);
        // Columns past the diacritic table cannot be spelled out. Kitty
        // infers the column (and the id's high byte) of a cell that carries
        // only a row diacritic from the placeholder to its left.
        if usize::from(column) < KITTY_DIACRITICS.len() || !continues_left {
          symbol.push(kitty_placeholder_diacritic(column));
          symbol.push(id_high_byte);
        }
        cell.set_symbol(&symbol);
        cell.set_fg(color);
        // Kitty interprets underline color as a placement id. Never let a
        // style inherited from the underlying widget select an unrelated
        // virtual placement.
        cell.underline_color = Color::Reset;
        cell.set_diff_option(CellDiffOption::None);
        continues_left = true;
      }
    }
  }
}

/// A wide glyph rendered immediately to the left of an opaque surface
/// occupies the surface's first cell as well. Ratatui otherwise sees the
/// wide leading cell first and skips that first popup cell while diffing.
/// Clip the whole underlying glyph so every modal boundary is valid even
/// when no protocol image exists beneath it.
pub(super) fn clip_wide_glyphs_crossing_occluder_left_edges(
  buffer: &mut Buffer,
  occluders: &[Rect],
) {
  let buffer_left = buffer.area.x;
  for occluder in occluders {
    if occluder.x <= buffer_left || rect_is_empty(*occluder) {
      continue;
    }
    let x = occluder.x - 1;
    for y in occluder.y..rect_bottom(*occluder) {
      let Some(cell) = buffer.cell_mut((x, y)) else {
        continue;
      };
      if UnicodeWidthStr::width(cell.symbol()) <= 1 {
        continue;
      }
      cell.set_symbol(" ");
      cell.set_diff_option(CellDiffOption::None);
    }
  }
}

/// Mark (in `owned`, indexed by column within `area`) the placeholder cells
/// of row `y` covered by a wide glyph whose leading cell belongs to text
/// rather than to the image. This expands a modal/text boundary by the
/// glyph's continuation cells and prevents Ratatui from skipping a U=1
/// placeholder halfway through a CJK character or emoji. `visible` is the
/// part of `area` inside the buffer.
fn mark_wide_text_owned_cells(
  buffer: &Buffer,
  area: Rect,
  visible: Rect,
  occluders: &[Rect],
  y: u16,
  owned: &mut Vec<bool>,
) {
  let scan_right = rect_right(visible);
  owned.clear();
  owned.resize(usize::from(scan_right - area.x), false);
  for x in buffer.area.x..scan_right {
    let Some(cell) = buffer.cell((x, y)) else {
      continue;
    };
    let width = UnicodeWidthStr::width(cell.symbol());
    if width <= 1 || placeholder_cell(area, occluders, x, y) {
      continue;
    }
    for offset in 1..width {
      let target = x.saturating_add(offset.min(usize::from(u16::MAX)) as u16);
      if target >= scan_right {
        break;
      }
      if placeholder_cell(area, occluders, target, y) {
        owned[usize::from(target - area.x)] = true;
      }
    }
  }
}

fn placeholder_cell(area: Rect, occluders: &[Rect], x: u16, y: u16) -> bool {
  rect_contains_position(area, x, y)
    && !occluders
      .iter()
      .any(|occluder| rect_contains_position(*occluder, x, y))
}

pub(super) const KITTY_PLACEHOLDER: char = '\u{10EEEE}';
/// Row/column diacritics from kitty's `rowcolumn-diacritics.txt`, indexed by
/// value.
const KITTY_DIACRITICS: [char; 297] = [
  '\u{0305}',
  '\u{030D}',
  '\u{030E}',
  '\u{0310}',
  '\u{0312}',
  '\u{033D}',
  '\u{033E}',
  '\u{033F}',
  '\u{0346}',
  '\u{034A}',
  '\u{034B}',
  '\u{034C}',
  '\u{0350}',
  '\u{0351}',
  '\u{0352}',
  '\u{0357}',
  '\u{035B}',
  '\u{0363}',
  '\u{0364}',
  '\u{0365}',
  '\u{0366}',
  '\u{0367}',
  '\u{0368}',
  '\u{0369}',
  '\u{036A}',
  '\u{036B}',
  '\u{036C}',
  '\u{036D}',
  '\u{036E}',
  '\u{036F}',
  '\u{0483}',
  '\u{0484}',
  '\u{0485}',
  '\u{0486}',
  '\u{0487}',
  '\u{0592}',
  '\u{0593}',
  '\u{0594}',
  '\u{0595}',
  '\u{0597}',
  '\u{0598}',
  '\u{0599}',
  '\u{059C}',
  '\u{059D}',
  '\u{059E}',
  '\u{059F}',
  '\u{05A0}',
  '\u{05A1}',
  '\u{05A8}',
  '\u{05A9}',
  '\u{05AB}',
  '\u{05AC}',
  '\u{05AF}',
  '\u{05C4}',
  '\u{0610}',
  '\u{0611}',
  '\u{0612}',
  '\u{0613}',
  '\u{0614}',
  '\u{0615}',
  '\u{0616}',
  '\u{0617}',
  '\u{0657}',
  '\u{0658}',
  '\u{0659}',
  '\u{065A}',
  '\u{065B}',
  '\u{065D}',
  '\u{065E}',
  '\u{06D6}',
  '\u{06D7}',
  '\u{06D8}',
  '\u{06D9}',
  '\u{06DA}',
  '\u{06DB}',
  '\u{06DC}',
  '\u{06DF}',
  '\u{06E0}',
  '\u{06E1}',
  '\u{06E2}',
  '\u{06E4}',
  '\u{06E7}',
  '\u{06E8}',
  '\u{06EB}',
  '\u{06EC}',
  '\u{0730}',
  '\u{0732}',
  '\u{0733}',
  '\u{0735}',
  '\u{0736}',
  '\u{073A}',
  '\u{073D}',
  '\u{073F}',
  '\u{0740}',
  '\u{0741}',
  '\u{0743}',
  '\u{0745}',
  '\u{0747}',
  '\u{0749}',
  '\u{074A}',
  '\u{07EB}',
  '\u{07EC}',
  '\u{07ED}',
  '\u{07EE}',
  '\u{07EF}',
  '\u{07F0}',
  '\u{07F1}',
  '\u{07F3}',
  '\u{0816}',
  '\u{0817}',
  '\u{0818}',
  '\u{0819}',
  '\u{081B}',
  '\u{081C}',
  '\u{081D}',
  '\u{081E}',
  '\u{081F}',
  '\u{0820}',
  '\u{0821}',
  '\u{0822}',
  '\u{0823}',
  '\u{0825}',
  '\u{0826}',
  '\u{0827}',
  '\u{0829}',
  '\u{082A}',
  '\u{082B}',
  '\u{082C}',
  '\u{082D}',
  '\u{0951}',
  '\u{0953}',
  '\u{0954}',
  '\u{0F82}',
  '\u{0F83}',
  '\u{0F86}',
  '\u{0F87}',
  '\u{135D}',
  '\u{135E}',
  '\u{135F}',
  '\u{17DD}',
  '\u{193A}',
  '\u{1A17}',
  '\u{1A75}',
  '\u{1A76}',
  '\u{1A77}',
  '\u{1A78}',
  '\u{1A79}',
  '\u{1A7A}',
  '\u{1A7B}',
  '\u{1A7C}',
  '\u{1B6B}',
  '\u{1B6D}',
  '\u{1B6E}',
  '\u{1B6F}',
  '\u{1B70}',
  '\u{1B71}',
  '\u{1B72}',
  '\u{1B73}',
  '\u{1CD0}',
  '\u{1CD1}',
  '\u{1CD2}',
  '\u{1CDA}',
  '\u{1CDB}',
  '\u{1CE0}',
  '\u{1DC0}',
  '\u{1DC1}',
  '\u{1DC3}',
  '\u{1DC4}',
  '\u{1DC5}',
  '\u{1DC6}',
  '\u{1DC7}',
  '\u{1DC8}',
  '\u{1DC9}',
  '\u{1DCB}',
  '\u{1DCC}',
  '\u{1DD1}',
  '\u{1DD2}',
  '\u{1DD3}',
  '\u{1DD4}',
  '\u{1DD5}',
  '\u{1DD6}',
  '\u{1DD7}',
  '\u{1DD8}',
  '\u{1DD9}',
  '\u{1DDA}',
  '\u{1DDB}',
  '\u{1DDC}',
  '\u{1DDD}',
  '\u{1DDE}',
  '\u{1DDF}',
  '\u{1DE0}',
  '\u{1DE1}',
  '\u{1DE2}',
  '\u{1DE3}',
  '\u{1DE4}',
  '\u{1DE5}',
  '\u{1DE6}',
  '\u{1DFE}',
  '\u{20D0}',
  '\u{20D1}',
  '\u{20D4}',
  '\u{20D5}',
  '\u{20D6}',
  '\u{20D7}',
  '\u{20DB}',
  '\u{20DC}',
  '\u{20E1}',
  '\u{20E7}',
  '\u{20E9}',
  '\u{20F0}',
  '\u{2CEF}',
  '\u{2CF0}',
  '\u{2CF1}',
  '\u{2DE0}',
  '\u{2DE1}',
  '\u{2DE2}',
  '\u{2DE3}',
  '\u{2DE4}',
  '\u{2DE5}',
  '\u{2DE6}',
  '\u{2DE7}',
  '\u{2DE8}',
  '\u{2DE9}',
  '\u{2DEA}',
  '\u{2DEB}',
  '\u{2DEC}',
  '\u{2DED}',
  '\u{2DEE}',
  '\u{2DEF}',
  '\u{2DF0}',
  '\u{2DF1}',
  '\u{2DF2}',
  '\u{2DF3}',
  '\u{2DF4}',
  '\u{2DF5}',
  '\u{2DF6}',
  '\u{2DF7}',
  '\u{2DF8}',
  '\u{2DF9}',
  '\u{2DFA}',
  '\u{2DFB}',
  '\u{2DFC}',
  '\u{2DFD}',
  '\u{2DFE}',
  '\u{2DFF}',
  '\u{A66F}',
  '\u{A67C}',
  '\u{A67D}',
  '\u{A6F0}',
  '\u{A6F1}',
  '\u{A8E0}',
  '\u{A8E1}',
  '\u{A8E2}',
  '\u{A8E3}',
  '\u{A8E4}',
  '\u{A8E5}',
  '\u{A8E6}',
  '\u{A8E7}',
  '\u{A8E8}',
  '\u{A8E9}',
  '\u{A8EA}',
  '\u{A8EB}',
  '\u{A8EC}',
  '\u{A8ED}',
  '\u{A8EE}',
  '\u{A8EF}',
  '\u{A8F0}',
  '\u{A8F1}',
  '\u{AAB0}',
  '\u{AAB2}',
  '\u{AAB3}',
  '\u{AAB7}',
  '\u{AAB8}',
  '\u{AABE}',
  '\u{AABF}',
  '\u{AAC1}',
  '\u{FE20}',
  '\u{FE21}',
  '\u{FE22}',
  '\u{FE23}',
  '\u{FE24}',
  '\u{FE25}',
  '\u{FE26}',
  '\u{10A0F}',
  '\u{10A38}',
  '\u{1D185}',
  '\u{1D186}',
  '\u{1D187}',
  '\u{1D188}',
  '\u{1D189}',
  '\u{1D1AA}',
  '\u{1D1AB}',
  '\u{1D1AC}',
  '\u{1D1AD}',
  '\u{1D242}',
  '\u{1D243}',
  '\u{1D244}',
];

fn kitty_placeholder_diacritic(index: u16) -> char {
  KITTY_DIACRITICS[usize::from(index) % KITTY_DIACRITICS.len()]
}

#[cfg(test)]
mod tests {
  use ratatui::style::{Color, Style};

  use super::*;
  use crate::{ProtocolImage, RenderMode};

  fn kitty_unicode_overlay(area: Rect) -> ProtocolOverlay {
    ProtocolImage {
      mode: RenderMode::Kitty,
      data: "upload-and-place".into(),
      refresh: Some("place".into()),
      placement: Some(ProtocolPlacement::KittyUnicode {
        image_id: 0x12_34_56,
      }),
      fingerprint: 1,
      erase: Some("erase".into()),
    }
    .overlay(area)
  }

  fn is_placeholder(buffer: &Buffer, x: u16, y: u16) -> bool {
    buffer[(x, y)].symbol().starts_with(KITTY_PLACEHOLDER)
  }

  #[test]
  fn kitty_unicode_placeholder_preserves_wide_glyph_crossing_left_edge() {
    let area = Rect::new(0, 0, 4, 1);
    let mut buffer = Buffer::empty(area);
    buffer.set_string(0, 0, "界", Style::default());
    let previous = buffer.clone();

    fill_kitty_unicode_placeholders(
      &mut buffer,
      &[kitty_unicode_overlay(Rect::new(1, 0, 2, 1))],
      &[],
    );

    assert_eq!(buffer[(0, 0)].symbol(), "界");
    assert_ne!(
      buffer[(1, 0)].symbol().chars().next(),
      Some(KITTY_PLACEHOLDER)
    );
    assert_eq!(
      buffer[(2, 0)].symbol().chars().next(),
      Some(KITTY_PLACEHOLDER)
    );
    assert_eq!(
      previous
        .diff(&buffer)
        .into_iter()
        .map(|(x, _, _)| x)
        .collect::<Vec<_>>(),
      vec![2]
    );
  }

  #[test]
  fn kitty_unicode_occluder_cells_use_regular_diff_without_placeholders() {
    let area = Rect::new(0, 0, 3, 1);
    let mut buffer = Buffer::with_lines(["pop"]);

    fill_kitty_unicode_placeholders(
      &mut buffer,
      &[kitty_unicode_overlay(area)],
      &[Rect::new(1, 0, 1, 1)],
    );

    assert_eq!(buffer[(1, 0)].symbol(), "o");
    assert!(matches!(buffer[(1, 0)].diff_option, CellDiffOption::None));
    assert_eq!(
      buffer[(0, 0)].symbol().chars().next(),
      Some(KITTY_PLACEHOLDER)
    );
    assert_eq!(
      buffer[(2, 0)].symbol().chars().next(),
      Some(KITTY_PLACEHOLDER)
    );
  }

  #[test]
  fn occluder_clips_underlying_wide_glyph_without_an_image() {
    let area = Rect::new(0, 0, 4, 1);
    let mut buffer = Buffer::empty(area);
    buffer.set_string(0, 0, "界", Style::default());
    buffer[(1, 0)].set_symbol("│");

    clip_wide_glyphs_crossing_occluder_left_edges(&mut buffer, &[Rect::new(1, 0, 3, 1)]);

    assert_eq!(buffer[(0, 0)].symbol(), " ");
    assert_eq!(buffer[(1, 0)].symbol(), "│");
    assert!(matches!(buffer[(0, 0)].diff_option, CellDiffOption::None));
  }

  #[test]
  fn kitty_unicode_occluder_preserves_wide_glyph_continuation() {
    let area = Rect::new(0, 0, 4, 1);
    let mut buffer = Buffer::empty(area);
    buffer.set_string(1, 0, "界", Style::default());

    fill_kitty_unicode_placeholders(
      &mut buffer,
      &[kitty_unicode_overlay(area)],
      &[Rect::new(1, 0, 1, 1)],
    );

    assert_eq!(buffer[(1, 0)].symbol(), "界");
    assert_ne!(
      buffer[(2, 0)].symbol().chars().next(),
      Some(KITTY_PLACEHOLDER)
    );
    assert_eq!(
      buffer[(3, 0)].symbol().chars().next(),
      Some(KITTY_PLACEHOLDER)
    );
  }

  #[test]
  fn unchanged_kitty_unicode_placeholders_produce_no_text_diff() {
    let area = Rect::new(0, 0, 3, 1);
    let mut previous = Buffer::empty(area);
    let mut current = Buffer::empty(area);
    let overlays = [kitty_unicode_overlay(area)];
    fill_kitty_unicode_placeholders(&mut previous, &overlays, &[]);
    fill_kitty_unicode_placeholders(&mut current, &overlays, &[]);

    assert!(previous.diff(&current).is_empty());
  }

  #[test]
  fn kitty_unicode_placeholder_encodes_full_image_id() {
    assert_eq!(KITTY_DIACRITICS.len(), 297);
    let area = Rect::new(0, 0, 1, 1);
    let mut buffer = Buffer::empty(area);
    let mut overlay = kitty_unicode_overlay(area);
    overlay.image.placement = Some(ProtocolPlacement::KittyUnicode {
      image_id: 0xab_12_34_56,
    });

    fill_kitty_unicode_placeholders(&mut buffer, &[overlay], &[]);

    let chars = buffer[(0, 0)].symbol().chars().collect::<Vec<_>>();
    assert_eq!(chars[0], KITTY_PLACEHOLDER);
    assert_eq!(chars[1], kitty_placeholder_diacritic(0));
    assert_eq!(chars[2], kitty_placeholder_diacritic(0));
    assert_eq!(chars[3], kitty_placeholder_diacritic(0xab));
    assert_eq!(buffer[(0, 0)].fg, Color::Rgb(0x12, 0x34, 0x56));
    assert_eq!(buffer[(0, 0)].underline_color, Color::Reset);
    assert!(matches!(buffer[(0, 0)].diff_option, CellDiffOption::None));
  }

  #[test]
  fn wide_images_infer_columns_past_the_diacritic_table() {
    let width = KITTY_DIACRITICS.len() as u16 + 3;
    let area = Rect::new(0, 0, width, 1);
    let mut buffer = Buffer::empty(area);

    fill_kitty_unicode_placeholders(
      &mut buffer,
      &[kitty_unicode_overlay(area)],
      &[Rect::new(width - 2, 0, 1, 1)],
    );

    let last_spelled = buffer[(width - 4, 0)].symbol().chars().collect::<Vec<_>>();
    assert_eq!(last_spelled.len(), 4);
    assert_eq!(
      last_spelled[2],
      KITTY_DIACRITICS[KITTY_DIACRITICS.len() - 1]
    );
    // First column past the table: row diacritic only, inferred from the left.
    let inferred = buffer[(width - 3, 0)].symbol().chars().collect::<Vec<_>>();
    assert_eq!(
      inferred,
      vec![KITTY_PLACEHOLDER, kitty_placeholder_diacritic(0)]
    );
    // After an occluded cell there is nothing to infer from.
    assert!(!is_placeholder(&buffer, width - 2, 0));
    assert_eq!(buffer[(width - 1, 0)].symbol().chars().count(), 4);
  }

  #[test]
  fn overlay_reaching_the_coordinate_limit_does_not_overflow() {
    let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 2));
    let overlay = kitty_unicode_overlay(Rect {
      x: 2,
      y: 1,
      width: u16::MAX,
      height: u16::MAX,
    });

    fill_kitty_unicode_placeholders(&mut buffer, &[overlay], &[Rect::new(3, 1, u16::MAX, 1)]);

    assert!(is_placeholder(&buffer, 2, 1));
    assert!(!is_placeholder(&buffer, 3, 1));
  }
}
