use std::{io::Write, thread, time::Duration};

use anyhow::Result;
use crossterm::{
  cursor::{Hide, MoveTo, RestorePosition, SavePosition, Show},
  queue,
};
use ratatui::{
  Frame, Terminal,
  backend::Backend,
  buffer::{Buffer, Cell, CellDiffOption},
  layout::Rect,
};

use crate::{ProtocolOverlay, ProtocolPlacement, RenderMode};

const TMUX_CURSOR_SYNC_DELAY: Duration = Duration::from_millis(1);

#[derive(Debug, Default)]
pub struct ProtocolOverlayRenderer {
  state: Vec<ProtocolOverlayState>,
}

#[derive(Debug, Default)]
pub struct ProtocolFrameRenderer {
  overlays: ProtocolOverlayRenderer,
  protected_cells: Vec<ProtectedArea>,
}

#[derive(Debug, Clone, Default)]
pub struct ProtocolFrameOutput {
  pub overlays: Vec<ProtocolOverlay>,
  pub protocol_writes: Vec<String>,
  pub cursor_position: Option<(u16, u16)>,
  pub preserve_overlays: bool,
  pub preserve_areas: Vec<Rect>,
}

#[derive(Debug)]
pub struct ProtocolOverlayCommit<'a> {
  next_state: Vec<ProtocolOverlayState>,
  writes: Vec<ProtocolOverlayWrite<'a>>,
  removed_after_write: Vec<ProtocolOverlayState>,
  clear_areas: Vec<Rect>,
}

#[derive(Debug)]
struct ProtocolOverlayWrite<'a> {
  overlay: &'a ProtocolOverlay,
  state: ProtocolOverlayState,
  clear_areas: Vec<Rect>,
  refresh: bool,
}

#[derive(Debug, Clone)]
struct ProtectedArea {
  area: Rect,
  cells: Vec<Cell>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProtocolOverlayState {
  area: Rect,
  mode: RenderMode,
  placement: Option<ProtocolPlacement>,
  fingerprint: u64,
  erase: Option<String>,
}

impl ProtocolOverlayRenderer {
  pub fn begin<'a>(
    &self,
    writer: &mut impl Write,
    overlays: &'a [ProtocolOverlay],
  ) -> Result<ProtocolOverlayCommit<'a>> {
    let update = self.update(overlays);
    self.commit_update(writer, update)
  }

  pub fn begin_preserving<'a>(
    &self,
    writer: &mut impl Write,
    overlays: &'a [ProtocolOverlay],
    preserve_areas: &[Rect],
  ) -> Result<ProtocolOverlayCommit<'a>> {
    let update = self.update_preserving(overlays, preserve_areas);
    self.commit_update(writer, update)
  }

  fn commit_update<'a>(
    &self,
    writer: &mut impl Write,
    update: ProtocolOverlayUpdate<'a>,
  ) -> Result<ProtocolOverlayCommit<'a>> {
    erase_protocol_state(writer, &update.removed)?;
    Ok(ProtocolOverlayCommit {
      next_state: update.next_state,
      writes: update.writes,
      removed_after_write: update.removed_after_write,
      clear_areas: update.clear_areas,
    })
  }

  pub fn finish(
    &mut self,
    writer: &mut impl Write,
    commit: ProtocolOverlayCommit<'_>,
  ) -> Result<()> {
    for write in commit.writes {
      write_protocol_overlay(writer, write.overlay, write.refresh)?;
    }
    erase_protocol_state(writer, &commit.removed_after_write)?;
    self.state = commit.next_state;
    Ok(())
  }

  pub fn clear(&mut self, writer: &mut impl Write) -> Result<()> {
    let old_state = std::mem::take(&mut self.state);
    erase_protocol_state(writer, &old_state)?;
    for overlay in old_state {
      clear_protocol_area(writer, overlay.area)?;
    }
    Ok(())
  }

  fn update<'a>(&self, overlays: &'a [ProtocolOverlay]) -> ProtocolOverlayUpdate<'a> {
    self.update_with_preserved_states(overlays, Vec::new())
  }

  fn update_preserving<'a>(
    &self,
    overlays: &'a [ProtocolOverlay],
    preserve_areas: &[Rect],
  ) -> ProtocolOverlayUpdate<'a> {
    let new_areas = overlays
      .iter()
      .map(|overlay| overlay.area)
      .collect::<Vec<_>>();
    let preserved = self
      .state
      .iter()
      .filter(|state| rect_contained_by_any(state.area, preserve_areas))
      .filter(|state| !rect_intersects_any(state.area, &new_areas))
      .cloned()
      .collect::<Vec<_>>();
    self.update_with_preserved_states(overlays, preserved)
  }

  fn update_with_preserved_states<'a>(
    &self,
    overlays: &'a [ProtocolOverlay],
    preserved: Vec<ProtocolOverlayState>,
  ) -> ProtocolOverlayUpdate<'a> {
    let next = overlays
      .iter()
      .map(|overlay| {
        (
          ProtocolOverlayState {
            area: overlay.area,
            mode: overlay.mode,
            placement: overlay.placement.clone(),
            fingerprint: overlay.fingerprint,
            erase: overlay.erase.clone(),
          },
          overlay,
        )
      })
      .collect::<Vec<_>>();
    let mut next_state = next
      .iter()
      .map(|(state, _)| state.clone())
      .collect::<Vec<_>>();
    for state in preserved {
      if !next_state.contains(&state) {
        next_state.push(state);
      }
    }

    if next_state == self.state {
      return ProtocolOverlayUpdate {
        next_state,
        removed: Vec::new(),
        removed_after_write: Vec::new(),
        writes: Vec::new(),
        clear_areas: Vec::new(),
      };
    }

    let next_areas = next_state
      .iter()
      .map(|state| state.area)
      .collect::<Vec<_>>();
    let removed_all = self
      .state
      .iter()
      .filter(|state| {
        !next_state.contains(state)
          && !next_state
            .iter()
            .any(|next| same_placement(state, next) || same_protocol_resource(state, next))
      })
      .cloned()
      .collect::<Vec<_>>();
    let removed = Vec::new();
    let removed_after_write = removed_all;
    let mut clear_areas = Vec::new();
    for old in &self.state {
      if next_state.contains(old) {
        continue;
      }
      clear_areas.extend(subtract_rects(old.area, &next_areas));
    }

    // Areas whose overlays survive unchanged keep their cells untouched
    // (anti-flicker). Everything a fresh write covers gets pre-cleared so
    // that transparent pixels never reveal stale cell content left behind
    // by earlier frames.
    let unchanged_old_areas = self
      .state
      .iter()
      .filter(|old| next_state.contains(old))
      .map(|state| state.area)
      .collect::<Vec<_>>();
    let writes = next
      .iter()
      .filter(|(state, _)| !self.state.contains(state))
      .map(|(state, overlay)| ProtocolOverlayWrite {
        overlay,
        state: state.clone(),
        clear_areas: subtract_rects(overlay.area, &unchanged_old_areas),
        refresh: overlay.refresh.is_some()
          && self
            .state
            .iter()
            .any(|old| same_placement(old, state) && old.fingerprint == state.fingerprint),
      })
      .collect::<Vec<_>>();
    let writes = order_overlay_writes(writes, &self.state);

    ProtocolOverlayUpdate {
      next_state,
      removed,
      removed_after_write,
      writes,
      clear_areas,
    }
  }
}

impl ProtocolFrameRenderer {
  pub fn draw<B, F>(&mut self, terminal: &mut Terminal<B>, render: F) -> Result<()>
  where
    B: Backend + Write,
    B::Error: std::error::Error + Send + Sync + 'static,
    F: FnOnce(&mut Frame) -> ProtocolFrameOutput,
  {
    terminal.autoresize()?;

    let output = {
      let mut frame = terminal.get_frame();
      render(&mut frame)
    };

    if output.preserve_overlays && !self.overlays.is_empty() {
      let old_areas = self.overlays.areas().collect::<Vec<_>>();
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
        .iter()
        .copied()
        .filter(|area| rect_contained_by_any(*area, &preserve_areas))
        .filter(|area| !rect_intersects_any(*area, &new_areas))
        .collect::<Vec<_>>();
      let protected_old_areas = intersect_rects(&preservable_old_areas, &preserve_areas);
      let commit = {
        let backend = terminal.backend_mut();
        self
          .overlays
          .begin_preserving(backend, &output.overlays, &preserve_areas)?
      };
      let mut forced_areas = commit.clear_areas().to_vec();
      forced_areas.extend(commit.write_clear_areas());
      let repaint_overlays = flush_stale_overlay_cells(
        terminal.current_buffer_mut(),
        &output.overlays,
        &self.protected_cells,
      );
      force_update_areas(terminal.current_buffer_mut(), &forced_areas);
      restore_protected_areas(
        terminal.current_buffer_mut(),
        &self.protected_cells,
        protected_old_areas.iter().copied(),
      );
      let mut protected_snapshot_areas = new_areas;
      protected_snapshot_areas.extend(protected_old_areas.iter().copied());
      let protected_cells =
        snapshot_protocol_areas(terminal.current_buffer_mut(), protected_snapshot_areas);

      terminal.flush()?;
      terminal.swap_buffers();

      {
        let backend = terminal.backend_mut();
        write_protocol_writes(backend, &output.protocol_writes)?;
        self.overlays.finish(backend, commit)?;
        for index in repaint_overlays {
          write_protocol_overlay(backend, &output.overlays[index], false)?;
        }
        queue_cursor_state(backend, output.cursor_position)?;
        Write::flush(backend)?;
      }

      self.protected_cells = protected_cells;
      return Ok(());
    }

    let commit = {
      let backend = terminal.backend_mut();
      self.overlays.begin(backend, &output.overlays)?
    };
    let mut forced_areas = commit.clear_areas().to_vec();
    forced_areas.extend(commit.write_clear_areas());
    let repaint_overlays = flush_stale_overlay_cells(
      terminal.current_buffer_mut(),
      &output.overlays,
      &self.protected_cells,
    );
    force_update_areas(terminal.current_buffer_mut(), &forced_areas);
    let protected_cells = snapshot_protocol_areas(
      terminal.current_buffer_mut(),
      output.overlays.iter().map(|overlay| overlay.area),
    );

    terminal.flush()?;
    terminal.swap_buffers();

    {
      let backend = terminal.backend_mut();
      write_protocol_writes(backend, &output.protocol_writes)?;
      self.overlays.finish(backend, commit)?;
      for index in repaint_overlays {
        write_protocol_overlay(backend, &output.overlays[index], false)?;
      }
      queue_cursor_state(backend, output.cursor_position)?;
      Write::flush(backend)?;
    }

    self.protected_cells = protected_cells;
    Ok(())
  }

  pub fn clear(&mut self, writer: &mut impl Write) -> Result<()> {
    self.protected_cells.clear();
    self.overlays.clear(writer)
  }

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

impl ProtocolFrameOutput {
  pub fn new(overlays: Vec<ProtocolOverlay>, cursor_position: Option<(u16, u16)>) -> Self {
    Self {
      overlays,
      protocol_writes: Vec::new(),
      cursor_position,
      preserve_overlays: false,
      preserve_areas: Vec::new(),
    }
  }

  pub fn without_images(mut self) -> Self {
    self.overlays.clear();
    self.protocol_writes.clear();
    self.preserve_overlays = false;
    self.preserve_areas.clear();
    self
  }

  pub fn without_cursor(mut self) -> Self {
    self.cursor_position = None;
    self
  }
}

impl ProtocolOverlayRenderer {
  fn is_empty(&self) -> bool {
    self.state.is_empty()
  }

  fn areas(&self) -> impl Iterator<Item = Rect> + '_ {
    self.state.iter().map(|state| state.area)
  }
}

impl ProtocolOverlayCommit<'_> {
  pub fn clear_areas(&self) -> &[Rect] {
    &self.clear_areas
  }

  /// Cells that must be flushed through the regular diff before the new
  /// overlay payloads are written, so transparent pixels expose the current
  /// frame's styled cells instead of stale content.
  fn write_clear_areas(&self) -> Vec<Rect> {
    self
      .writes
      .iter()
      .flat_map(|write| write.clear_areas.iter().copied())
      .collect()
  }
}

pub fn force_update_areas(buffer: &mut Buffer, areas: &[Rect]) {
  if areas.is_empty() {
    return;
  }

  for area in areas {
    for y in area.y..area.y.saturating_add(area.height) {
      for x in area.x..area.x.saturating_add(area.width) {
        let Some(cell) = buffer.cell_mut((x, y)) else {
          continue;
        };
        cell.set_diff_option(CellDiffOption::AlwaysUpdate);
      }
    }
  }
}

pub fn skip_protocol_areas(buffer: &mut Buffer, areas: impl IntoIterator<Item = Rect>) {
  for area in areas {
    for y in area.y..area.y.saturating_add(area.height) {
      for x in area.x..area.x.saturating_add(area.width) {
        let Some(cell) = buffer.cell_mut((x, y)) else {
          continue;
        };
        cell.set_diff_option(CellDiffOption::Skip);
      }
    }
  }
}

fn snapshot_protocol_areas(
  buffer: &Buffer,
  areas: impl IntoIterator<Item = Rect>,
) -> Vec<ProtectedArea> {
  areas
    .into_iter()
    .map(|area| {
      let mut cells = Vec::with_capacity(usize::from(area.width) * usize::from(area.height));
      for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
          cells.push(buffer.cell((x, y)).cloned().unwrap_or_default());
        }
      }
      ProtectedArea { area, cells }
    })
    .collect()
}

fn restore_protected_areas(
  buffer: &mut Buffer,
  protected: &[ProtectedArea],
  fallback_areas: impl IntoIterator<Item = Rect>,
) {
  for area in fallback_areas {
    restore_protected_area(buffer, protected, area);
  }
}

fn restore_protected_area(buffer: &mut Buffer, protected: &[ProtectedArea], area: Rect) {
  skip_protocol_areas(buffer, [area]);
  for snapshot in protected {
    let Some(overlap) = rect_intersection(snapshot.area, area) else {
      continue;
    };
    for y in overlap.y..overlap.y.saturating_add(overlap.height) {
      for x in overlap.x..overlap.x.saturating_add(overlap.width) {
        let Some(snapshot_cell) = protected_cell(snapshot, x, y) else {
          continue;
        };
        let Some(cell) = buffer.cell_mut((x, y)) else {
          continue;
        };
        *cell = snapshot_cell.clone();
        cell.set_diff_option(CellDiffOption::Skip);
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
fn flush_stale_overlay_cells(
  buffer: &mut Buffer,
  overlays: &[ProtocolOverlay],
  flushed: &[ProtectedArea],
) -> Vec<usize> {
  let mut repaint = Vec::new();
  for (index, overlay) in overlays.iter().enumerate() {
    let Some(snapshot) = flushed.iter().find(|snap| snap.area == overlay.area) else {
      continue;
    };
    let mut changed = false;
    for y in overlay.area.y..rect_bottom(overlay.area) {
      for x in overlay.area.x..rect_right(overlay.area) {
        let Some(desired) = buffer.cell((x, y)) else {
          continue;
        };
        let Some(known) = protected_cell(snapshot, x, y) else {
          continue;
        };
        if desired.symbol() == known.symbol() && desired.style() == known.style() {
          continue;
        }
        if let Some(cell) = buffer.cell_mut((x, y)) {
          cell.set_diff_option(CellDiffOption::AlwaysUpdate);
        }
        changed = true;
      }
    }
    if changed && overlay.mode != RenderMode::Kitty {
      repaint.push(index);
    }
  }
  repaint
}

fn protected_cell(snapshot: &ProtectedArea, x: u16, y: u16) -> Option<&Cell> {
  if x < snapshot.area.x
    || y < snapshot.area.y
    || x >= rect_right(snapshot.area)
    || y >= rect_bottom(snapshot.area)
  {
    return None;
  }
  let local_x = usize::from(x.saturating_sub(snapshot.area.x));
  let local_y = usize::from(y.saturating_sub(snapshot.area.y));
  let width = usize::from(snapshot.area.width);
  snapshot
    .cells
    .get(local_y.saturating_mul(width).saturating_add(local_x))
}

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

struct ProtocolOverlayUpdate<'a> {
  next_state: Vec<ProtocolOverlayState>,
  removed: Vec<ProtocolOverlayState>,
  removed_after_write: Vec<ProtocolOverlayState>,
  writes: Vec<ProtocolOverlayWrite<'a>>,
  clear_areas: Vec<Rect>,
}

fn same_placement(old: &ProtocolOverlayState, new: &ProtocolOverlayState) -> bool {
  old.mode == new.mode
    && matches!(
      (&old.placement, &new.placement),
      (
        Some(ProtocolPlacement::KittyPlacement {
          image_id: old_image_id,
          placement_id: old_placement_id,
        }),
        Some(ProtocolPlacement::KittyPlacement {
          image_id: new_image_id,
          placement_id: new_placement_id,
        })
      ) if old_image_id == new_image_id && old_placement_id == new_placement_id
    )
}

fn same_protocol_resource(old: &ProtocolOverlayState, new: &ProtocolOverlayState) -> bool {
  old.mode == new.mode && old.erase.is_some() && old.erase == new.erase
}

fn order_overlay_writes<'a>(
  writes: Vec<ProtocolOverlayWrite<'a>>,
  old_states: &[ProtocolOverlayState],
) -> Vec<ProtocolOverlayWrite<'a>> {
  if writes.len() < 2 {
    return writes;
  }

  let dependencies = write_dependencies(&writes, old_states);
  stable_topological_order(writes, dependencies)
}

fn write_dependencies(
  writes: &[ProtocolOverlayWrite<'_>],
  old_states: &[ProtocolOverlayState],
) -> Vec<Vec<usize>> {
  let mut dependencies = vec![Vec::new(); writes.len()];
  for (write_index, write) in writes.iter().enumerate() {
    let vacated = old_states
      .iter()
      .filter(|old| same_placement(old, &write.state) || same_protocol_resource(old, &write.state))
      .flat_map(|old| subtract_rect(old.area, write.state.area))
      .collect::<Vec<_>>();
    if vacated.is_empty() {
      continue;
    }
    for (cover_index, cover) in writes.iter().enumerate() {
      if cover_index == write_index {
        continue;
      }
      if vacated
        .iter()
        .any(|area| rect_intersection(*area, cover.state.area).is_some())
      {
        dependencies[write_index].push(cover_index);
      }
    }
  }
  dependencies
}

fn stable_topological_order<'a>(
  mut writes: Vec<ProtocolOverlayWrite<'a>>,
  dependencies: Vec<Vec<usize>>,
) -> Vec<ProtocolOverlayWrite<'a>> {
  let len = writes.len();
  let mut emitted = vec![false; len];
  let mut out_indices = Vec::with_capacity(len);
  while out_indices.len() < len {
    let mut selected = None;
    for index in 0..len {
      if emitted[index] {
        continue;
      }
      if dependencies[index]
        .iter()
        .all(|dependency| emitted[*dependency])
      {
        selected = Some(index);
        break;
      }
    }
    let Some(index) = selected else {
      break;
    };
    emitted[index] = true;
    out_indices.push(index);
  }
  for (index, emitted_flag) in emitted.iter().enumerate().take(len) {
    if !emitted_flag {
      out_indices.push(index);
    }
  }

  let mut slots = writes.drain(..).map(Some).collect::<Vec<_>>();
  out_indices
    .into_iter()
    .filter_map(|index| slots.get_mut(index).and_then(Option::take))
    .collect()
}

fn subtract_rects(area: Rect, covers: &[Rect]) -> Vec<Rect> {
  let mut remaining = vec![area];
  for cover in covers {
    remaining = remaining
      .into_iter()
      .flat_map(|rect| subtract_rect(rect, *cover))
      .collect();
    if remaining.is_empty() {
      break;
    }
  }
  remaining
}

fn subtract_rect(area: Rect, cover: Rect) -> Vec<Rect> {
  let Some(intersection) = rect_intersection(area, cover) else {
    return vec![area];
  };

  let area_right = rect_right(area);
  let area_bottom = rect_bottom(area);
  let intersection_right = rect_right(intersection);
  let intersection_bottom = rect_bottom(intersection);
  let mut out = Vec::with_capacity(4);

  push_rect(
    &mut out,
    area.x,
    area.y,
    area.width,
    intersection.y.saturating_sub(area.y),
  );
  push_rect(
    &mut out,
    area.x,
    intersection_bottom,
    area.width,
    area_bottom.saturating_sub(intersection_bottom),
  );
  push_rect(
    &mut out,
    area.x,
    intersection.y,
    intersection.x.saturating_sub(area.x),
    intersection.height,
  );
  push_rect(
    &mut out,
    intersection_right,
    intersection.y,
    area_right.saturating_sub(intersection_right),
    intersection.height,
  );

  out
}

fn intersect_rects(areas: &[Rect], clips: &[Rect]) -> Vec<Rect> {
  let mut out = Vec::new();
  for area in areas {
    for clip in clips {
      if let Some(intersection) = rect_intersection(*area, *clip) {
        out.push(intersection);
      }
    }
  }
  out
}

fn rect_intersects_any(area: Rect, clips: &[Rect]) -> bool {
  clips
    .iter()
    .any(|clip| rect_intersection(area, *clip).is_some())
}

fn rect_contained_by_any(area: Rect, clips: &[Rect]) -> bool {
  clips.iter().any(|clip| rect_contains(*clip, area))
}

fn rect_contains(outer: Rect, inner: Rect) -> bool {
  inner.x >= outer.x
    && inner.y >= outer.y
    && rect_right(inner) <= rect_right(outer)
    && rect_bottom(inner) <= rect_bottom(outer)
}

fn rect_intersection(left: Rect, right: Rect) -> Option<Rect> {
  let x1 = left.x.max(right.x);
  let y1 = left.y.max(right.y);
  let x2 = rect_right(left).min(rect_right(right));
  let y2 = rect_bottom(left).min(rect_bottom(right));
  let width = x2.saturating_sub(x1);
  let height = y2.saturating_sub(y1);
  (width > 0 && height > 0).then_some(Rect::new(x1, y1, width, height))
}

fn rect_right(area: Rect) -> u16 {
  area.x.saturating_add(area.width)
}

fn rect_bottom(area: Rect) -> u16 {
  area.y.saturating_add(area.height)
}

fn push_rect(out: &mut Vec<Rect>, x: u16, y: u16, width: u16, height: u16) {
  if width > 0 && height > 0 {
    out.push(Rect::new(x, y, width, height));
  }
}

fn erase_protocol_state(writer: &mut impl Write, state: &[ProtocolOverlayState]) -> Result<()> {
  if state.is_empty() {
    return Ok(());
  }

  queue!(writer, SavePosition)?;
  for overlay in state {
    if let Some(sequence) = &overlay.erase {
      writer.write_all(sequence.as_bytes())?;
    }
  }
  queue!(writer, RestorePosition)?;
  Ok(())
}

fn clear_protocol_area(writer: &mut impl Write, area: Rect) -> Result<()> {
  if area.width == 0 || area.height == 0 {
    return Ok(());
  }
  let blank = " ".repeat(area.width as usize);
  queue!(writer, SavePosition)?;
  for y in area.y..area.y.saturating_add(area.height) {
    queue!(writer, MoveTo(area.x, y))?;
    writer.write_all(blank.as_bytes())?;
  }
  queue!(writer, RestorePosition)?;
  Ok(())
}

fn write_protocol_overlay(
  writer: &mut impl Write,
  overlay: &ProtocolOverlay,
  refresh: bool,
) -> Result<()> {
  if matches!(
    overlay.placement,
    Some(ProtocolPlacement::KittyPlacement { .. })
  ) && overlay.refresh.is_some()
  {
    return write_kitty_placement_overlay(writer, overlay, refresh);
  }
  // Replaced overlay areas are pre-cleared by flushing the real cell
  // content through the terminal diff (see ProtocolFrameRenderer::draw);
  // writing plain spaces here would clobber the styled cells.
  let data = if refresh {
    overlay.refresh.as_deref().unwrap_or(&overlay.data)
  } else {
    &overlay.data
  };
  let tmux_passthrough = is_tmux_passthrough(data);
  queue!(writer, SavePosition)?;
  move_to_protocol_area(writer, overlay.area, tmux_passthrough)?;
  writer.write_all(data.as_bytes())?;
  if !refresh && let Some(ProtocolPlacement::KittyUnicode { image_id }) = &overlay.placement {
    write_kitty_unicode_placeholders(writer, overlay.area, *image_id)?;
  }
  writer.write_all(b"\x1b[0m")?;
  restore_protocol_cursor(writer, tmux_passthrough)?;
  Ok(())
}

fn write_kitty_placement_overlay(
  writer: &mut impl Write,
  overlay: &ProtocolOverlay,
  refresh: bool,
) -> Result<()> {
  let Some(placement) = overlay.refresh.as_deref() else {
    return Ok(());
  };
  let tmux_passthrough = is_tmux_passthrough(placement);

  queue!(writer, SavePosition)?;
  if refresh {
    move_to_protocol_area(writer, overlay.area, tmux_passthrough)?;
    writer.write_all(placement.as_bytes())?;
  } else {
    writer.write_all(overlay.data.as_bytes())?;
    move_to_protocol_area(writer, overlay.area, tmux_passthrough)?;
    writer.write_all(placement.as_bytes())?;
  }
  writer.write_all(b"\x1b[0m")?;
  restore_protocol_cursor(writer, tmux_passthrough)?;
  Ok(())
}

fn write_protocol_writes(writer: &mut impl Write, writes: &[String]) -> Result<()> {
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

fn move_to_protocol_area(
  writer: &mut impl Write,
  area: Rect,
  tmux_passthrough: bool,
) -> Result<()> {
  if tmux_passthrough {
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

fn write_kitty_unicode_placeholders(
  writer: &mut impl Write,
  area: Rect,
  image_id: u32,
) -> Result<()> {
  if area.width == 0 || area.height == 0 {
    return Ok(());
  }

  let red = (image_id >> 16) & 0xff;
  let green = (image_id >> 8) & 0xff;
  let blue = image_id & 0xff;
  write!(writer, "\x1b[0m\x1b[38;2;{red};{green};{blue}m")?;

  for y in 0..area.height {
    queue!(writer, MoveTo(area.x, area.y.saturating_add(y)))?;
    let row = kitty_placeholder_diacritic(y);
    for x in 0..area.width {
      write!(
        writer,
        "{}{}{}",
        KITTY_PLACEHOLDER,
        row,
        kitty_placeholder_diacritic(x)
      )?;
    }
  }

  Ok(())
}

fn queue_cursor_state(writer: &mut impl Write, cursor_position: Option<(u16, u16)>) -> Result<()> {
  match cursor_position {
    Some((x, y)) => queue!(writer, Show, MoveTo(x, y))?,
    None => queue!(writer, Hide)?,
  }
  Ok(())
}

const KITTY_PLACEHOLDER: char = '\u{10EEEE}';
const KITTY_DIACRITICS: &[char] = &[
  '\u{0305}', '\u{030D}', '\u{030E}', '\u{0310}', '\u{0312}', '\u{033D}', '\u{033E}', '\u{033F}',
  '\u{0346}', '\u{034A}', '\u{034B}', '\u{034C}', '\u{0350}', '\u{0351}', '\u{0352}', '\u{0357}',
  '\u{035B}', '\u{0363}', '\u{0364}', '\u{0365}', '\u{0366}', '\u{0367}', '\u{0368}', '\u{0369}',
  '\u{036A}', '\u{036B}', '\u{036C}', '\u{036D}', '\u{036E}', '\u{036F}', '\u{0483}', '\u{0484}',
  '\u{0485}', '\u{0486}', '\u{0487}', '\u{0592}', '\u{0593}', '\u{0594}', '\u{0595}', '\u{0597}',
  '\u{0598}', '\u{0599}', '\u{059C}', '\u{059D}', '\u{059E}', '\u{059F}', '\u{05A0}', '\u{05A1}',
  '\u{05A8}', '\u{05A9}', '\u{05AB}', '\u{05AC}', '\u{05AF}', '\u{05C4}', '\u{0610}', '\u{0611}',
  '\u{0612}', '\u{0613}', '\u{0614}', '\u{0615}', '\u{0616}', '\u{0617}', '\u{0618}', '\u{0619}',
  '\u{061A}', '\u{064B}', '\u{064C}', '\u{064D}', '\u{064E}', '\u{064F}', '\u{0650}', '\u{0651}',
  '\u{0652}', '\u{0653}', '\u{0654}', '\u{0655}', '\u{0656}', '\u{0657}', '\u{0658}', '\u{0659}',
  '\u{065A}', '\u{065B}', '\u{065C}', '\u{065D}', '\u{065E}', '\u{06D6}', '\u{06D7}', '\u{06D8}',
  '\u{06D9}', '\u{06DA}', '\u{06DB}', '\u{06DC}', '\u{06DF}', '\u{06E0}', '\u{06E1}', '\u{06E2}',
  '\u{06E3}', '\u{06E4}', '\u{06E7}', '\u{06E8}', '\u{06EA}', '\u{06EB}', '\u{06EC}', '\u{0730}',
  '\u{0731}', '\u{0732}', '\u{0733}', '\u{0734}', '\u{0735}', '\u{0736}', '\u{0737}', '\u{0738}',
  '\u{0739}', '\u{073A}', '\u{073B}', '\u{073C}', '\u{073D}', '\u{073E}', '\u{073F}', '\u{0740}',
  '\u{0741}', '\u{0742}', '\u{0743}', '\u{0744}', '\u{0745}', '\u{0746}', '\u{0747}', '\u{0748}',
  '\u{0749}', '\u{074A}', '\u{07EB}', '\u{07EC}', '\u{07ED}', '\u{07EE}', '\u{07EF}', '\u{07F0}',
  '\u{07F1}', '\u{07F2}', '\u{07F3}', '\u{0816}', '\u{0817}', '\u{0818}', '\u{0819}', '\u{081B}',
  '\u{081C}', '\u{081D}', '\u{081E}', '\u{081F}', '\u{0820}', '\u{0821}', '\u{0822}', '\u{0823}',
  '\u{0825}', '\u{0826}', '\u{0827}', '\u{0829}', '\u{082A}', '\u{082B}', '\u{082C}', '\u{082D}',
  '\u{0859}', '\u{085A}', '\u{085B}', '\u{08D4}', '\u{08D5}', '\u{08D6}', '\u{08D7}', '\u{08D8}',
  '\u{08D9}', '\u{08DA}', '\u{08DB}', '\u{08DC}', '\u{08DD}', '\u{08DE}', '\u{08DF}', '\u{08E0}',
  '\u{08E1}', '\u{08E3}', '\u{08E4}', '\u{08E5}', '\u{08E6}', '\u{08E7}', '\u{08E8}', '\u{08E9}',
  '\u{08EA}', '\u{08EB}', '\u{08EC}', '\u{08ED}', '\u{08EE}', '\u{08EF}', '\u{08F0}', '\u{08F1}',
  '\u{08F2}', '\u{08F3}', '\u{08F4}', '\u{08F5}', '\u{08F6}', '\u{08F7}', '\u{08F8}', '\u{08F9}',
  '\u{08FA}', '\u{08FB}', '\u{08FC}', '\u{08FD}', '\u{08FE}', '\u{08FF}',
];

fn kitty_placeholder_diacritic(index: u16) -> char {
  KITTY_DIACRITICS[index as usize % KITTY_DIACRITICS.len()]
}

#[cfg(test)]
mod tests {
  use super::*;
  use ratatui::style::{Color, Style};

  #[test]
  fn kitty_placement_update_preclears_replaced_area() {
    let renderer = ProtocolOverlayRenderer {
      state: vec![ProtocolOverlayState {
        area: Rect::new(0, 0, 80, 23),
        mode: RenderMode::Kitty,
        placement: Some(ProtocolPlacement::KittyPlacement {
          image_id: 7,
          placement_id: 11,
        }),
        fingerprint: 1,
        erase: Some("erase".to_string()),
      }],
    };
    let overlays = vec![ProtocolOverlay {
      area: Rect::new(0, 10, 80, 13),
      mode: RenderMode::Kitty,
      data: "place".to_string(),
      refresh: None,
      placement: Some(ProtocolPlacement::KittyPlacement {
        image_id: 7,
        placement_id: 11,
      }),
      fingerprint: 2,
      erase: Some("erase".to_string()),
    }];

    let update = renderer.update(&overlays);

    assert!(update.removed.is_empty());
    assert_eq!(update.clear_areas, vec![Rect::new(0, 0, 80, 10)]);
    assert_eq!(update.writes.len(), 1);
    // The replacement image has a new fingerprint: its full area must be
    // flushed through the diff so transparent pixels cannot reveal the
    // previous frame's cells.
    assert_eq!(update.writes[0].clear_areas, vec![Rect::new(0, 10, 80, 13)]);
  }

  #[test]
  fn page_boundary_update_only_preclears_new_text_area() {
    let renderer = ProtocolOverlayRenderer {
      state: vec![ProtocolOverlayState {
        area: Rect::new(0, 0, 80, 8),
        mode: RenderMode::Kitty,
        placement: Some(ProtocolPlacement::KittyPlacement {
          image_id: 1,
          placement_id: 1,
        }),
        fingerprint: 1,
        erase: Some("erase-old".to_string()),
      }],
    };
    let overlays = vec![
      ProtocolOverlay {
        area: Rect::new(0, 0, 80, 2),
        mode: RenderMode::Kitty,
        data: "old-page".to_string(),
        refresh: None,
        placement: Some(ProtocolPlacement::KittyPlacement {
          image_id: 1,
          placement_id: 1,
        }),
        fingerprint: 2,
        erase: Some("erase-old".to_string()),
      },
      ProtocolOverlay {
        area: Rect::new(0, 3, 80, 20),
        mode: RenderMode::Kitty,
        data: "new-page".to_string(),
        refresh: None,
        placement: Some(ProtocolPlacement::KittyPlacement {
          image_id: 2,
          placement_id: 2,
        }),
        fingerprint: 3,
        erase: Some("erase-new".to_string()),
      },
    ];

    let update = renderer.update(&overlays);

    assert_eq!(update.clear_areas, vec![Rect::new(0, 2, 80, 1)]);
    assert_eq!(update.writes.len(), 2);
    let old_page = update
      .writes
      .iter()
      .find(|write| write.overlay.data == "old-page")
      .expect("old page write");
    let new_page = update
      .writes
      .iter()
      .find(|write| write.overlay.data == "new-page")
      .expect("new page write");
    assert_eq!(old_page.clear_areas, vec![Rect::new(0, 0, 80, 2)]);
    assert_eq!(new_page.clear_areas, vec![Rect::new(0, 3, 80, 20)]);
  }

  #[test]
  fn unchanged_kitty_placement_is_not_rewritten() {
    let state = ProtocolOverlayState {
      area: Rect::new(0, 0, 80, 23),
      mode: RenderMode::Kitty,
      placement: Some(ProtocolPlacement::KittyPlacement {
        image_id: 7,
        placement_id: 11,
      }),
      fingerprint: 1,
      erase: Some("erase".to_string()),
    };
    let renderer = ProtocolOverlayRenderer {
      state: vec![state.clone()],
    };
    let overlays = vec![ProtocolOverlay {
      area: state.area,
      mode: state.mode,
      data: "upload-and-place".to_string(),
      refresh: Some("place".to_string()),
      placement: state.placement,
      fingerprint: state.fingerprint,
      erase: state.erase,
    }];

    let update = renderer.update(&overlays);

    assert!(update.removed.is_empty());
    assert!(update.clear_areas.is_empty());
    assert!(update.writes.is_empty());
  }

  #[test]
  fn moving_same_kitty_placement_uses_refresh_payload() {
    let mut renderer = ProtocolOverlayRenderer {
      state: vec![ProtocolOverlayState {
        area: Rect::new(0, 0, 80, 20),
        mode: RenderMode::Kitty,
        placement: Some(ProtocolPlacement::KittyPlacement {
          image_id: 7,
          placement_id: 11,
        }),
        fingerprint: 1,
        erase: Some("erase".to_string()),
      }],
    };
    let overlays = vec![ProtocolOverlay {
      area: Rect::new(0, 1, 80, 20),
      mode: RenderMode::Kitty,
      data: "upload-and-place".to_string(),
      refresh: Some("place-only".to_string()),
      placement: Some(ProtocolPlacement::KittyPlacement {
        image_id: 7,
        placement_id: 11,
      }),
      fingerprint: 1,
      erase: Some("erase".to_string()),
    }];

    let mut begin_output = Vec::new();
    let commit = renderer.begin(&mut begin_output, &overlays).unwrap();

    assert!(begin_output.is_empty());
    assert_eq!(commit.clear_areas(), &[Rect::new(0, 0, 80, 1)]);
    assert_eq!(commit.writes.len(), 1);
    assert!(commit.writes[0].refresh);

    let mut finish_output = Vec::new();
    renderer.finish(&mut finish_output, commit).unwrap();
    let output = String::from_utf8(finish_output).unwrap();

    assert!(output.contains("place-only"));
    assert!(!output.contains("upload-and-place"));
  }

  #[test]
  fn new_kitty_placement_uploads_before_place() {
    let mut renderer = ProtocolOverlayRenderer::default();
    let overlays = vec![ProtocolOverlay {
      area: Rect::new(0, 0, 3, 1),
      mode: RenderMode::Kitty,
      data: "upload-only".to_string(),
      refresh: Some("place-only".to_string()),
      placement: Some(ProtocolPlacement::KittyPlacement {
        image_id: 7,
        placement_id: 11,
      }),
      fingerprint: 1,
      erase: Some("erase".to_string()),
    }];

    let mut begin_output = Vec::new();
    let commit = renderer.begin(&mut begin_output, &overlays).unwrap();

    assert!(begin_output.is_empty());
    assert_eq!(commit.clear_areas(), &[]);
    assert_eq!(commit.writes.len(), 1);
    assert!(!commit.writes[0].refresh);
    assert_eq!(commit.writes[0].clear_areas, vec![Rect::new(0, 0, 3, 1)]);

    let mut finish_output = Vec::new();
    renderer.finish(&mut finish_output, commit).unwrap();
    let output = String::from_utf8(finish_output).unwrap();

    // Pre-clearing now happens through the terminal diff (real styled
    // cells), not through raw space writes in the protocol stream.
    assert!(!output.contains("   "));
    let upload = output.find("upload-only").expect("upload write");
    let place = output.find("place-only").expect("placement write");
    assert!(upload < place);
  }

  #[test]
  fn scroll_down_writes_bottom_replacement_before_moving_old_bottom() {
    let mut renderer = ProtocolOverlayRenderer {
      state: vec![
        ProtocolOverlayState {
          area: Rect::new(0, 0, 80, 10),
          mode: RenderMode::Kitty,
          placement: Some(ProtocolPlacement::KittyPlacement {
            image_id: 1,
            placement_id: 1,
          }),
          fingerprint: 1,
          erase: Some("erase-a".to_string()),
        },
        ProtocolOverlayState {
          area: Rect::new(0, 10, 80, 10),
          mode: RenderMode::Kitty,
          placement: Some(ProtocolPlacement::KittyPlacement {
            image_id: 2,
            placement_id: 2,
          }),
          fingerprint: 2,
          erase: Some("erase-b".to_string()),
        },
        ProtocolOverlayState {
          area: Rect::new(0, 20, 80, 10),
          mode: RenderMode::Kitty,
          placement: Some(ProtocolPlacement::KittyPlacement {
            image_id: 3,
            placement_id: 3,
          }),
          fingerprint: 3,
          erase: Some("erase-c".to_string()),
        },
      ],
    };
    let overlays = vec![
      ProtocolOverlay {
        area: Rect::new(0, 0, 80, 10),
        mode: RenderMode::Kitty,
        data: "upload-b".to_string(),
        refresh: Some("place-b".to_string()),
        placement: Some(ProtocolPlacement::KittyPlacement {
          image_id: 2,
          placement_id: 2,
        }),
        fingerprint: 2,
        erase: Some("erase-b".to_string()),
      },
      ProtocolOverlay {
        area: Rect::new(0, 10, 80, 10),
        mode: RenderMode::Kitty,
        data: "upload-c".to_string(),
        refresh: Some("place-c".to_string()),
        placement: Some(ProtocolPlacement::KittyPlacement {
          image_id: 3,
          placement_id: 3,
        }),
        fingerprint: 3,
        erase: Some("erase-c".to_string()),
      },
      ProtocolOverlay {
        area: Rect::new(0, 20, 80, 10),
        mode: RenderMode::Kitty,
        data: "upload-d".to_string(),
        refresh: Some("place-d".to_string()),
        placement: Some(ProtocolPlacement::KittyPlacement {
          image_id: 4,
          placement_id: 4,
        }),
        fingerprint: 4,
        erase: Some("erase-d".to_string()),
      },
    ];

    let mut begin_output = Vec::new();
    let commit = renderer.begin(&mut begin_output, &overlays).unwrap();

    assert!(begin_output.is_empty());
    assert_eq!(commit.writes.len(), 3);

    let mut finish_output = Vec::new();
    renderer.finish(&mut finish_output, commit).unwrap();
    let output = String::from_utf8(finish_output).unwrap();

    let place_d = output.find("place-d").expect("bottom replacement");
    let place_c = output.find("place-c").expect("old bottom move");
    let place_b = output.find("place-b").expect("middle move");
    assert!(place_d < place_c);
    assert!(place_c < place_b);
    assert!(!output.contains("upload-b"));
    assert!(!output.contains("upload-c"));
  }

  #[test]
  fn preserving_pending_area_still_writes_ready_overlay() {
    let old_ready_area = ProtocolOverlayState {
      area: Rect::new(0, 0, 3, 1),
      mode: RenderMode::Kitty,
      placement: Some(ProtocolPlacement::KittyPlacement {
        image_id: 1,
        placement_id: 1,
      }),
      fingerprint: 1,
      erase: Some("erase-ready".to_string()),
    };
    let old_pending_area = ProtocolOverlayState {
      area: Rect::new(0, 1, 3, 1),
      mode: RenderMode::Kitty,
      placement: Some(ProtocolPlacement::KittyPlacement {
        image_id: 2,
        placement_id: 2,
      }),
      fingerprint: 2,
      erase: Some("erase-pending".to_string()),
    };
    let renderer = ProtocolOverlayRenderer {
      state: vec![old_ready_area, old_pending_area.clone()],
    };
    let overlays = vec![ProtocolOverlay {
      area: Rect::new(0, 0, 3, 1),
      mode: RenderMode::Kitty,
      data: "new-ready".to_string(),
      refresh: None,
      placement: Some(ProtocolPlacement::KittyPlacement {
        image_id: 3,
        placement_id: 3,
      }),
      fingerprint: 3,
      erase: Some("erase-new".to_string()),
    }];

    let update = renderer.update_preserving(&overlays, &[Rect::new(0, 1, 3, 1)]);

    assert_eq!(update.writes.len(), 1);
    assert_eq!(update.writes[0].overlay.data, "new-ready");
    assert!(update.next_state.contains(&old_pending_area));
    assert!(update.next_state.iter().any(|state| state.fingerprint == 3));
    assert!(update.clear_areas.is_empty());
  }

  #[test]
  fn preserving_partial_overlap_does_not_keep_old_overlay() {
    let old_area = ProtocolOverlayState {
      area: Rect::new(0, 0, 10, 10),
      mode: RenderMode::Kitty,
      placement: Some(ProtocolPlacement::KittyPlacement {
        image_id: 1,
        placement_id: 1,
      }),
      fingerprint: 1,
      erase: Some("erase-old".to_string()),
    };
    let renderer = ProtocolOverlayRenderer {
      state: vec![old_area.clone()],
    };

    let update = renderer.update_preserving(&[], &[Rect::new(5, 5, 2, 2)]);

    assert!(!update.next_state.contains(&old_area));
    assert_eq!(update.removed_after_write, vec![old_area]);
    assert_eq!(update.clear_areas, vec![Rect::new(0, 0, 10, 10)]);
  }

  #[test]
  fn moving_same_protocol_resource_does_not_erase_new_image() {
    let renderer = ProtocolOverlayRenderer {
      state: vec![ProtocolOverlayState {
        area: Rect::new(0, 0, 3, 1),
        mode: RenderMode::Kitty,
        placement: None,
        fingerprint: 1,
        erase: Some("erase-image-7".to_string()),
      }],
    };
    let overlays = vec![ProtocolOverlay {
      area: Rect::new(0, 1, 3, 1),
      mode: RenderMode::Kitty,
      data: "same-image-new-position".to_string(),
      refresh: None,
      placement: None,
      fingerprint: 1,
      erase: Some("erase-image-7".to_string()),
    }];

    let update = renderer.update(&overlays);

    assert!(update.removed_after_write.is_empty());
    assert_eq!(update.writes.len(), 1);
    assert_eq!(update.clear_areas, vec![Rect::new(0, 0, 3, 1)]);
  }

  #[test]
  fn clear_areas_are_left_to_terminal_diff() {
    let mut renderer = ProtocolOverlayRenderer {
      state: vec![ProtocolOverlayState {
        area: Rect::new(0, 0, 3, 2),
        mode: RenderMode::Kitty,
        placement: Some(ProtocolPlacement::KittyPlacement {
          image_id: 7,
          placement_id: 11,
        }),
        fingerprint: 1,
        erase: None,
      }],
    };
    let overlays = vec![ProtocolOverlay {
      area: Rect::new(0, 1, 3, 1),
      mode: RenderMode::Kitty,
      data: "place".to_string(),
      refresh: None,
      placement: Some(ProtocolPlacement::KittyPlacement {
        image_id: 7,
        placement_id: 11,
      }),
      fingerprint: 2,
      erase: None,
    }];

    let mut begin_output = Vec::new();
    let commit = renderer.begin(&mut begin_output, &overlays).unwrap();

    assert!(begin_output.is_empty());
    assert_eq!(commit.clear_areas(), &[Rect::new(0, 0, 3, 1)]);

    let mut finish_output = Vec::new();
    renderer.finish(&mut finish_output, commit).unwrap();
    let output = String::from_utf8(finish_output).unwrap();

    assert!(output.contains("place"));
    assert!(!output.contains("   "));
  }

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
    assert!(matches!(sixel_buffer[(1, 0)].diff_option, CellDiffOption::Skip));

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
}
