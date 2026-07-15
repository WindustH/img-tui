//! Reusable terminal image rendering primitives for ratatui applications.
//!
//! `ProtocolFrameRenderer` is the normal integration point: it wraps a ratatui
//! frame in a two-phase protocol image transaction, clearing stale image areas,
//! forcing text cells under removed images to be redrawn, and writing new image
//! protocol payloads after ratatui swaps buffers to avoid a visible blank frame.

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
pub use native_image::{NativeImageConfig, PreparedNativeImage, erase_sequence};
pub use protocol::{ProtocolOverlay, ProtocolPlacement};
