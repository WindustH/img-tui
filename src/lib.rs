//! Terminal image rendering for ratatui applications.
//!
//! - [`capability`] detects which image protocols the terminal supports
//!   (kitty graphics, sixel, iTerm2 inline images), including inside tmux and
//!   Zellij, and suggests [`RenderMode`]s to try.
//! - [`native_image`] decodes and scales images and encodes them as protocol
//!   escape sequences.
//! - [`display`] draws ratatui frames together with those images.
//!
//! [`ProtocolFrameRenderer`] is the normal integration point. Its
//! [`draw`](ProtocolFrameRenderer::draw) renders a frame and returns a list of
//! [`ProtocolOverlay`]s; the renderer uploads new kitty placeholder images
//! before the frame's text is flushed, writes the other image payloads right
//! after it (text cells under replaced or removed images are redrawn in the
//! same flush), and erases images that are gone only once their replacements
//! have been written. Unchanged images are not rewritten, so frames never
//! show a blank intermediate state.

pub mod capability;
pub mod display;
pub mod native_image;
pub mod protocol;

pub use capability::{
  ColorLevel, PixelProtocol, RENDER_MODES_ENV, RenderMode, TerminalCapability, TerminalProbe,
  detect, render_modes_override_from_env,
};
pub use display::{
  ProtocolFrameOutput, ProtocolFrameRenderer, ProtocolOverlayCommit, ProtocolOverlayRenderer,
  force_update_areas, reset_protocol_images,
};
pub use native_image::{
  NativeImageConfig, NativeImageViewport, PreparedNativeImage, erase_kitty_placement_sequence,
  erase_sequence,
};
pub use protocol::{ProtocolOverlay, ProtocolPlacement};
