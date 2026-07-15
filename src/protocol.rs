use ratatui::layout::Rect;

use crate::RenderMode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolPlacement {
  KittyUnicode { image_id: u32 },
}

#[derive(Debug, Clone)]
pub struct ProtocolOverlay {
  pub area: Rect,
  pub mode: RenderMode,
  pub data: String,
  pub placement: Option<ProtocolPlacement>,
  pub fingerprint: u64,
  pub erase: Option<String>,
}
