//! Writing overlay payloads, erase sequences and cursor state to the terminal.

use std::{io::Write, thread, time::Duration};

use anyhow::Result;
use crossterm::{
  cursor::{Hide, MoveTo, RestorePosition, SavePosition, Show},
  queue,
};
use ratatui::layout::Rect;

use super::rect::{rect_bottom, rect_is_empty};
use crate::{ProtocolOverlay, ProtocolPlacement};

const TMUX_CURSOR_SYNC_DELAY: Duration = Duration::from_millis(1);

/// Write `sequence` (typically a kitty delete-all from
/// [`erase_sequence`](crate::native_image::erase_sequence)) without moving
/// the cursor, and flush. Does nothing for `None`.
pub fn reset_protocol_images(writer: &mut impl Write, sequence: Option<&str>) -> Result<()> {
  let Some(sequence) = sequence else {
    return Ok(());
  };
  queue!(writer, SavePosition)?;
  writer.write_all(sequence.as_bytes())?;
  queue!(writer, RestorePosition)?;
  writer.flush()?;
  Ok(())
}

/// Write the erase sequences of `erases` without moving the cursor.
pub(super) fn write_erase_sequences<'a>(
  writer: &mut impl Write,
  erases: impl IntoIterator<Item = Option<&'a str>>,
) -> Result<()> {
  let mut erases = erases.into_iter().flatten().peekable();
  if erases.peek().is_none() {
    return Ok(());
  }
  queue!(writer, SavePosition)?;
  for sequence in erases {
    writer.write_all(sequence.as_bytes())?;
  }
  queue!(writer, RestorePosition)?;
  Ok(())
}

/// Overwrite `area` with spaces.
pub(super) fn clear_protocol_area(writer: &mut impl Write, area: Rect) -> Result<()> {
  if rect_is_empty(area) {
    return Ok(());
  }
  let blank = " ".repeat(usize::from(area.width));
  queue!(writer, SavePosition)?;
  for y in area.y..rect_bottom(area) {
    queue!(writer, MoveTo(area.x, y))?;
    writer.write_all(blank.as_bytes())?;
  }
  queue!(writer, RestorePosition)?;
  Ok(())
}

/// Write an overlay's payload at its area: the full `data`, or with
/// `refresh` the lighter re-placement payload for an image that is already
/// on the terminal.
pub(super) fn write_protocol_overlay(
  writer: &mut impl Write,
  overlay: &ProtocolOverlay,
  refresh: bool,
) -> Result<()> {
  // A U=1 image's screen position comes entirely from its placeholder text
  // cells. When the same image moves, the regular terminal diff relocates
  // those cells; creating another virtual placement would be redundant and
  // can leave multiple placements competing for the same image id.
  let image = &overlay.image;
  if refresh
    && matches!(
      image.placement,
      Some(ProtocolPlacement::KittyUnicode { .. })
    )
  {
    return Ok(());
  }
  if matches!(
    image.placement,
    Some(ProtocolPlacement::KittyPlacement { .. })
  ) && image.refresh.is_some()
  {
    return write_kitty_placement_overlay(writer, overlay, refresh);
  }
  // Replaced overlay areas are pre-cleared by flushing the real cell
  // content through the terminal diff (see ProtocolFrameRenderer::draw);
  // writing plain spaces here would clobber the styled cells.
  let data = if refresh {
    image.refresh.as_deref().unwrap_or(&image.data)
  } else {
    &image.data
  };
  let tmux_passthrough = is_tmux_passthrough(data);
  queue!(writer, SavePosition)?;
  move_to_protocol_area(writer, overlay.area, tmux_passthrough)?;
  writer.write_all(data.as_bytes())?;
  writer.write_all(b"\x1b[0m")?;
  restore_protocol_cursor(writer, tmux_passthrough)?;
  Ok(())
}

/// Upload (unless `refresh`) at the current cursor, then place at the
/// overlay area: `data` holds the upload and `refresh` the placement.
fn write_kitty_placement_overlay(
  writer: &mut impl Write,
  overlay: &ProtocolOverlay,
  refresh: bool,
) -> Result<()> {
  let Some(placement) = overlay.image.refresh.as_deref() else {
    return Ok(());
  };
  let tmux_passthrough = is_tmux_passthrough(placement);

  queue!(writer, SavePosition)?;
  if !refresh {
    writer.write_all(overlay.image.data.as_bytes())?;
  }
  move_to_protocol_area(writer, overlay.area, tmux_passthrough)?;
  writer.write_all(placement.as_bytes())?;
  writer.write_all(b"\x1b[0m")?;
  restore_protocol_cursor(writer, tmux_passthrough)?;
  Ok(())
}

/// Write application-supplied protocol sequences without moving the cursor.
pub(super) fn write_protocol_writes(writer: &mut impl Write, writes: &[String]) -> Result<()> {
  if writes.is_empty() {
    return Ok(());
  }
  queue!(writer, SavePosition)?;
  for write in writes {
    writer.write_all(write.as_bytes())?;
  }
  queue!(writer, RestorePosition)?;
  Ok(())
}

/// Show the cursor at `cursor_position`, or hide it.
pub(super) fn queue_cursor_state(
  writer: &mut impl Write,
  cursor_position: Option<(u16, u16)>,
) -> Result<()> {
  match cursor_position {
    Some((x, y)) => queue!(writer, Show, MoveTo(x, y))?,
    None => queue!(writer, Hide)?,
  }
  Ok(())
}

fn move_to_protocol_area(
  writer: &mut impl Write,
  area: Rect,
  tmux_passthrough: bool,
) -> Result<()> {
  if tmux_passthrough {
    // tmux forwards passthrough payloads to the outer terminal at *its*
    // cursor position; make sure tmux has synced the cursor move there
    // before the payload arrives.
    for _ in 0..3 {
      queue!(writer, MoveTo(area.x, area.y), Show)?;
    }
    writer.flush()?;
    thread::sleep(TMUX_CURSOR_SYNC_DELAY);
  } else {
    queue!(writer, MoveTo(area.x, area.y))?;
  }
  Ok(())
}

fn restore_protocol_cursor(writer: &mut impl Write, tmux_passthrough: bool) -> Result<()> {
  if tmux_passthrough {
    queue!(writer, Hide, RestorePosition)?;
  } else {
    queue!(writer, RestorePosition)?;
  }
  Ok(())
}

fn is_tmux_passthrough(data: &str) -> bool {
  data.starts_with("\x1bPtmux;")
}
