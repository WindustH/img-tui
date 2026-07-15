//! Reusable terminal image rendering primitives for ratatui applications.
//!
//! `ProtocolFrameRenderer` is the normal integration point: it wraps a ratatui
//! frame in a protocol image transaction, forcing text cells under removed
//! images to be redrawn, writing new image protocol payloads before ratatui
//! flushes the frame, and clearing stale image areas only after replacement
//! overlays have been queued. This avoids exposing blank intermediate frames.

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
