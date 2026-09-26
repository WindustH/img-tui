//! Description of one protocol image in a frame.

use ratatui::layout::Rect;

use crate::RenderMode;

/// How a kitty image is positioned on the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolPlacement {
  /// Virtual placement shown through Unicode placeholder cells, which
  /// [`ProtocolFrameRenderer`](crate::ProtocolFrameRenderer) fills in.
  KittyUnicode { image_id: u32 },
  /// Regular placement; moving it only needs a new placement command.
  KittyPlacement { image_id: u32, placement_id: u32 },
}

/// An image to show in a frame.
#[derive(Debug, Clone)]
pub struct ProtocolOverlay {
  /// Cells the image covers.
  pub area: Rect,
  pub mode: RenderMode,
  /// Escape sequences that draw the image, written with the cursor at the
  /// top-left of `area`. For [`ProtocolPlacement::KittyPlacement`] this is
  /// the upload only and `refresh` holds the placement.
  pub data: String,
  /// Lighter sequence that shows an image already on the terminal again
  /// (a kitty placement or virtual placement), used when only its position
  /// changes.
  pub refresh: Option<String>,
  pub placement: Option<ProtocolPlacement>,
  /// Identifies the image content; a different value forces a rewrite.
  pub fingerprint: u64,
  /// Sequence that deletes the image from the terminal (kitty only).
  pub erase: Option<String>,
}
