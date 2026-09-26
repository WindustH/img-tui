//! Saturating rectangle arithmetic on terminal cell coordinates.

use ratatui::layout::Rect;

pub(super) fn rect_right(area: Rect) -> u16 {
  area.x.saturating_add(area.width)
}

pub(super) fn rect_bottom(area: Rect) -> u16 {
  area.y.saturating_add(area.height)
}

pub(super) fn rect_is_empty(area: Rect) -> bool {
  area.width == 0 || area.height == 0
}

pub(super) fn rect_contains_position(area: Rect, x: u16, y: u16) -> bool {
  x >= area.x && x < rect_right(area) && y >= area.y && y < rect_bottom(area)
}

pub(super) fn rect_contains(outer: Rect, inner: Rect) -> bool {
  inner.x >= outer.x
    && inner.y >= outer.y
    && rect_right(inner) <= rect_right(outer)
    && rect_bottom(inner) <= rect_bottom(outer)
}

pub(super) fn rect_intersection(left: Rect, right: Rect) -> Option<Rect> {
  let x1 = left.x.max(right.x);
  let y1 = left.y.max(right.y);
  let x2 = rect_right(left).min(rect_right(right));
  let y2 = rect_bottom(left).min(rect_bottom(right));
  let width = x2.saturating_sub(x1);
  let height = y2.saturating_sub(y1);
  (width > 0 && height > 0).then_some(Rect::new(x1, y1, width, height))
}

pub(super) fn rect_intersects_any(area: Rect, clips: &[Rect]) -> bool {
  clips
    .iter()
    .any(|clip| rect_intersection(area, *clip).is_some())
}

pub(super) fn rect_contained_by_any(area: Rect, clips: &[Rect]) -> bool {
  clips.iter().any(|clip| rect_contains(*clip, area))
}

/// Pairwise intersections of `areas` with `clips`.
pub(super) fn intersect_rects(areas: &[Rect], clips: &[Rect]) -> Vec<Rect> {
  areas
    .iter()
    .flat_map(|area| {
      clips
        .iter()
        .filter_map(move |clip| rect_intersection(*area, *clip))
    })
    .collect()
}

/// Parts of `area` not covered by any of `covers`.
pub(super) fn subtract_rects(area: Rect, covers: &[Rect]) -> Vec<Rect> {
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

/// Parts of `area` outside `cover`: up to four bands (above, below, left,
/// right of the overlap).
pub(super) fn subtract_rect(area: Rect, cover: Rect) -> Vec<Rect> {
  let Some(intersection) = rect_intersection(area, cover) else {
    return vec![area];
  };

  let area_right = rect_right(area);
  let area_bottom = rect_bottom(area);
  let intersection_right = rect_right(intersection);
  let intersection_bottom = rect_bottom(intersection);
  [
    Rect::new(
      area.x,
      area.y,
      area.width,
      intersection.y.saturating_sub(area.y),
    ),
    Rect::new(
      area.x,
      intersection_bottom,
      area.width,
      area_bottom.saturating_sub(intersection_bottom),
    ),
    Rect::new(
      area.x,
      intersection.y,
      intersection.x.saturating_sub(area.x),
      intersection.height,
    ),
    Rect::new(
      intersection_right,
      intersection.y,
      area_right.saturating_sub(intersection_right),
      intersection.height,
    ),
  ]
  .into_iter()
  .filter(|rect| !rect_is_empty(*rect))
  .collect()
}

#[cfg(test)]
mod tests {
  use super::{Rect, subtract_rect, subtract_rects};

  #[test]
  fn subtracting_a_centered_cover_leaves_four_bands() {
    let parts = subtract_rect(Rect::new(0, 0, 10, 10), Rect::new(3, 3, 4, 4));
    assert_eq!(
      parts,
      vec![
        Rect::new(0, 0, 10, 3),
        Rect::new(0, 7, 10, 3),
        Rect::new(0, 3, 3, 4),
        Rect::new(7, 3, 3, 4),
      ]
    );
    let area = parts.iter().map(|rect| rect.area()).sum::<u32>();
    assert_eq!(area, 100 - 16);
  }

  #[test]
  fn subtracting_full_cover_leaves_nothing() {
    assert!(subtract_rects(Rect::new(2, 2, 3, 3), &[Rect::new(0, 0, 10, 10)]).is_empty());
    assert_eq!(
      subtract_rects(Rect::new(0, 0, 4, 1), &[Rect::new(8, 8, 1, 1)]),
      vec![Rect::new(0, 0, 4, 1)]
    );
  }
}
