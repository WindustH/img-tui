use std::{
  collections::HashMap,
  fmt::Write as FmtWrite,
  hash::{DefaultHasher, Hash, Hasher},
  io::Write as IoWrite,
  path::Path,
  sync::{Arc, Mutex, OnceLock},
  time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use fast_image_resize::{
  FilterType as FirFilterType, PixelType, ResizeAlg, ResizeOptions, Resizer,
  images::{Image as FirImage, ImageRef as FirImageRef},
};
use image::{
  ColorType, DynamicImage, ExtendedColorType, GrayAlphaImage, GrayImage, ImageBuffer, ImageDecoder,
  ImageEncoder, ImageReader, RgbImage, RgbaImage,
  codecs::{jpeg::JpegEncoder, png::PngEncoder},
  imageops::FilterType,
  metadata::{Cicp, Orientation},
};
use moxcms::{
  CicpColorPrimaries, ColorProfile, DataColorSpace, Layout, TransferCharacteristics,
  TransformOptions,
};
use palette::{Srgb, cast::ComponentsAs};
use quantette::{
  PaletteSize,
  color_map::IndexedColorMap,
  wu::{BinnerU8x3, WuU8x3},
};
use rayon::prelude::*;

use crate::RenderMode;

#[derive(Debug, Clone)]
pub struct NativeImageConfig {
  pub cell_pixels: Option<(u16, u16)>,
  pub passthrough: Option<String>,
  pub kitty_unicode_placeholders: bool,
}

#[derive(Debug, Clone)]
pub struct PreparedNativeImage {
  image: Arc<DynamicImage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NativeImageViewport {
  pub full_width_cells: u16,
  pub full_height_cells: u16,
  pub visible_width_cells: u16,
  pub visible_height_cells: u16,
  pub left_cells: u32,
  pub top_cells: u32,
}

#[derive(Debug, Clone)]
pub struct KittyImageUpload {
  pub data: Vec<u8>,
  pub image_id: u32,
  pub pixel_width: u32,
  pub pixel_height: u32,
  pub cell_pixels: Option<(u16, u16)>,
}

#[derive(Debug, Clone)]
struct ProtocolEnvelope {
  start: &'static str,
  escape: &'static str,
  close: &'static str,
}

#[derive(Debug)]
struct KittyImageIdRegistry {
  next: u32,
  by_key: HashMap<Vec<u8>, u32>,
}

/// Allocate a process-unique kitty image id for a stable resource key.
///
/// Unicode placeholders can carry the full 32-bit id (the low 24 bits in
/// their foreground color and the high byte in a third diacritic). Keeping a
/// registry avoids birthday collisions when an application, such as a PDF
/// viewer, has thousands of images alive in one terminal session. A
/// per-process starting point also prevents stale virtual placements left by
/// a crashed earlier process from being selected for a new image.
pub fn kitty_image_id(key: &[u8]) -> u32 {
  static IDS: OnceLock<Mutex<KittyImageIdRegistry>> = OnceLock::new();
  let ids = IDS.get_or_init(|| {
    let mut hasher = DefaultHasher::new();
    std::process::id().hash(&mut hasher);
    SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .unwrap_or_default()
      .as_nanos()
      .hash(&mut hasher);
    let next = (hasher.finish() as u32).max(1);
    Mutex::new(KittyImageIdRegistry {
      next,
      by_key: HashMap::new(),
    })
  });
  let mut ids = ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
  if let Some(id) = ids.by_key.get(key) {
    return *id;
  }
  let id = ids.next.max(1);
  ids.next = id.wrapping_add(1).max(1);
  ids.by_key.insert(key.to_vec(), id);
  id
}

impl ProtocolEnvelope {
  fn new(passthrough: Option<&str>) -> Self {
    match passthrough {
      Some("tmux") => Self {
        start: "\x1bPtmux;\x1b\x1b",
        escape: "\x1b\x1b",
        close: "\x1b\\",
      },
      Some("screen") => Self {
        start: "\x1bP\x1b",
        escape: "\x1b",
        close: "\x1b\\",
      },
      _ => Self {
        start: "\x1b",
        escape: "\x1b",
        close: "",
      },
    }
  }
}

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

pub async fn prepare(
  path: &Path,
  width_cells: u16,
  height_cells: u16,
  cell_pixels: Option<(u16, u16)>,
) -> Result<PreparedNativeImage> {
  let image = scale_to_fit(path, width_cells, height_cells, cell_pixels).await?;
  Ok(PreparedNativeImage {
    image: Arc::new(image),
  })
}

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
      encode_kitty(
        image,
        &envelope,
        image_id.unwrap_or(1),
        config.kitty_unicode_placeholders,
      )
      .await
    }
    RenderMode::Iterm2 => encode_iterm(image, &envelope).await,
    RenderMode::Sixel => encode_sixel(image, &envelope).await,
    RenderMode::Symbols | RenderMode::Ascii => bail!("{} is not a native image mode", mode.label()),
  }
}

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
      let cropped = crop_prepared(prepared, viewport, config.cell_pixels).await?;
      render_prepared(&cropped, mode, config, image_id).await
    }
    RenderMode::Symbols | RenderMode::Ascii => bail!("{} is not a native image mode", mode.label()),
  }
}

pub async fn render_prepared_kitty_upload(
  prepared: &PreparedNativeImage,
  config: &NativeImageConfig,
  image_id: u32,
) -> Result<KittyImageUpload> {
  let image = prepared.image.clone();
  let envelope = ProtocolEnvelope::new(config.passthrough.as_deref());
  let cell_pixels = config.cell_pixels;
  tokio::task::spawn_blocking(move || {
    let data = encode_kitty_image(
      image.as_ref(),
      image_id.max(1),
      KittyTransmit::UploadOnly,
      &envelope,
    )?;
    Ok(KittyImageUpload {
      data,
      image_id: image_id.max(1),
      pixel_width: image.width(),
      pixel_height: image.height(),
      cell_pixels,
    })
  })
  .await?
}

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

pub fn render_kitty_viewport_from_geometry(
  pixel_width: u32,
  pixel_height: u32,
  cell_pixels: Option<(u16, u16)>,
  viewport: NativeImageViewport,
  config: &NativeImageConfig,
  image_id: u32,
  placement_id: u32,
) -> Result<Vec<u8>> {
  let envelope = ProtocolEnvelope::new(config.passthrough.as_deref());
  let pixels = pixel_viewport_for_size(pixel_width, pixel_height, viewport, cell_pixels)?;
  encode_kitty_placement(
    pixels,
    viewport,
    &envelope,
    image_id.max(1),
    placement_id.max(1),
  )
}

/// Encode a *virtual* placement (kitty U=1): an invisible prototype scaling
/// the image to `cols` x `rows` cells. Actual display happens via unicode
/// placeholder text cells referencing `image_id` (see display.rs).
pub fn render_kitty_virtual_placement(
  config: &NativeImageConfig,
  image_id: u32,
  cols: u16,
  rows: u16,
) -> Vec<u8> {
  let envelope = ProtocolEnvelope::new(config.passthrough.as_deref());
  let mut out = Vec::new();
  let _ = write!(
    out,
    "{}_Gq=2,a=p,U=1,i={},c={},r={}{}\\{}",
    envelope.start,
    image_id.max(1),
    cols.max(1),
    rows.max(1),
    envelope.escape,
    envelope.close
  );
  out
}

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

pub fn erase_sequence(
  mode: RenderMode,
  passthrough: Option<&str>,
  image_id: Option<u32>,
) -> Option<String> {
  if mode != RenderMode::Kitty {
    return None;
  }
  let envelope = ProtocolEnvelope::new(passthrough);
  Some(match image_id {
    Some(image_id) => format!(
      "{}_Gq=2,a=d,d=i,i={image_id}{}\\{}",
      envelope.start, envelope.escape, envelope.close
    ),
    None => format!(
      "{}_Gq=2,a=d,d=A{}\\{}",
      envelope.start, envelope.escape, envelope.close
    ),
  })
}

pub fn erase_kitty_placement_sequence(
  passthrough: Option<&str>,
  image_id: u32,
  placement_id: u32,
) -> Option<String> {
  let envelope = ProtocolEnvelope::new(passthrough);
  Some(format!(
    "{}_Gq=2,a=d,d=i,i={image_id},p={placement_id}{}\\{}",
    envelope.start, envelope.escape, envelope.close
  ))
}

async fn scale_to_fit(
  path: &Path,
  width_cells: u16,
  height_cells: u16,
  cell_pixels: Option<(u16, u16)>,
) -> Result<DynamicImage> {
  let path = path.to_path_buf();
  tokio::task::spawn_blocking(move || {
    let decoded = decode_image(&path)?;
    let (cell_width, cell_height) = cell_pixels.unwrap_or((8, 16));
    let max_width = u32::from(width_cells.max(1)) * u32::from(cell_width.max(1));
    let max_height = u32::from(height_cells.max(1)) * u32::from(cell_height.max(1));
    let (max_width, max_height) = flip_size(decoded.orientation, (max_width, max_height));
    let (target_width, target_height) = fit_pixel_size(decoded.size, (max_width, max_height));

    let mut image = decoded.image;
    if image.width() != target_width || image.height() != target_height {
      let filter = if target_width > image.width() || target_height > image.height() {
        FilterType::CatmullRom
      } else {
        FilterType::Triangle
      };
      image = resize_exact_fast(image, target_width, target_height, filter);
    }
    if decoded.orientation != Orientation::NoTransforms {
      image.apply_orientation(decoded.orientation);
    }
    Ok(image)
  })
  .await?
}

struct DecodedImage {
  image: DynamicImage,
  orientation: Orientation,
  size: (u32, u32),
}

fn fit_pixel_size(image_size: (u32, u32), bounds: (u32, u32)) -> (u32, u32) {
  let (image_width, image_height) = (image_size.0.max(1), image_size.1.max(1));
  let (max_width, max_height) = (bounds.0.max(1), bounds.1.max(1));
  let scale = (max_width as f64 / image_width as f64)
    .min(max_height as f64 / image_height as f64)
    .min(1.0);
  let target_width = ((image_width as f64 * scale).round() as u32).clamp(1, max_width);
  let target_height = ((image_height as f64 * scale).round() as u32).clamp(1, max_height);
  (target_width, target_height)
}

fn decode_image(path: &Path) -> Result<DecodedImage> {
  let reader = ImageReader::open(path)
    .with_context(|| format!("failed to open {}", path.display()))?
    .with_guessed_format()
    .with_context(|| format!("failed to guess image format for {}", path.display()))?;
  let mut decoder = reader
    .into_decoder()
    .with_context(|| format!("failed to decode {}", path.display()))?;
  let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
  let size = decoder.dimensions();
  let image = decode_image_pixels(decoder)
    .with_context(|| format!("failed to read image pixels from {}", path.display()))?;
  Ok(DecodedImage {
    image,
    orientation,
    size,
  })
}

fn decode_image_pixels(mut decoder: impl ImageDecoder) -> Result<DynamicImage> {
  if let Some(layout) = color_type_to_moxcms_layout(decoder.color_type())
    && let Some(icc) = decoder.icc_profile().unwrap_or_default()
    && let Ok(profile) = ColorProfile::new_from_slice(&icc)
    && icc_requires_transform(&profile)
  {
    let (width, height) = decoder.dimensions();
    let total_bytes = usize::try_from(decoder.total_bytes()).context("image is too large")?;
    let mut pixels = vec![0_u8; total_bytes];
    decoder.read_image(&mut pixels)?;

    let transform = profile
      .create_transform_8bit(
        layout,
        &ColorProfile::new_srgb(),
        layout,
        TransformOptions::default(),
      )
      .context("failed to create ICC transform")?;

    let mut converted = vec![0_u8; pixels.len()];
    transform
      .transform(&pixels, &mut converted)
      .context("failed to transform image ICC profile")?;

    let mut image = image_from_moxcms_pixels(layout, width, height, converted)?;
    image.set_rgb_primaries(Cicp::SRGB.primaries);
    image.set_transfer_function(Cicp::SRGB.transfer);
    Ok(image)
  } else {
    Ok(DynamicImage::from_decoder(decoder)?)
  }
}

fn color_type_to_moxcms_layout(color_type: ColorType) -> Option<Layout> {
  match color_type {
    ColorType::L8 => Some(Layout::Gray),
    ColorType::La8 => Some(Layout::GrayAlpha),
    ColorType::Rgb8 => Some(Layout::Rgb),
    ColorType::Rgba8 => Some(Layout::Rgba),
    _ => None,
  }
}

fn image_from_moxcms_pixels(
  layout: Layout,
  width: u32,
  height: u32,
  pixels: Vec<u8>,
) -> Result<DynamicImage> {
  match layout {
    Layout::Gray => GrayImage::from_raw(width, height, pixels)
      .map(DynamicImage::ImageLuma8)
      .context("converted grayscale buffer has an invalid size"),
    Layout::GrayAlpha => GrayAlphaImage::from_raw(width, height, pixels)
      .map(DynamicImage::ImageLumaA8)
      .context("converted grayscale-alpha buffer has an invalid size"),
    Layout::Rgb => RgbImage::from_raw(width, height, pixels)
      .map(DynamicImage::ImageRgb8)
      .context("converted RGB buffer has an invalid size"),
    Layout::Rgba => RgbaImage::from_raw(width, height, pixels)
      .map(DynamicImage::ImageRgba8)
      .context("converted RGBA buffer has an invalid size"),
    _ => bail!("unsupported ICC conversion pixel layout"),
  }
}

fn icc_requires_transform(profile: &ColorProfile) -> bool {
  if profile.color_space == DataColorSpace::Cmyk {
    return false;
  }

  profile.cicp.is_none_or(|cicp| {
    cicp.color_primaries != CicpColorPrimaries::Bt709
      || cicp.transfer_characteristics != TransferCharacteristics::Srgb
  })
}

fn flip_size(orientation: Orientation, size: (u32, u32)) -> (u32, u32) {
  use image::metadata::Orientation::{Rotate90, Rotate90FlipH, Rotate270, Rotate270FlipH};
  match orientation {
    Rotate90 | Rotate270 | Rotate90FlipH | Rotate270FlipH => (size.1, size.0),
    _ => size,
  }
}

fn resize_exact_fast(
  image: DynamicImage,
  target_width: u32,
  target_height: u32,
  filter: FilterType,
) -> DynamicImage {
  match try_resize_exact_fast(&image, target_width, target_height, filter) {
    Ok(resized) => resized,
    Err(_) => image.resize_exact(target_width, target_height, filter),
  }
}

fn try_resize_exact_fast(
  image: &DynamicImage,
  target_width: u32,
  target_height: u32,
  filter: FilterType,
) -> Result<DynamicImage> {
  match image {
    DynamicImage::ImageLuma8(image) => {
      let pixels = resize_u8_pixels(
        image.as_raw(),
        image.width(),
        image.height(),
        target_width,
        target_height,
        PixelType::U8,
        filter,
      )?;
      let image: GrayImage = ImageBuffer::from_raw(target_width, target_height, pixels)
        .context("resized luma buffer has an invalid size")?;
      Ok(DynamicImage::ImageLuma8(image))
    }
    DynamicImage::ImageLumaA8(image) => {
      let pixels = resize_u8_pixels(
        image.as_raw(),
        image.width(),
        image.height(),
        target_width,
        target_height,
        PixelType::U8x2,
        filter,
      )?;
      let image: GrayAlphaImage = ImageBuffer::from_raw(target_width, target_height, pixels)
        .context("resized luma-alpha buffer has an invalid size")?;
      Ok(DynamicImage::ImageLumaA8(image))
    }
    DynamicImage::ImageRgb8(image) => {
      let pixels = resize_u8_pixels(
        image.as_raw(),
        image.width(),
        image.height(),
        target_width,
        target_height,
        PixelType::U8x3,
        filter,
      )?;
      let image: RgbImage = ImageBuffer::from_raw(target_width, target_height, pixels)
        .context("resized rgb buffer has an invalid size")?;
      Ok(DynamicImage::ImageRgb8(image))
    }
    DynamicImage::ImageRgba8(image) => {
      let pixels = resize_u8_pixels(
        image.as_raw(),
        image.width(),
        image.height(),
        target_width,
        target_height,
        PixelType::U8x4,
        filter,
      )?;
      let image: RgbaImage = ImageBuffer::from_raw(target_width, target_height, pixels)
        .context("resized rgba buffer has an invalid size")?;
      Ok(DynamicImage::ImageRgba8(image))
    }
    _ => bail!("fast resize supports only 8-bit native image buffers"),
  }
}

fn resize_u8_pixels(
  pixels: &[u8],
  width: u32,
  height: u32,
  target_width: u32,
  target_height: u32,
  pixel_type: PixelType,
  filter: FilterType,
) -> Result<Vec<u8>> {
  let src = FirImageRef::new(width, height, pixels, pixel_type)?;
  let mut dst = FirImage::new(target_width, target_height, pixel_type);
  let options = ResizeOptions::new().resize_alg(resize_algorithm(filter));
  Resizer::new().resize(&src, &mut dst, Some(&options))?;
  Ok(dst.into_vec())
}

fn resize_algorithm(filter: FilterType) -> ResizeAlg {
  match filter {
    FilterType::Nearest => ResizeAlg::Nearest,
    FilterType::Triangle => ResizeAlg::Convolution(FirFilterType::Bilinear),
    FilterType::CatmullRom => ResizeAlg::Convolution(FirFilterType::CatmullRom),
    FilterType::Gaussian => ResizeAlg::Convolution(FirFilterType::Gaussian),
    FilterType::Lanczos3 => ResizeAlg::Convolution(FirFilterType::Lanczos3),
  }
}

#[derive(Debug, Clone, Copy)]
struct PixelViewport {
  x: u32,
  y: u32,
  width: u32,
  height: u32,
}

async fn crop_prepared(
  prepared: &PreparedNativeImage,
  viewport: NativeImageViewport,
  cell_pixels: Option<(u16, u16)>,
) -> Result<PreparedNativeImage> {
  let image = prepared.image.clone();
  tokio::task::spawn_blocking(move || {
    let pixels = pixel_viewport(image.as_ref(), viewport, cell_pixels)?;
    let cropped = image.crop_imm(pixels.x, pixels.y, pixels.width, pixels.height);
    Ok(PreparedNativeImage {
      image: Arc::new(cropped),
    })
  })
  .await?
}

pub async fn encode_prepared_viewport_png(
  prepared: &PreparedNativeImage,
  viewport: NativeImageViewport,
  cell_pixels: Option<(u16, u16)>,
) -> Result<Vec<u8>> {
  let image = prepared.image.clone();
  tokio::task::spawn_blocking(move || {
    let pixels = pixel_viewport(image.as_ref(), viewport, cell_pixels)?;
    let cropped = image.crop_imm(pixels.x, pixels.y, pixels.width, pixels.height);
    let rgba = cropped.to_rgba8();
    let mut out = Vec::new();
    PngEncoder::new(&mut out).write_image(
      rgba.as_raw(),
      rgba.width(),
      rgba.height(),
      ExtendedColorType::Rgba8,
    )?;
    Ok(out)
  })
  .await?
}

fn pixel_viewport(
  image: &DynamicImage,
  viewport: NativeImageViewport,
  cell_pixels: Option<(u16, u16)>,
) -> Result<PixelViewport> {
  pixel_viewport_for_size(image.width(), image.height(), viewport, cell_pixels)
}

fn pixel_viewport_for_size(
  pixel_width: u32,
  pixel_height: u32,
  viewport: NativeImageViewport,
  _cell_pixels: Option<(u16, u16)>,
) -> Result<PixelViewport> {
  if pixel_width == 0 || pixel_height == 0 {
    bail!("image is empty");
  }
  if viewport.visible_width_cells == 0 || viewport.visible_height_cells == 0 {
    bail!("viewport is empty");
  }

  let full_width_cells = u32::from(viewport.full_width_cells.max(1));
  let full_height_cells = u32::from(viewport.full_height_cells.max(1));
  let left_cells = viewport.left_cells.min(full_width_cells);
  let top_cells = viewport.top_cells.min(full_height_cells);
  let visible_width_cells = u32::from(viewport.visible_width_cells.max(1));
  let visible_height_cells = u32::from(viewport.visible_height_cells.max(1));
  let right_cells = left_cells
    .saturating_add(visible_width_cells)
    .min(full_width_cells);
  let bottom_cells = top_cells
    .saturating_add(visible_height_cells)
    .min(full_height_cells);

  let x = scale_cells_to_pixels_floor(left_cells, full_width_cells, pixel_width);
  let y = scale_cells_to_pixels_floor(top_cells, full_height_cells, pixel_height);
  if x >= pixel_width || y >= pixel_height {
    bail!("viewport starts outside image bounds");
  }

  let end_x = scale_cells_to_pixels_ceil(right_cells, full_width_cells, pixel_width);
  let end_y = scale_cells_to_pixels_ceil(bottom_cells, full_height_cells, pixel_height);
  let width = end_x
    .saturating_sub(x)
    .min(pixel_width.saturating_sub(x))
    .max(1);
  let height = end_y
    .saturating_sub(y)
    .min(pixel_height.saturating_sub(y))
    .max(1);
  Ok(PixelViewport {
    x,
    y,
    width,
    height,
  })
}

fn scale_cells_to_pixels_floor(cells: u32, full_cells: u32, pixels: u32) -> u32 {
  let full_cells = u64::from(full_cells.max(1));
  let scaled = u64::from(cells).saturating_mul(u64::from(pixels)) / full_cells;
  scaled.min(u64::from(pixels)).try_into().unwrap_or(pixels)
}

fn scale_cells_to_pixels_ceil(cells: u32, full_cells: u32, pixels: u32) -> u32 {
  let full_cells = u64::from(full_cells.max(1));
  let scaled = u64::from(cells)
    .saturating_mul(u64::from(pixels))
    .saturating_add(full_cells.saturating_sub(1))
    / full_cells;
  scaled.min(u64::from(pixels)).try_into().unwrap_or(pixels)
}

fn encode_kitty_placement(
  pixels: PixelViewport,
  viewport: NativeImageViewport,
  envelope: &ProtocolEnvelope,
  image_id: u32,
  placement_id: u32,
) -> Result<Vec<u8>> {
  let mut out = Vec::new();
  write!(
    out,
    "{}_Gq=2,a=p,C=1,z=-1,i={image_id},p={placement_id},x={},y={},w={},h={},c={},r={}{}\\{}",
    envelope.start,
    pixels.x,
    pixels.y,
    pixels.width,
    pixels.height,
    viewport.visible_width_cells,
    viewport.visible_height_cells,
    envelope.escape,
    envelope.close
  )?;
  Ok(out)
}

async fn encode_kitty(
  image: Arc<DynamicImage>,
  envelope: &ProtocolEnvelope,
  image_id: u32,
  unicode_placeholders: bool,
) -> Result<Vec<u8>> {
  let envelope = envelope.clone();
  tokio::task::spawn_blocking(move || {
    encode_kitty_image(
      image.as_ref(),
      image_id,
      KittyTransmit::Display {
        unicode_placeholders,
      },
      &envelope,
    )
  })
  .await?
}

#[derive(Debug, Clone, Copy)]
enum KittyTransmit {
  Display { unicode_placeholders: bool },
  UploadOnly,
}

fn encode_kitty_image(
  image: &DynamicImage,
  image_id: u32,
  transmit: KittyTransmit,
  envelope: &ProtocolEnvelope,
) -> Result<Vec<u8>> {
  let size = (image.width(), image.height());
  match image {
    DynamicImage::ImageRgb8(image) => {
      encode_kitty_raw(image.as_raw(), 24, size, image_id, transmit, envelope)
    }
    DynamicImage::ImageRgba8(image) => {
      encode_kitty_raw(image.as_raw(), 32, size, image_id, transmit, envelope)
    }
    image if image.color().has_alpha() => {
      let image = image.to_rgba8();
      encode_kitty_raw(image.as_raw(), 32, size, image_id, transmit, envelope)
    }
    image => {
      let image = image.to_rgb8();
      encode_kitty_raw(image.as_raw(), 24, size, image_id, transmit, envelope)
    }
  }
}

fn encode_kitty_raw(
  raw: &[u8],
  format: u8,
  size: (u32, u32),
  image_id: u32,
  transmit: KittyTransmit,
  envelope: &ProtocolEnvelope,
) -> Result<Vec<u8>> {
  const RAW_CHUNK_SIZE: usize = 3072;

  let encoded_len = raw.len().div_ceil(3) * 4;
  let chunk_count = raw.len().div_ceil(RAW_CHUNK_SIZE);
  let mut chunks = raw.chunks(RAW_CHUNK_SIZE).peekable();
  let mut encoded = String::with_capacity(4096);
  let mut out = Vec::with_capacity(encoded_len + chunk_count * 64 + 64);
  if let Some(first) = chunks.next() {
    STANDARD.encode_string(first, &mut encoded);
    let control = kitty_transmit_control(transmit);
    write!(
      out,
      "{}_Gq=2,{control},f={format},s={},v={},i={image_id},m={};{}{}\\{}",
      envelope.start,
      size.0,
      size.1,
      u8::from(chunks.peek().is_some()),
      encoded,
      envelope.escape,
      envelope.close
    )?;
  }

  while let Some(chunk) = chunks.next() {
    encoded.clear();
    STANDARD.encode_string(chunk, &mut encoded);
    write!(
      out,
      "{}_Gm={};{}{}\\{}",
      envelope.start,
      u8::from(chunks.peek().is_some()),
      encoded,
      envelope.escape,
      envelope.close
    )?;
  }

  Ok(out)
}

fn kitty_transmit_control(transmit: KittyTransmit) -> &'static str {
  match transmit {
    KittyTransmit::Display {
      unicode_placeholders: true,
    } => "a=T,C=1,U=1",
    KittyTransmit::Display {
      unicode_placeholders: false,
    } => "a=T,z=-1,C=1",
    KittyTransmit::UploadOnly => "a=t",
  }
}

async fn encode_iterm(image: Arc<DynamicImage>, envelope: &ProtocolEnvelope) -> Result<Vec<u8>> {
  let envelope = envelope.clone();
  tokio::task::spawn_blocking(move || {
    let (width, height) = (image.width(), image.height());
    let mut image_bytes = Vec::new();
    if image.color().has_alpha() {
      match image.as_ref() {
        DynamicImage::ImageRgba8(rgba) => {
          PngEncoder::new(&mut image_bytes).write_image(
            rgba.as_raw(),
            width,
            height,
            ExtendedColorType::Rgba8,
          )?;
        }
        image => {
          let rgba = image.to_rgba8();
          PngEncoder::new(&mut image_bytes).write_image(
            rgba.as_raw(),
            width,
            height,
            ExtendedColorType::Rgba8,
          )?;
        }
      }
    } else {
      JpegEncoder::new_with_quality(&mut image_bytes, 85).encode_image(image.as_ref())?;
    }

    let mut out = String::with_capacity(256 + image_bytes.len() * 4 / 3);
    write!(
      out,
      "{}]1337;File=inline=1;size={};width={width}px;height={height}px;doNotMoveCursor=1:",
      envelope.start,
      image_bytes.len()
    )?;
    STANDARD.encode_string(image_bytes, &mut out);
    write!(out, "\x07{}", envelope.close)?;
    Ok(out.into_bytes())
  })
  .await?
}

struct QuantizeOutput<T> {
  indices: Vec<u8>,
  palette: Vec<T>,
}

struct SixelPixels {
  rgb: Vec<u8>,
  alpha: Option<Vec<u8>>,
  width: u32,
  height: u32,
}

async fn encode_sixel(image: Arc<DynamicImage>, envelope: &ProtocolEnvelope) -> Result<Vec<u8>> {
  let envelope = envelope.clone();
  tokio::task::spawn_blocking(move || {
    if image.width() == 0 || image.height() == 0 {
      bail!("image is empty");
    }
    let pixels = prepare_sixel_pixels(image.as_ref());
    let has_alpha = pixels.alpha.is_some();
    let quantized = quantify(&pixels.rgb, has_alpha, pixels.width, pixels.height)?;
    let indexed = build_sixel_indices(quantized.indices, pixels.alpha.as_deref());

    let mut out = Vec::new();
    write!(
      out,
      "{}P9;1q\"1;1;{};{}",
      envelope.start, pixels.width, pixels.height
    )?;

    for (index, color) in quantized.palette.iter().enumerate() {
      write!(
        out,
        "#{};2;{};{};{}",
        index + usize::from(has_alpha),
        u16::from(color.red) * 100 / 255,
        u16::from(color.green) * 100 / 255,
        u16::from(color.blue) * 100 / 255
      )?;
    }

    for row in encode_sixel_rows(&indexed, pixels.width, pixels.height)? {
      out.extend(row);
    }

    write!(out, "{}\\{}", envelope.escape, envelope.close)?;
    Ok(out)
  })
  .await?
}

fn prepare_sixel_pixels(image: &DynamicImage) -> SixelPixels {
  let (width, height) = (image.width(), image.height());
  if image.color().has_alpha() {
    match image {
      DynamicImage::ImageRgba8(rgba) => {
        let raw = rgba.as_raw();
        let rgb = raw
          .par_chunks_exact(4)
          .flat_map_iter(|pixel| [pixel[0], pixel[1], pixel[2]])
          .collect();
        let alpha = raw.par_chunks_exact(4).map(|pixel| pixel[3]).collect();
        return SixelPixels {
          rgb,
          alpha: Some(alpha),
          width,
          height,
        };
      }
      image => {
        let rgba = image.to_rgba8();
        let raw = rgba.as_raw();
        let rgb = raw
          .par_chunks_exact(4)
          .flat_map_iter(|pixel| [pixel[0], pixel[1], pixel[2]])
          .collect();
        let alpha = raw.par_chunks_exact(4).map(|pixel| pixel[3]).collect();
        return SixelPixels {
          rgb,
          alpha: Some(alpha),
          width,
          height,
        };
      }
    }
  }

  let rgb = match image {
    DynamicImage::ImageRgb8(rgb) => rgb.as_raw().clone(),
    image => image.to_rgb8().into_raw(),
  };
  SixelPixels {
    rgb,
    alpha: None,
    width,
    height,
  }
}

fn build_sixel_indices(indices: Vec<u8>, alpha: Option<&[u8]>) -> Vec<u8> {
  match alpha {
    Some(alpha) => indices
      .par_iter()
      .zip(alpha.par_iter())
      .map(|(&index, &alpha)| {
        if alpha == 0 {
          0
        } else {
          index.saturating_add(1)
        }
      })
      .collect(),
    None => indices,
  }
}

fn encode_sixel_rows(indexed: &[u8], width: u32, height: u32) -> Result<Vec<Vec<u8>>> {
  let width = width as usize;
  let height = height as usize;
  let rows: Vec<Result<Vec<u8>>> = (0..height)
    .into_par_iter()
    .map(|y| {
      let sixel_char = (b'?' + (1_u8 << (y % 6))) as char;
      let mut out = Vec::new();
      let mut last = 0_u8;
      let mut repeat = 0_usize;

      for &index in &indexed[y * width..(y + 1) * width] {
        if index == last || repeat == 0 {
          last = index;
          repeat += 1;
          continue;
        }
        write_sixel_run(&mut out, last, repeat, sixel_char)?;
        last = index;
        repeat = 1;
      }

      write_sixel_run(&mut out, last, repeat, sixel_char)?;
      write!(out, "$")?;
      if y % 6 == 5 {
        write!(out, "-")?;
      }
      Ok(out)
    })
    .collect();

  rows.into_iter().collect()
}

fn quantify(
  rgb: &[u8],
  has_alpha: bool,
  width: u32,
  height: u32,
) -> Result<QuantizeOutput<Srgb<u8>>> {
  let color_count = width as usize * height as usize;
  let colors: &[Srgb<u8>] = rgb[..color_count * 3].components_as();
  let palette_size = PaletteSize::try_from(256_u16 - u16::from(has_alpha))?;
  let color_map = WuU8x3::run_slice(colors, BinnerU8x3::rgb())?.color_map(palette_size);
  Ok(QuantizeOutput {
    indices: color_map.map_to_indices(colors),
    palette: color_map.into_palette().into_vec(),
  })
}

fn write_sixel_run(out: &mut Vec<u8>, index: u8, repeat: usize, sixel_char: char) -> Result<()> {
  if repeat > 1 {
    write!(out, "#{index}!{repeat}{sixel_char}")?;
  } else {
    write!(out, "#{index}{sixel_char}")?;
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
  };

  use image::{Rgba, RgbaImage};

  use super::*;

  #[test]
  fn kitty_image_ids_are_stable_and_unique_within_process() {
    let first = kitty_image_id(b"img-tui-test-resource-a");
    let first_again = kitty_image_id(b"img-tui-test-resource-a");
    let second = kitty_image_id(b"img-tui-test-resource-b");

    assert_ne!(first, 0);
    assert_eq!(first, first_again);
    assert_ne!(first, second);
  }

  #[test]
  fn render_viewport_supports_protocol_modes() {
    let runtime = tokio::runtime::Builder::new_current_thread()
      .build()
      .expect("runtime");
    runtime.block_on(async {
      let path = test_image_path();
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
    let runtime = tokio::runtime::Builder::new_current_thread()
      .build()
      .expect("runtime");
    runtime.block_on(async {
      let path = test_image_path();
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

  fn test_image_path() -> PathBuf {
    let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .expect("clock")
      .as_nanos();
    std::env::temp_dir().join(format!("img-tui-viewport-test-{nanos}.png"))
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
