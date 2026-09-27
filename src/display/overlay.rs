//! Protocol overlay bookkeeping: which images are on the terminal, which of
//! them a new frame writes, moves or removes, and in what order.

use std::{io::Write, sync::Arc};

use anyhow::Result;
use ratatui::layout::Rect;

use super::{
  rect::{
    rect_contained_by_any, rect_intersection, rect_intersects_any, subtract_rect, subtract_rects,
  },
  write::{clear_protocol_area, write_erase_sequences, write_protocol_overlay},
};
use crate::{ProtocolOverlay, ProtocolPlacement, RenderMode};

/// Tracks the protocol images on the terminal and turns each frame's overlay
/// list into the minimal set of writes and erases.
///
/// [`begin`](Self::begin) plans the frame and writes what must precede the
/// text flush; [`finish`](Self::finish) writes the rest after it.
/// [`ProtocolFrameRenderer`](crate::ProtocolFrameRenderer) drives both.
#[derive(Debug, Default)]
pub struct ProtocolOverlayRenderer {
  state: Vec<ProtocolOverlayState>,
}

/// A planned overlay update between [`ProtocolOverlayRenderer::begin`] and
/// [`ProtocolOverlayRenderer::finish`].
#[derive(Debug)]
pub struct ProtocolOverlayCommit<'a> {
  next_state: Vec<ProtocolOverlayState>,
  writes: Vec<ProtocolOverlayWrite<'a>>,
  removed_after_write: Vec<ProtocolOverlayState>,
  clear_areas: Vec<Rect>,
}

#[derive(Debug)]
struct ProtocolOverlayWrite<'a> {
  /// Position of `overlay` in the frame's overlay list.
  index: usize,
  overlay: &'a ProtocolOverlay,
  state: ProtocolOverlayState,
  clear_areas: Vec<Rect>,
  refresh: bool,
  prewritten: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProtocolOverlayState {
  area: Rect,
  mode: RenderMode,
  placement: Option<ProtocolPlacement>,
  fingerprint: u64,
  erase: Option<Arc<str>>,
}

impl ProtocolOverlayState {
  fn of(overlay: &ProtocolOverlay) -> Self {
    let image = &overlay.image;
    Self {
      area: overlay.area,
      mode: image.mode,
      placement: image.placement,
      fingerprint: image.fingerprint,
      erase: image.erase.clone(),
    }
  }

  fn is_kitty_unicode(&self) -> bool {
    is_kitty_unicode(self.placement.as_ref())
  }

  /// Whether `new` shows the same terminal image placement, possibly moved.
  fn same_placement(&self, new: &Self) -> bool {
    self.mode == new.mode
      && match (&self.placement, &new.placement) {
        (
          Some(ProtocolPlacement::KittyPlacement {
            image_id: old_image,
            placement_id: old_placement,
          }),
          Some(ProtocolPlacement::KittyPlacement {
            image_id: new_image,
            placement_id: new_placement,
          }),
        ) => old_image == new_image && old_placement == new_placement,
        (
          Some(ProtocolPlacement::KittyUnicode {
            image_id: old_image,
          }),
          Some(ProtocolPlacement::KittyUnicode {
            image_id: new_image,
          }),
        ) => old_image == new_image,
        _ => false,
      }
  }

  /// Whether `new` owns the same terminal resource (erasing one would erase
  /// the other).
  fn same_resource(&self, new: &Self) -> bool {
    self.mode == new.mode && self.erase.is_some() && self.erase == new.erase
  }
}

struct ProtocolOverlayUpdate<'a> {
  next_state: Vec<ProtocolOverlayState>,
  removed_after_write: Vec<ProtocolOverlayState>,
  writes: Vec<ProtocolOverlayWrite<'a>>,
  clear_areas: Vec<Rect>,
}

impl ProtocolOverlayRenderer {
  /// Plan a frame showing `overlays` and write the payloads that must reach
  /// the terminal before the frame's text (new kitty Unicode placeholder
  /// images, whose placeholder cells would otherwise show nothing).
  pub fn begin<'a>(
    &self,
    writer: &mut impl Write,
    overlays: &'a [ProtocolOverlay],
  ) -> Result<ProtocolOverlayCommit<'a>> {
    let update = self.update(overlays);
    self.commit_update(writer, update)
  }

  /// Like [`begin`](Self::begin), but images inside `preserve_areas` that no
  /// new overlay overlaps stay on the terminal (e.g. while their
  /// replacements are still rendering).
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
    mut update: ProtocolOverlayUpdate<'a>,
  ) -> Result<ProtocolOverlayCommit<'a>> {
    // U=1 placeholders must not reach the terminal before their image and
    // virtual placement exist. Write those protocol resources first; the
    // regular terminal diff that follows will then reveal the image without
    // a blank intermediate frame.
    for write in &mut update.writes {
      if is_kitty_unicode(write.overlay.image.placement.as_ref()) {
        write_protocol_overlay(writer, write.overlay, write.refresh)?;
        write.prewritten = true;
      }
    }
    Ok(ProtocolOverlayCommit {
      next_state: update.next_state,
      writes: update.writes,
      removed_after_write: update.removed_after_write,
      clear_areas: update.clear_areas,
    })
  }

  /// Write the remaining payloads of `commit` (call after the frame's text
  /// has been flushed), then erase the images the frame no longer shows.
  pub fn finish(
    &mut self,
    writer: &mut impl Write,
    commit: ProtocolOverlayCommit<'_>,
  ) -> Result<()> {
    for write in commit.writes {
      if !write.prewritten {
        write_protocol_overlay(writer, write.overlay, write.refresh)?;
      }
    }
    erase_states(writer, &commit.removed_after_write)?;
    self.state = commit.next_state;
    Ok(())
  }

  /// Erase every tracked image and blank its area.
  pub fn clear(&mut self, writer: &mut impl Write) -> Result<()> {
    let old_state = std::mem::take(&mut self.state);
    erase_states(writer, &old_state)?;
    for overlay in old_state {
      clear_protocol_area(writer, overlay.area)?;
    }
    Ok(())
  }

  /// Forget the images a full screen clear (such as Ratatui's resize) wiped
  /// out, erasing any kitty resources they held, so the next frame writes
  /// them again in full. Kitty Unicode placeholder images survive a clear:
  /// their virtual placement stays and the redraw restores their cells.
  pub(super) fn forget_cleared_images(&mut self, writer: &mut impl Write) -> Result<()> {
    let (kept, cleared): (Vec<_>, Vec<_>) = std::mem::take(&mut self.state)
      .into_iter()
      .partition(ProtocolOverlayState::is_kitty_unicode);
    self.state = kept;
    erase_states(writer, &cleared)
  }

  pub(super) fn is_empty(&self) -> bool {
    self.state.is_empty()
  }

  pub(super) fn areas(&self) -> impl Iterator<Item = Rect> + '_ {
    self.state.iter().map(|state| state.area)
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
      .map(ProtocolOverlayState::of)
      .collect::<Vec<_>>();
    let mut next_state = next.clone();
    for state in preserved {
      if !next_state.contains(&state) {
        next_state.push(state);
      }
    }

    if next_state == self.state {
      return ProtocolOverlayUpdate {
        next_state,
        removed_after_write: Vec::new(),
        writes: Vec::new(),
        clear_areas: Vec::new(),
      };
    }

    let removed_after_write = self.removed_states(&next_state);
    let clear_areas = self.vacated_areas(&next_state);
    let writes = order_overlay_writes(self.changed_writes(overlays, next), &self.state);
    ProtocolOverlayUpdate {
      next_state,
      removed_after_write,
      writes,
      clear_areas,
    }
  }

  /// Old images the next frame neither keeps nor moves; they are erased
  /// after the new payloads are written.
  fn removed_states(&self, next_state: &[ProtocolOverlayState]) -> Vec<ProtocolOverlayState> {
    self
      .state
      .iter()
      .filter(|state| {
        !next_state.contains(state)
          && !next_state
            .iter()
            .any(|next| state.same_placement(next) || state.same_resource(next))
      })
      .cloned()
      .collect()
  }

  /// Parts of changed old overlay areas that no next overlay covers; their
  /// text must be redrawn. U=1 areas are text already and redraw by
  /// themselves.
  fn vacated_areas(&self, next_state: &[ProtocolOverlayState]) -> Vec<Rect> {
    let next_areas = next_state
      .iter()
      .map(|state| state.area)
      .collect::<Vec<_>>();
    self
      .state
      .iter()
      .filter(|old| !next_state.contains(old) && !old.is_kitty_unicode())
      .flat_map(|old| subtract_rects(old.area, &next_areas))
      .collect()
  }

  /// Writes for overlays that are new or changed.
  ///
  /// Areas whose overlays survive unchanged keep their cells untouched
  /// (anti-flicker). Everything a fresh write covers gets pre-cleared so
  /// that transparent pixels never reveal stale cell content left behind
  /// by earlier frames.
  fn changed_writes<'a>(
    &self,
    overlays: &'a [ProtocolOverlay],
    next: Vec<ProtocolOverlayState>,
  ) -> Vec<ProtocolOverlayWrite<'a>> {
    let unchanged_old_areas = self
      .state
      .iter()
      .filter(|old| next.contains(old))
      .map(|state| state.area)
      .collect::<Vec<_>>();
    next
      .into_iter()
      .zip(overlays)
      .enumerate()
      .filter(|(_, (state, _))| !self.state.contains(state))
      .map(|(index, (state, overlay))| ProtocolOverlayWrite {
        index,
        overlay,
        clear_areas: if state.is_kitty_unicode() {
          Vec::new()
        } else {
          subtract_rects(overlay.area, &unchanged_old_areas)
        },
        refresh: overlay.image.refresh.is_some()
          && self
            .state
            .iter()
            .any(|old| old.same_placement(&state) && old.fingerprint == state.fingerprint),
        state,
        prewritten: false,
      })
      .collect()
  }
}

impl ProtocolOverlayCommit<'_> {
  /// Areas vacated by moved or removed images; their text must be redrawn.
  pub fn clear_areas(&self) -> &[Rect] {
    &self.clear_areas
  }

  /// Cells that must be flushed through the regular diff before the new
  /// overlay payloads are written, so transparent pixels expose the current
  /// frame's styled cells instead of stale content.
  pub(super) fn write_clear_areas(&self) -> impl Iterator<Item = Rect> + '_ {
    self
      .writes
      .iter()
      .flat_map(|write| write.clear_areas.iter().copied())
  }

  /// Whether [`ProtocolOverlayRenderer::finish`] writes overlay `index` of
  /// the frame in full (not just a re-placement).
  pub(super) fn writes_in_full(&self, index: usize) -> bool {
    self
      .writes
      .iter()
      .any(|write| write.index == index && !write.refresh)
  }
}

fn is_kitty_unicode(placement: Option<&ProtocolPlacement>) -> bool {
  matches!(placement, Some(ProtocolPlacement::KittyUnicode { .. }))
}

fn erase_states(writer: &mut impl Write, states: &[ProtocolOverlayState]) -> Result<()> {
  write_erase_sequences(writer, states.iter().map(|state| state.erase.as_deref()))
}

/// Order writes so that an image moving away from an area is only moved
/// after the image replacing it there has been written.
fn order_overlay_writes<'a>(
  writes: Vec<ProtocolOverlayWrite<'a>>,
  old_states: &[ProtocolOverlayState],
) -> Vec<ProtocolOverlayWrite<'a>> {
  if writes.len() < 2 {
    return writes;
  }

  let dependencies = write_dependencies(&writes, old_states);
  stable_topological_order(writes, &dependencies)
}

/// For each write, the writes covering an area it vacates.
fn write_dependencies(
  writes: &[ProtocolOverlayWrite<'_>],
  old_states: &[ProtocolOverlayState],
) -> Vec<Vec<usize>> {
  writes
    .iter()
    .enumerate()
    .map(|(write_index, write)| {
      let vacated = old_states
        .iter()
        .filter(|old| old.same_placement(&write.state) || old.same_resource(&write.state))
        .flat_map(|old| subtract_rect(old.area, write.state.area))
        .collect::<Vec<_>>();
      writes
        .iter()
        .enumerate()
        .filter(|(cover_index, cover)| {
          *cover_index != write_index
            && vacated
              .iter()
              .any(|area| rect_intersection(*area, cover.state.area).is_some())
        })
        .map(|(cover_index, _)| cover_index)
        .collect()
    })
    .collect()
}

/// Emit writes whose dependencies are satisfied, earliest first; writes
/// caught in a dependency cycle keep their original order at the end.
fn stable_topological_order<'a>(
  writes: Vec<ProtocolOverlayWrite<'a>>,
  dependencies: &[Vec<usize>],
) -> Vec<ProtocolOverlayWrite<'a>> {
  let len = writes.len();
  let mut emitted = vec![false; len];
  let mut order = Vec::with_capacity(len);
  while let Some(index) = (0..len).find(|&index| {
    !emitted[index]
      && dependencies[index]
        .iter()
        .all(|dependency| emitted[*dependency])
  }) {
    emitted[index] = true;
    order.push(index);
  }
  order.extend((0..len).filter(|index| !emitted[*index]));

  let mut slots = writes.into_iter().map(Some).collect::<Vec<_>>();
  order
    .into_iter()
    .filter_map(|index| slots[index].take())
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ProtocolImage;

  fn kitty_placement_state(
    area: Rect,
    ids: (u32, u32),
    fingerprint: u64,
    erase: &str,
  ) -> ProtocolOverlayState {
    ProtocolOverlayState {
      area,
      mode: RenderMode::Kitty,
      placement: Some(ProtocolPlacement::KittyPlacement {
        image_id: ids.0,
        placement_id: ids.1,
      }),
      fingerprint,
      erase: Some(erase.into()),
    }
  }

  fn kitty_placement_overlay(
    area: Rect,
    ids: (u32, u32),
    fingerprint: u64,
    data: &str,
    refresh: Option<&str>,
    erase: &str,
  ) -> ProtocolOverlay {
    ProtocolImage {
      mode: RenderMode::Kitty,
      data: data.into(),
      refresh: refresh.map(Arc::from),
      placement: Some(ProtocolPlacement::KittyPlacement {
        image_id: ids.0,
        placement_id: ids.1,
      }),
      fingerprint,
      erase: Some(erase.into()),
    }
    .overlay(area)
  }

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

  fn sixel_overlay(area: Rect, fingerprint: u64) -> ProtocolOverlay {
    crate::display::test_overlay(area, RenderMode::Sixel, "sixel", fingerprint)
  }

  #[test]
  fn kitty_placement_update_preclears_replaced_area() {
    let renderer = ProtocolOverlayRenderer {
      state: vec![kitty_placement_state(
        Rect::new(0, 0, 80, 23),
        (7, 11),
        1,
        "erase",
      )],
    };
    let overlays = vec![kitty_placement_overlay(
      Rect::new(0, 10, 80, 13),
      (7, 11),
      2,
      "place",
      None,
      "erase",
    )];

    let update = renderer.update(&overlays);

    assert!(update.removed_after_write.is_empty());
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
      state: vec![kitty_placement_state(
        Rect::new(0, 0, 80, 8),
        (1, 1),
        1,
        "erase-old",
      )],
    };
    let overlays = vec![
      kitty_placement_overlay(
        Rect::new(0, 0, 80, 2),
        (1, 1),
        2,
        "old-page",
        None,
        "erase-old",
      ),
      kitty_placement_overlay(
        Rect::new(0, 3, 80, 20),
        (2, 2),
        3,
        "new-page",
        None,
        "erase-new",
      ),
    ];

    let update = renderer.update(&overlays);

    assert_eq!(update.clear_areas, vec![Rect::new(0, 2, 80, 1)]);
    assert_eq!(update.writes.len(), 2);
    let old_page = update
      .writes
      .iter()
      .find(|write| &*write.overlay.image.data == "old-page")
      .expect("old page write");
    let new_page = update
      .writes
      .iter()
      .find(|write| &*write.overlay.image.data == "new-page")
      .expect("new page write");
    assert_eq!(old_page.clear_areas, vec![Rect::new(0, 0, 80, 2)]);
    assert_eq!(new_page.clear_areas, vec![Rect::new(0, 3, 80, 20)]);
  }

  #[test]
  fn unchanged_kitty_placement_is_not_rewritten() {
    let state = kitty_placement_state(Rect::new(0, 0, 80, 23), (7, 11), 1, "erase");
    let renderer = ProtocolOverlayRenderer {
      state: vec![state.clone()],
    };
    let overlays = vec![kitty_placement_overlay(
      state.area,
      (7, 11),
      state.fingerprint,
      "upload-and-place",
      Some("place"),
      "erase",
    )];

    let update = renderer.update(&overlays);

    assert!(update.removed_after_write.is_empty());
    assert!(update.clear_areas.is_empty());
    assert!(update.writes.is_empty());
  }

  #[test]
  fn moving_same_kitty_placement_uses_refresh_payload() {
    let mut renderer = ProtocolOverlayRenderer {
      state: vec![kitty_placement_state(
        Rect::new(0, 0, 80, 20),
        (7, 11),
        1,
        "erase",
      )],
    };
    let overlays = vec![kitty_placement_overlay(
      Rect::new(0, 1, 80, 20),
      (7, 11),
      1,
      "upload-and-place",
      Some("place-only"),
      "erase",
    )];

    let mut begin_output = Vec::new();
    let commit = renderer.begin(&mut begin_output, &overlays).unwrap();

    assert!(begin_output.is_empty());
    assert_eq!(commit.clear_areas(), &[Rect::new(0, 0, 80, 1)]);
    assert_eq!(commit.writes.len(), 1);
    assert!(commit.writes[0].refresh);
    assert!(!commit.writes_in_full(0));

    let mut finish_output = Vec::new();
    renderer.finish(&mut finish_output, commit).unwrap();
    let output = String::from_utf8(finish_output).unwrap();

    assert!(output.contains("place-only"));
    assert!(!output.contains("upload-and-place"));
  }

  #[test]
  fn moving_same_kitty_unicode_image_writes_no_protocol_data() {
    let mut renderer = ProtocolOverlayRenderer {
      state: vec![ProtocolOverlayState {
        area: Rect::new(0, 0, 80, 20),
        mode: RenderMode::Kitty,
        placement: Some(ProtocolPlacement::KittyUnicode { image_id: 7 }),
        fingerprint: 1,
        erase: Some("erase".into()),
      }],
    };
    let overlays = vec![
      ProtocolImage {
        mode: RenderMode::Kitty,
        data: "upload-and-place".into(),
        refresh: Some("virtual-place".into()),
        placement: Some(ProtocolPlacement::KittyUnicode { image_id: 7 }),
        fingerprint: 1,
        erase: Some("erase".into()),
      }
      .overlay(Rect::new(0, 1, 80, 20)),
    ];

    let mut output = Vec::new();
    let commit = renderer.begin(&mut output, &overlays).unwrap();
    renderer.finish(&mut output, commit).unwrap();

    assert!(output.is_empty());
    assert_eq!(renderer.state[0].area, Rect::new(0, 1, 80, 20));
  }

  #[test]
  fn new_kitty_unicode_image_is_written_during_begin() {
    let mut renderer = ProtocolOverlayRenderer::default();
    let overlays = vec![kitty_unicode_overlay(Rect::new(0, 0, 3, 1))];

    let mut begin_output = Vec::new();
    let commit = renderer.begin(&mut begin_output, &overlays).unwrap();

    assert!(String::from_utf8_lossy(&begin_output).contains("upload-and-place"));
    assert!(commit.clear_areas().is_empty());
    assert!(commit.writes[0].clear_areas.is_empty());
    assert!(commit.writes[0].prewritten);

    let mut finish_output = Vec::new();
    renderer.finish(&mut finish_output, commit).unwrap();
    assert!(finish_output.is_empty());
  }

  #[test]
  fn new_kitty_placement_uploads_before_place() {
    let mut renderer = ProtocolOverlayRenderer::default();
    let overlays = vec![kitty_placement_overlay(
      Rect::new(0, 0, 3, 1),
      (7, 11),
      1,
      "upload-only",
      Some("place-only"),
      "erase",
    )];

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
        kitty_placement_state(Rect::new(0, 0, 80, 10), (1, 1), 1, "erase-a"),
        kitty_placement_state(Rect::new(0, 10, 80, 10), (2, 2), 2, "erase-b"),
        kitty_placement_state(Rect::new(0, 20, 80, 10), (3, 3), 3, "erase-c"),
      ],
    };
    let overlays = vec![
      kitty_placement_overlay(
        Rect::new(0, 0, 80, 10),
        (2, 2),
        2,
        "upload-b",
        Some("place-b"),
        "erase-b",
      ),
      kitty_placement_overlay(
        Rect::new(0, 10, 80, 10),
        (3, 3),
        3,
        "upload-c",
        Some("place-c"),
        "erase-c",
      ),
      kitty_placement_overlay(
        Rect::new(0, 20, 80, 10),
        (4, 4),
        4,
        "upload-d",
        Some("place-d"),
        "erase-d",
      ),
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
    let old_ready_area = kitty_placement_state(Rect::new(0, 0, 3, 1), (1, 1), 1, "erase-ready");
    let old_pending_area = kitty_placement_state(Rect::new(0, 1, 3, 1), (2, 2), 2, "erase-pending");
    let renderer = ProtocolOverlayRenderer {
      state: vec![old_ready_area, old_pending_area.clone()],
    };
    let overlays = vec![kitty_placement_overlay(
      Rect::new(0, 0, 3, 1),
      (3, 3),
      3,
      "new-ready",
      None,
      "erase-new",
    )];

    let update = renderer.update_preserving(&overlays, &[Rect::new(0, 1, 3, 1)]);

    assert_eq!(update.writes.len(), 1);
    assert_eq!(&*update.writes[0].overlay.image.data, "new-ready");
    assert!(update.next_state.contains(&old_pending_area));
    assert!(update.next_state.iter().any(|state| state.fingerprint == 3));
    assert!(update.clear_areas.is_empty());
  }

  #[test]
  fn preserving_partial_overlap_does_not_keep_old_overlay() {
    let old_area = kitty_placement_state(Rect::new(0, 0, 10, 10), (1, 1), 1, "erase-old");
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
        erase: Some("erase-image-7".into()),
      }],
    };
    let overlays = vec![
      ProtocolImage {
        mode: RenderMode::Kitty,
        data: "same-image-new-position".into(),
        refresh: None,
        placement: None,
        fingerprint: 1,
        erase: Some("erase-image-7".into()),
      }
      .overlay(Rect::new(0, 1, 3, 1)),
    ];

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
    let overlays = vec![
      ProtocolImage {
        mode: RenderMode::Kitty,
        data: "place".into(),
        refresh: None,
        placement: Some(ProtocolPlacement::KittyPlacement {
          image_id: 7,
          placement_id: 11,
        }),
        fingerprint: 2,
        erase: None,
      }
      .overlay(Rect::new(0, 1, 3, 1)),
    ];

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
  fn screen_clear_forgets_pixel_images_but_keeps_placeholder_images() {
    let placement = kitty_placement_state(Rect::new(0, 0, 4, 2), (7, 11), 1, "erase-placement");
    let sixel = ProtocolOverlayState::of(&sixel_overlay(Rect::new(0, 2, 4, 2), 2));
    let unicode = ProtocolOverlayState::of(&kitty_unicode_overlay(Rect::new(0, 4, 4, 2)));
    let mut renderer = ProtocolOverlayRenderer {
      state: vec![placement, sixel, unicode.clone()],
    };

    let mut output = Vec::new();
    renderer.forget_cleared_images(&mut output).unwrap();

    assert_eq!(renderer.state, vec![unicode]);
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("erase-placement"));
    assert!(!output.contains("erase\u{1b}"));

    // The same sixel image in the same place is written again in full.
    let overlays = vec![sixel_overlay(Rect::new(0, 2, 4, 2), 2)];
    let commit = renderer.begin(&mut Vec::new(), &overlays).unwrap();
    assert!(commit.writes_in_full(0));
  }
}
