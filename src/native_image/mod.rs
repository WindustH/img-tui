//! Native (in-process) image rendering for the kitty, sixel and iTerm2
//! protocols.
//!
//! [`prepare`] decodes an image file, converts it to 8-bit sRGB and scales it
//! to fit a cell area. [`encode_protocol`] turns a [`PreparedNativeImage`]
//! into the escape sequences of a [`ProtocolImage`](crate::ProtocolImage) for
//! any protocol, choosing how kitty images are uploaded and placed; the
//! `render_*` functions are the lower-level building blocks it uses.
//! Sequences are wrapped for tmux or screen passthrough when
//! [`NativeImageConfig::passthrough`] says so. CPU-heavy work runs on Tokio's
//! blocking thread pool.

mod encode;
mod envelope;
mod geometry;
mod iterm;
mod kitty;
mod scale;
mod sixel;
mod viewport;

use std::{path::Path, sync::Arc};

use anyhow::{Result, bail};
use image::{DynamicImage, ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};

pub(crate) use self::encode::KittyShow;
pub use self::{
  encode::{EncodedProtocolImage, ProtocolImageSpec, encode_protocol},
  kitty::{KittyImageUpload, erase_kitty_placement_sequence, erase_sequence, kitty_image_id},
};
use self::{envelope::ProtocolEnvelope, kitty::KittyTransmit};
use crate::RenderMode;
pub use geometry::KittyPlacementGeometry;

/// Terminal facts the encoders need, usually derived from
/// [`TerminalCapability`](crate::TerminalCapability).
#[derive(Debug, Clone)]
pub struct NativeImageConfig {
  /// Cell size in pixels; `(8, 16)` is assumed when unknown.
  pub cell_pixels: Option<(u16, u16)>,
  /// `Some("tmux")` or `Some("screen")` to wrap sequences for passthrough.
  pub passthrough: Option<String>,
  /// Display kitty images through Unicode placeholders (`U=1`).
  pub kitty_unicode_placeholders: bool,
}

/// A decoded image scaled for display, cheap to clone and reuse across
/// protocol encodings.
#[derive(Debug, Clone)]
pub struct PreparedNativeImage {
  image: Arc<DynamicImage>,
}

impl PreparedNativeImage {
  /// Actual decoded storage, used for weighted caches.
  pub fn byte_len(&self) -> u64 {
    self.image.as_bytes().len() as u64
  }
}

/// The visible window onto an image laid out over `full_*_cells`, starting
/// `left_cells`/`top_cells` into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NativeImageViewport {
  pub full_width_cells: u16,
  pub full_height_cells: u16,
  pub visible_width_cells: u16,
  pub visible_height_cells: u16,
  pub left_cells: u32,
  pub top_cells: u32,
}

/// [`prepare`] and [`render_prepared`] in one step.
pub async fn render(
  path: &Path,
  width_cells: u16,
  height_cells: u16,
  mode: RenderMode,
  config: &NativeImageConfig,
  image_id: Option<u32>,
) -> Result<Vec<u8>> {
  let prepared = prepare(path, width_cells, height_cells, config.cell_pixels).await?;
  render_prepared(&prepared, mode, config, image_id).await
}

/// [`prepare`] and [`render_prepared_viewport`] in one step.
pub async fn render_viewport(
  path: &Path,
  viewport: NativeImageViewport,
  mode: RenderMode,
  config: &NativeImageConfig,
  image_id: Option<u32>,
  placement_id: Option<u32>,
  include_kitty_upload: bool,
) -> Result<Vec<u8>> {
  let prepared = prepare_cell_grid(
    path,
    viewport.full_width_cells,
    viewport.full_height_cells,
    config.cell_pixels,
  )
  .await?;
  render_prepared_viewport(
    &prepared,
    viewport,
    mode,
    config,
    image_id,
    placement_id,
    include_kitty_upload,
  )
  .await
}

/// Decode `path` and scale it to fit `width_cells` x `height_cells` (never
/// enlarging it), applying EXIF orientation and embedded ICC profiles.
pub async fn prepare(
  path: &Path,
  width_cells: u16,
  height_cells: u16,
  cell_pixels: Option<(u16, u16)>,
) -> Result<PreparedNativeImage> {
  let path = path.to_path_buf();
  let image =
    blocking(move || scale::load_scaled(&path, width_cells, height_cells, cell_pixels)).await?;
  Ok(PreparedNativeImage {
    image: Arc::new(image),
  })
}

/// Decode an image onto an exact terminal-cell pixel grid, including upscaling.
///
/// The caller must first fit the cell box to the image's aspect ratio. Unlike
/// [`prepare`], this fills every pixel of that box, avoiding fractional cell
/// boundaries when a full image is cropped into adjacent scroll slices.
/// EXIF orientation and color management match [`prepare`].
pub async fn prepare_cell_grid(
  path: &Path,
  width_cells: u16,
  height_cells: u16,
  cell_pixels: Option<(u16, u16)>,
) -> Result<PreparedNativeImage> {
  let path = path.to_path_buf();
  let image =
    blocking(move || scale::load_cell_grid(&path, width_cells, height_cells, cell_pixels)).await?;
  Ok(PreparedNativeImage {
    image: Arc::new(image),
  })
}

/// Escape sequences that display `prepared` at the cursor.
///
/// Kitty images are transmitted and displayed in one command; `image_id`
/// defaults to 1. `kitty_unicode_placeholders` is not used here: placeholder
/// images need a [`ProtocolPlacement::KittyUnicode`](crate::ProtocolPlacement)
/// overlay, which [`encode_protocol`] and
/// [`ProtocolImage`](crate::ProtocolImage) set up. Text modes are rejected.
pub async fn render_prepared(
  prepared: &PreparedNativeImage,
  mode: RenderMode,
  config: &NativeImageConfig,
  image_id: Option<u32>,
) -> Result<Vec<u8>> {
  let image = prepared.image.clone();
  let envelope = ProtocolEnvelope::new(config.passthrough.as_deref());
  match mode {
    RenderMode::Kitty => {
      let image_id = image_id.unwrap_or(1);
      blocking(move || {
        Ok(kitty::encode_image(
          &image,
          image_id,
          KittyTransmit::Display,
          envelope,
        ))
      })
      .await
    }
    RenderMode::Iterm2 => blocking(move || iterm::encode(&image, envelope)).await,
    RenderMode::Sixel => blocking(move || sixel::encode(&image, envelope)).await,
    RenderMode::Symbols | RenderMode::Ascii => bail!("{} is not a native image mode", mode.label()),
  }
}

/// Escape sequences that display the `viewport` part of `prepared`.
///
/// For kitty this places the image (optionally uploading it first) with
/// `placement_id`; sixel and iTerm2 images are cropped and re-encoded.
pub async fn render_prepared_viewport(
  prepared: &PreparedNativeImage,
  viewport: NativeImageViewport,
  mode: RenderMode,
  config: &NativeImageConfig,
  image_id: Option<u32>,
  placement_id: Option<u32>,
  include_kitty_upload: bool,
) -> Result<Vec<u8>> {
  match mode {
    RenderMode::Kitty => {
      let image_id = image_id.unwrap_or(1);
      let mut out = if include_kitty_upload {
        render_prepared_kitty_upload(prepared, config, image_id)
          .await?
          .data
      } else {
        Vec::new()
      };
      out.extend(render_kitty_viewport_from_geometry(
        prepared.image.width(),
        prepared.image.height(),
        config.cell_pixels,
        viewport,
        config,
        image_id,
        placement_id.unwrap_or(1),
      )?);
      Ok(out)
    }
    RenderMode::Iterm2 | RenderMode::Sixel => {
      let cropped = crop_prepared(prepared, viewport).await?;
      render_prepared(&cropped, mode, config, image_id).await
    }
    RenderMode::Symbols | RenderMode::Ascii => bail!("{} is not a native image mode", mode.label()),
  }
}

/// Encode a visible slice directly from shared decoded pixels, without a PNG
/// file round trip. `spec` must describe the visible cell dimensions.
pub async fn encode_protocol_viewport(
  prepared: &PreparedNativeImage,
  viewport: NativeImageViewport,
  spec: &ProtocolImageSpec,
  config: &NativeImageConfig,
) -> Result<EncodedProtocolImage> {
  let cropped = crop_prepared(prepared, viewport).await?;
  encode_protocol(&cropped, spec, config).await
}

/// Upload `prepared` to the terminal under `image_id` without displaying it.
/// Place it with [`render_kitty_viewport_from_upload`] or
/// [`render_kitty_virtual_placement`].
pub async fn render_prepared_kitty_upload(
  prepared: &PreparedNativeImage,
  config: &NativeImageConfig,
  image_id: u32,
) -> Result<KittyImageUpload> {
  let image = prepared.image.clone();
  let envelope = ProtocolEnvelope::new(config.passthrough.as_deref());
  let image_id = image_id.max(1);
  let data = blocking(move || {
    Ok(kitty::encode_image(
      &image,
      image_id,
      KittyTransmit::UploadOnly,
      envelope,
    ))
  })
  .await?;
  Ok(KittyImageUpload {
    data,
    image_id,
    pixel_width: prepared.image.width(),
    pixel_height: prepared.image.height(),
    cell_pixels: config.cell_pixels,
  })
}

/// Placement command showing the `viewport` part of an uploaded image at the
/// cursor, below text, as `placement_id`.
pub fn render_kitty_viewport_from_upload(
  upload: &KittyImageUpload,
  viewport: NativeImageViewport,
  config: &NativeImageConfig,
  placement_id: u32,
) -> Result<Vec<u8>> {
  render_kitty_viewport_from_geometry(
    upload.pixel_width,
    upload.pixel_height,
    upload.cell_pixels,
    viewport,
    config,
    upload.image_id,
    placement_id,
  )
}

/// Like [`render_kitty_viewport_from_upload`] for an image of the given pixel
/// size. `cell_pixels` is accepted for compatibility and not used: the image
/// is assumed to span the viewport's full cell size.
pub fn render_kitty_viewport_from_geometry(
  pixel_width: u32,
  pixel_height: u32,
  _cell_pixels: Option<(u16, u16)>,
  viewport: NativeImageViewport,
  config: &NativeImageConfig,
  image_id: u32,
  placement_id: u32,
) -> Result<Vec<u8>> {
  let envelope = ProtocolEnvelope::new(config.passthrough.as_deref());
  let pixels = viewport::pixel_viewport((pixel_width, pixel_height), viewport)?;
  Ok(kitty::placement(
    pixels,
    viewport,
    envelope,
    image_id.max(1),
    placement_id.max(1),
  ))
}

/// Encode a *virtual* placement (kitty `U=1`): an invisible prototype scaling
/// the image to `cols` x `rows` cells. The image appears wherever Unicode
/// placeholder cells reference `image_id`, which
/// [`ProtocolFrameRenderer`](crate::ProtocolFrameRenderer) draws for overlays
/// with [`ProtocolPlacement::KittyUnicode`](crate::ProtocolPlacement::KittyUnicode).
pub fn render_kitty_virtual_placement(
  config: &NativeImageConfig,
  image_id: u32,
  cols: u16,
  rows: u16,
) -> Vec<u8> {
  let envelope = ProtocolEnvelope::new(config.passthrough.as_deref());
  kitty::virtual_placement(envelope, image_id, cols, rows)
}

/// PNG of the `viewport` part of `path` scaled to the viewport's full size.
pub async fn encode_viewport_png(
  path: &Path,
  viewport: NativeImageViewport,
  cell_pixels: Option<(u16, u16)>,
) -> Result<Vec<u8>> {
  let prepared = prepare_cell_grid(
    path,
    viewport.full_width_cells,
    viewport.full_height_cells,
    cell_pixels,
  )
  .await?;
  encode_prepared_viewport_png(&prepared, viewport, cell_pixels).await
}

/// PNG of the `viewport` part of `prepared`.
pub async fn encode_prepared_viewport_png(
  prepared: &PreparedNativeImage,
  viewport: NativeImageViewport,
  _cell_pixels: Option<(u16, u16)>,
) -> Result<Vec<u8>> {
  let image = prepared.image.clone();
  blocking(move || {
    let rgba = viewport::crop(&image, viewport)?.to_rgba8();
    let mut out = Vec::new();
    PngEncoder::new(&mut out).write_image(
      rgba.as_raw(),
      rgba.width(),
      rgba.height(),
      ExtendedColorType::Rgba8,
    )?;
    Ok(out)
  })
  .await
}

async fn crop_prepared(
  prepared: &PreparedNativeImage,
  viewport: NativeImageViewport,
) -> Result<PreparedNativeImage> {
  let image = prepared.image.clone();
  let cropped = blocking(move || viewport::crop(&image, viewport)).await?;
  Ok(PreparedNativeImage {
    image: Arc::new(cropped),
  })
}

async fn blocking<T, F>(work: F) -> Result<T>
where
  T: Send + 'static,
  F: FnOnce() -> Result<T> + Send + 'static,
{
  tokio::task::spawn_blocking(work).await?
}
