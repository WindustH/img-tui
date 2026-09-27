//! Drawing ratatui frames together with terminal protocol images.
//!
//! [`ProtocolFrameRenderer::draw`] renders a frame, then keeps the terminal's
//! images in step with the [`ProtocolOverlay`]s the frame returned: new or
//! changed images are written, moved kitty images are re-placed, removed ones
//! are erased, and the text beneath them is redrawn exactly where needed.

mod cells;
mod overlay;
mod placeholder;
mod rect;
mod write;

use std::io::Write;

use anyhow::Result;
use ratatui::{Frame, Terminal, backend::Backend, layout::Rect};

use self::{
  cells::{
    ProtectedArea, flush_stale_overlay_cells, restore_protected_areas, snapshot_protocol_areas,
  },
  placeholder::{clip_wide_glyphs_crossing_occluder_left_edges, fill_kitty_unicode_placeholders},
  rect::{intersect_rects, rect_contained_by_any, rect_intersects_any},
  write::{queue_cursor_state, write_protocol_overlay, write_protocol_writes},
};
pub use self::{
  cells::{force_update_areas, reserve_protocol_area, skip_protocol_areas},
  overlay::{ProtocolOverlayCommit, ProtocolOverlayRenderer},
  write::reset_protocol_images,
};
use crate::ProtocolOverlay;

/// Draws ratatui frames and the protocol images they carry. Keep one per
/// terminal and route every draw through it.
#[derive(Debug, Default)]
pub struct ProtocolFrameRenderer {
  overlays: ProtocolOverlayRenderer,
  /// Cells flushed beneath each overlay by the previous frame.
  protected_cells: Vec<ProtectedArea>,
}

/// What a frame's render closure returns besides the text it drew.
#[derive(Debug, Clone, Default)]
pub struct ProtocolFrameOutput {
  /// Images to show this frame.
  pub overlays: Vec<ProtocolOverlay>,
  /// Extra protocol sequences written verbatim after the text flush.
  pub protocol_writes: Vec<String>,
  /// Where to show the cursor; hidden when `None`.
  pub cursor_position: Option<(u16, u16)>,
  /// Keep images from earlier frames that no overlay replaces (e.g. while
  /// their successors are still rendering).
  pub preserve_overlays: bool,
  /// Limit `preserve_overlays` to images inside these areas; empty means
  /// every earlier image.
  pub preserve_areas: Vec<Rect>,
  /// Modal rectangles whose text cells replace kitty U=1 placeholders.
  pub occluders: Vec<Rect>,
}

impl ProtocolFrameRenderer {
  /// Render a frame with `render` and bring the terminal's images in line
  /// with the returned [`ProtocolFrameOutput`].
  ///
  /// Order of operations: new kitty Unicode placeholder images are uploaded,
  /// the frame's text is flushed (redrawing cells uncovered by moved or
  /// removed images and cells under images about to be replaced), the other
  /// image payloads are written, and finally images that are gone are
  /// erased, so no blank intermediate frame is ever shown.
  pub fn draw<B, F>(&mut self, terminal: &mut Terminal<B>, render: F) -> Result<()>
  where
    B: Backend + Write,
    B::Error: std::error::Error + Send + Sync + 'static,
    F: FnOnce(&mut Frame) -> ProtocolFrameOutput,
  {
    let screen_cleared = autoresize(terminal)?;
    if screen_cleared {
      // Everything the terminal showed is gone; what we remember about it
      // would suppress the rewrites the new frame needs.
      self.protected_cells.clear();
      self
        .overlays
        .forget_cleared_images(terminal.backend_mut())?;
    }

    let output = {
      let mut frame = terminal.get_frame();
      render(&mut frame)
    };
    let buffer = terminal.current_buffer_mut();
    clip_wide_glyphs_crossing_occluder_left_edges(buffer, &output.occluders);
    // U=1 kitty images display through unicode placeholder *text cells*.
    // Fill them after the render closure (modal dialogs painted by the app
    // keep their cells, clipping the image there) and before the diff flush
    // (the text diff then carries placeholder/restore updates for free).
    fill_kitty_unicode_placeholders(buffer, &output.overlays, &output.occluders);

    // After a screen clear only the images this frame lists get drawn again,
    // so earlier ones cannot be kept on screen.
    let preserve = (output.preserve_overlays && !screen_cleared && !self.overlays.is_empty())
      .then(|| PreservePlan::new(&self.overlays, &output));
    let commit = {
      let backend = terminal.backend_mut();
      match &preserve {
        Some(plan) => {
          self
            .overlays
            .begin_preserving(backend, &output.overlays, &plan.preserve_areas)?
        }
        None => self.overlays.begin(backend, &output.overlays)?,
      }
    };

    let buffer = terminal.current_buffer_mut();
    let mut forced_areas = commit.clear_areas().to_vec();
    forced_areas.extend(commit.write_clear_areas());
    let mut repaint_overlays =
      flush_stale_overlay_cells(buffer, &output.overlays, &self.protected_cells);
    // Overlays written in full after the flush need no second repaint.
    repaint_overlays.retain(|index| !commit.writes_in_full(*index));
    force_update_areas(buffer, &forced_areas);
    let mut snapshot_areas = output
      .overlays
      .iter()
      .map(|overlay| overlay.area)
      .collect::<Vec<_>>();
    if let Some(plan) = &preserve {
      restore_protected_areas(
        buffer,
        &self.protected_cells,
        plan.protected_old_areas.iter().copied(),
      );
      snapshot_areas.extend(plan.protected_old_areas.iter().copied());
    }
    let protected_cells = snapshot_protocol_areas(buffer, snapshot_areas);

    terminal.flush()?;
    terminal.swap_buffers();

    let backend = terminal.backend_mut();
    write_protocol_writes(backend, &output.protocol_writes)?;
    self.overlays.finish(backend, commit)?;
    for index in repaint_overlays {
      write_protocol_overlay(backend, &output.overlays[index], false)?;
    }
    queue_cursor_state(backend, output.cursor_position)?;
    Write::flush(backend)?;

    self.protected_cells = protected_cells;
    Ok(())
  }

  /// Erase every image and blank its area, e.g. before suspending.
  pub fn clear(&mut self, writer: &mut impl Write) -> Result<()> {
    self.protected_cells.clear();
    self.overlays.clear(writer)
  }

  /// [`clear`](Self::clear), then write `reset_sequence` (see
  /// [`reset_protocol_images`]) and flush.
  pub fn clear_and_reset(
    &mut self,
    writer: &mut impl Write,
    reset_sequence: Option<&str>,
  ) -> Result<()> {
    self.clear(writer)?;
    reset_protocol_images(writer, reset_sequence)?;
    writer.flush()?;
    Ok(())
  }
}

/// Resize the terminal's buffers to the screen, reporting whether that
/// happened. Ratatui clears the whole screen when it resizes.
fn autoresize<B>(terminal: &mut Terminal<B>) -> Result<bool>
where
  B: Backend,
  B::Error: std::error::Error + Send + Sync + 'static,
{
  let before = terminal.current_buffer_mut().area;
  terminal.autoresize()?;
  Ok(terminal.current_buffer_mut().area != before)
}

/// Which earlier images a preserving frame keeps, and the parts of them whose
/// text cells must be restored rather than redrawn.
struct PreservePlan {
  preserve_areas: Vec<Rect>,
  protected_old_areas: Vec<Rect>,
}

impl PreservePlan {
  fn new(overlays: &ProtocolOverlayRenderer, output: &ProtocolFrameOutput) -> Self {
    let old_areas = overlays.areas().collect::<Vec<_>>();
    let new_areas = output
      .overlays
      .iter()
      .map(|overlay| overlay.area)
      .collect::<Vec<_>>();
    let preserve_areas = if output.preserve_areas.is_empty() {
      old_areas.clone()
    } else {
      output.preserve_areas.clone()
    };
    let preservable_old_areas = old_areas
      .into_iter()
      .filter(|area| rect_contained_by_any(*area, &preserve_areas))
      .filter(|area| !rect_intersects_any(*area, &new_areas))
      .collect::<Vec<_>>();
    let protected_old_areas = intersect_rects(&preservable_old_areas, &preserve_areas);
    Self {
      preserve_areas,
      protected_old_areas,
    }
  }
}

impl ProtocolFrameOutput {
  pub fn new(overlays: Vec<ProtocolOverlay>, cursor_position: Option<(u16, u16)>) -> Self {
    Self {
      overlays,
      protocol_writes: Vec::new(),
      cursor_position,
      preserve_overlays: false,
      preserve_areas: Vec::new(),
      occluders: Vec::new(),
    }
  }

  /// Drop every image-related field, keeping only the cursor.
  pub fn without_images(mut self) -> Self {
    self.overlays.clear();
    self.protocol_writes.clear();
    self.preserve_overlays = false;
    self.preserve_areas.clear();
    self.occluders.clear();
    self
  }

  pub fn without_cursor(mut self) -> Self {
    self.cursor_position = None;
    self
  }
}

/// An overlay of a `mode` image drawn by `data`, for tests.
#[cfg(test)]
pub(crate) fn test_overlay(
  area: Rect,
  mode: crate::RenderMode,
  data: &str,
  fingerprint: u64,
) -> ProtocolOverlay {
  crate::ProtocolImage {
    mode,
    data: data.into(),
    refresh: None,
    placement: None,
    fingerprint,
    erase: None,
  }
  .overlay(area)
}

#[cfg(test)]
mod tests {
  use std::io;

  use ratatui::{
    backend::{Backend, ClearType, TestBackend, WindowSize},
    buffer::Cell,
    layout::{Position, Size},
    style::Style,
  };

  use super::*;
  use crate::{ProtocolImage, ProtocolPlacement, RenderMode};

  /// Test backend that also records the raw bytes written through `Write`.
  struct RecordingBackend {
    inner: TestBackend,
    written: Vec<u8>,
  }

  impl RecordingBackend {
    fn new(width: u16, height: u16) -> Self {
      Self {
        inner: TestBackend::new(width, height),
        written: Vec::new(),
      }
    }
  }

  impl Write for RecordingBackend {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
      self.written.extend_from_slice(buf);
      Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
      Ok(())
    }
  }

  impl Backend for RecordingBackend {
    type Error = <TestBackend as Backend>::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
      I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
      self.inner.draw(content)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
      self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
      self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
      self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
      self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
      self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
      self.inner.clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, Self::Error> {
      self.inner.size()
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
      self.inner.window_size()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
      Backend::flush(&mut self.inner)
    }

    fn scroll_region_up(
      &mut self,
      region: std::ops::Range<u16>,
      line_count: u16,
    ) -> Result<(), Self::Error> {
      self.inner.scroll_region_up(region, line_count)
    }

    fn scroll_region_down(
      &mut self,
      region: std::ops::Range<u16>,
      line_count: u16,
    ) -> Result<(), Self::Error> {
      self.inner.scroll_region_down(region, line_count)
    }
  }

  fn take_written(terminal: &mut Terminal<RecordingBackend>) -> String {
    String::from_utf8(std::mem::take(&mut terminal.backend_mut().written)).unwrap()
  }

  /// First row of `area` as shown on the test terminal.
  fn screen_text(terminal: &Terminal<RecordingBackend>, area: Rect) -> String {
    let screen = terminal.backend().inner.buffer();
    (area.x..area.x + area.width)
      .map(|x| screen[(x, area.y)].symbol())
      .collect()
  }

  fn overlay(area: Rect, data: &str, fingerprint: u64) -> ProtocolOverlay {
    test_overlay(area, RenderMode::Sixel, data, fingerprint)
  }

  /// A kitty overlay showing `data` through `placement`.
  fn kitty_overlay(
    area: Rect,
    (data, refresh, erase): (&str, &str, &str),
    placement: ProtocolPlacement,
    fingerprint: u64,
  ) -> ProtocolOverlay {
    ProtocolImage {
      mode: RenderMode::Kitty,
      data: data.into(),
      refresh: Some(refresh.into()),
      placement: Some(placement),
      fingerprint,
      erase: Some(erase.into()),
    }
    .overlay(area)
  }

  /// A frame that draws `text` under the overlays' areas and reserves them,
  /// the way applications lay out protocol images.
  fn frame(
    overlays: Vec<ProtocolOverlay>,
    text: &'static str,
  ) -> impl FnOnce(&mut Frame) -> ProtocolFrameOutput {
    move |frame| {
      for overlay in &overlays {
        frame
          .buffer_mut()
          .set_string(overlay.area.x, overlay.area.y, text, Style::default());
        if !matches!(
          overlay.image.placement,
          Some(ProtocolPlacement::KittyUnicode { .. })
        ) {
          reserve_protocol_area(frame, overlay.area);
        }
      }
      ProtocolFrameOutput::new(overlays, None)
    }
  }

  #[test]
  fn resize_rewrites_images_cleared_from_the_screen() {
    let mut terminal = Terminal::new(RecordingBackend::new(20, 8)).unwrap();
    let mut renderer = ProtocolFrameRenderer::default();
    let area = Rect::new(1, 1, 4, 2);
    let placement = kitty_overlay(
      Rect::new(10, 1, 4, 2),
      ("UPLOAD-K", "PLACE-K", "ERASE-K"),
      ProtocolPlacement::KittyPlacement {
        image_id: 7,
        placement_id: 3,
      },
      2,
    );
    let overlays = vec![overlay(area, "SIXEL-A", 1), placement];

    renderer
      .draw(&mut terminal, frame(overlays.clone(), "text"))
      .unwrap();
    let first = take_written(&mut terminal);
    assert!(first.contains("SIXEL-A") && first.contains("UPLOAD-K"));

    renderer
      .draw(&mut terminal, frame(overlays.clone(), "text"))
      .unwrap();
    let unchanged = take_written(&mut terminal);
    assert!(!unchanged.contains("SIXEL-A") && !unchanged.contains("PLACE-K"));

    // Ratatui clears the screen when it adopts the new size; both images
    // must be written again even though the frame did not change.
    terminal.backend_mut().inner.resize(24, 8);
    renderer
      .draw(&mut terminal, frame(overlays, "text"))
      .unwrap();
    let resized = take_written(&mut terminal);
    assert_eq!(resized.matches("SIXEL-A").count(), 1);
    let erase = resized
      .find("ERASE-K")
      .expect("stale kitty placement erased");
    let upload = resized
      .find("UPLOAD-K")
      .expect("kitty image uploaded again");
    assert!(erase < upload);
    assert!(resized[upload..].contains("PLACE-K"));
  }

  #[test]
  fn resize_keeps_placeholder_images_without_reupload() {
    let mut terminal = Terminal::new(RecordingBackend::new(20, 8)).unwrap();
    let mut renderer = ProtocolFrameRenderer::default();
    let unicode = kitty_overlay(
      Rect::new(2, 2, 3, 2),
      ("UPLOAD-U", "VIRTUAL", "ERASE-U"),
      ProtocolPlacement::KittyUnicode { image_id: 9 },
      1,
    );

    renderer
      .draw(&mut terminal, frame(vec![unicode.clone()], ""))
      .unwrap();
    assert!(take_written(&mut terminal).contains("UPLOAD-U"));

    terminal.backend_mut().inner.resize(20, 9);
    renderer
      .draw(&mut terminal, frame(vec![unicode], ""))
      .unwrap();
    let resized = take_written(&mut terminal);
    assert!(!resized.contains("UPLOAD-U") && !resized.contains("ERASE-U"));
    // The cleared screen got the placeholder cells back.
    let screen = terminal.backend().inner.buffer();
    assert!(screen[(2, 2)].symbol().starts_with('\u{10EEEE}'));
    assert!(screen[(4, 3)].symbol().starts_with('\u{10EEEE}'));
  }

  #[test]
  fn replaced_sixel_image_is_written_once() {
    let mut terminal = Terminal::new(RecordingBackend::new(20, 8)).unwrap();
    let mut renderer = ProtocolFrameRenderer::default();
    let area = Rect::new(1, 1, 4, 2);

    renderer
      .draw(
        &mut terminal,
        frame(vec![overlay(area, "SIXEL-A", 1)], "aaaa"),
      )
      .unwrap();
    take_written(&mut terminal);

    // New image, and the text beneath it changed as well.
    renderer
      .draw(
        &mut terminal,
        frame(vec![overlay(area, "SIXEL-B", 2)], "bbbb"),
      )
      .unwrap();
    let written = take_written(&mut terminal);
    assert_eq!(written.matches("SIXEL-B").count(), 1);
    // The cells beneath were flushed despite being reserved for the image.
    assert_eq!(screen_text(&terminal, area), "bbbb");
  }

  #[test]
  fn restyled_cells_under_unchanged_sixel_repaint_it() {
    let mut terminal = Terminal::new(RecordingBackend::new(20, 8)).unwrap();
    let mut renderer = ProtocolFrameRenderer::default();
    let area = Rect::new(1, 1, 4, 2);

    renderer
      .draw(
        &mut terminal,
        frame(vec![overlay(area, "SIXEL-A", 1)], "aaaa"),
      )
      .unwrap();
    take_written(&mut terminal);

    renderer
      .draw(
        &mut terminal,
        frame(vec![overlay(area, "SIXEL-A", 1)], "cccc"),
      )
      .unwrap();
    let written = take_written(&mut terminal);
    assert_eq!(written.matches("SIXEL-A").count(), 1);
    assert_eq!(screen_text(&terminal, area), "cccc");
  }
}
