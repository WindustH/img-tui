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
  let prepared = prepare(
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
  let prepared = prepare(
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

#[cfg(test)]
mod tests {
  use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
  };

  use image::{ImageBuffer, Rgb, Rgba, RgbaImage};

  use super::*;

  fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
      .build()
      .expect("runtime")
  }

  #[test]
  fn render_viewport_supports_protocol_modes() {
    runtime().block_on(async {
      let path = test_image_path("png");
      write_test_image(&path);

      let config = NativeImageConfig {
        cell_pixels: Some((4, 8)),
        passthrough: None,
        kitty_unicode_placeholders: false,
      };
      let viewport = NativeImageViewport {
        full_width_cells: 4,
        full_height_cells: 4,
        visible_width_cells: 4,
        visible_height_cells: 2,
        left_cells: 0,
        top_cells: 1,
      };

      let kitty = render_viewport(
        &path,
        viewport,
        RenderMode::Kitty,
        &config,
        Some(42),
        Some(7),
        true,
      )
      .await
      .expect("kitty viewport render");
      let kitty = String::from_utf8(kitty).expect("kitty output is utf-8");
      assert!(kitty.contains("a=t"));
      assert!(kitty.contains("a=p"));
      assert!(kitty.contains("i=42"));
      assert!(kitty.contains("p=7"));
      assert!(kitty.contains("x=0"));
      assert!(kitty.contains("y=8"));
      assert!(kitty.contains("c=4"));
      assert!(kitty.contains("r=2"));

      let prepared = prepare(&path, 4, 4, config.cell_pixels)
        .await
        .expect("prepare kitty upload source");
      let upload = render_prepared_kitty_upload(&prepared, &config, 99)
        .await
        .expect("kitty upload");
      let placement = render_kitty_viewport_from_upload(&upload, viewport, &config, 13)
        .expect("kitty upload placement");
      let placement = String::from_utf8(placement).expect("placement is utf-8");
      assert!(!placement.contains("a=t"));
      assert!(placement.contains("a=p"));
      assert!(placement.contains("i=99"));
      assert!(placement.contains("p=13"));
      assert!(placement.contains("x=0"));
      assert!(placement.contains("y=8"));

      let sixel = render_viewport(
        &path,
        viewport,
        RenderMode::Sixel,
        &config,
        None,
        None,
        false,
      )
      .await
      .expect("sixel viewport render");
      assert!(sixel.starts_with(b"\x1bP9;1q"));

      let iterm = render_viewport(
        &path,
        viewport,
        RenderMode::Iterm2,
        &config,
        None,
        None,
        false,
      )
      .await
      .expect("iterm2 viewport render");
      let iterm = String::from_utf8(iterm).expect("iterm output is utf-8");
      assert!(iterm.starts_with("\x1b]1337;File=inline=1;"));
      assert!(iterm.contains("width=16px;height=16px"));

      let _ = fs::remove_file(path);
    });
  }

  #[test]
  fn kitty_viewport_uses_proportional_pixels_when_not_upscaled() {
    runtime().block_on(async {
      let path = test_image_path("png");
      write_test_image(&path);

      let config = NativeImageConfig {
        cell_pixels: Some((4, 8)),
        passthrough: None,
        kitty_unicode_placeholders: false,
      };
      let viewport = NativeImageViewport {
        full_width_cells: 8,
        full_height_cells: 8,
        visible_width_cells: 8,
        visible_height_cells: 4,
        left_cells: 0,
        top_cells: 4,
      };

      let prepared = prepare(&path, 8, 8, config.cell_pixels)
        .await
        .expect("prepare kitty upload source");
      let upload = render_prepared_kitty_upload(&prepared, &config, 99)
        .await
        .expect("kitty upload");
      assert_eq!(upload.pixel_width, 16);
      assert_eq!(upload.pixel_height, 32);

      let placement = render_kitty_viewport_from_upload(&upload, viewport, &config, 13)
        .expect("kitty upload placement");
      let placement = String::from_utf8(placement).expect("placement is utf-8");
      assert!(placement.contains("x=0"));
      assert!(placement.contains("y=16"));
      assert!(placement.contains("w=16"));
      assert!(placement.contains("h=16"));
      assert!(placement.contains("c=8"));
      assert!(placement.contains("r=4"));

      let _ = fs::remove_file(path);
    });
  }

  #[test]
  fn sixteen_bit_images_render_in_every_protocol() {
    runtime().block_on(async {
      let path = test_image_path("16bit.png");
      ImageBuffer::from_fn(24, 12, |x, y| {
        Rgb([(x * 2000) as u16, (y * 5000) as u16, 40000_u16])
      })
      .save(&path)
      .expect("save 16-bit test image");
      let config = NativeImageConfig {
        cell_pixels: Some((4, 8)),
        passthrough: None,
        kitty_unicode_placeholders: false,
      };

      let prepared = prepare(&path, 3, 1, config.cell_pixels)
        .await
        .expect("prepare 16-bit image");
      for mode in [RenderMode::Kitty, RenderMode::Sixel, RenderMode::Iterm2] {
        let out = render_prepared(&prepared, mode, &config, Some(3))
          .await
          .unwrap_or_else(|error| panic!("{} render failed: {error}", mode.label()));
        assert!(!out.is_empty());
      }

      let _ = fs::remove_file(path);
    });
  }

  fn test_image_path(suffix: &str) -> PathBuf {
    let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .expect("clock")
      .as_nanos();
    std::env::temp_dir().join(format!("img-tui-viewport-test-{nanos}.{suffix}"))
  }

  fn write_test_image(path: &Path) {
    let mut image = RgbaImage::new(16, 32);
    for y in 0..32 {
      for x in 0..16 {
        image.put_pixel(
          x,
          y,
          Rgba([(x * 13) as u8, (y * 7) as u8, ((x + y) * 5) as u8, 255]),
        );
      }
    }
    image.save(path).expect("save test image");
  }
}
