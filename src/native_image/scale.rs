//! Image decoding, color management and scaling to a cell grid.

use std::path::Path;

use anyhow::{Context, Result, bail};
use fast_image_resize::{
  FilterType as FirFilterType, PixelType, ResizeAlg, ResizeOptions, Resizer,
  images::{Image as FirImage, ImageRef as FirImageRef},
};
use image::{
  ColorType, DynamicImage, GrayAlphaImage, GrayImage, ImageBuffer, ImageDecoder, ImageReader,
  RgbImage, RgbaImage,
  imageops::FilterType,
  metadata::{Cicp, Orientation},
};
use moxcms::{
  CicpColorPrimaries, ColorProfile, DataColorSpace, Layout, TransferCharacteristics,
  TransformOptions,
};
use tracing::warn;

/// Cell size assumed when the terminal did not report one.
const DEFAULT_CELL_PIXELS: (u16, u16) = (8, 16);

/// Decode `path` as 8-bit sRGB and scale it down to
/// fit `width_cells` x `height_cells` (never up), honoring EXIF orientation.
pub(super) fn load_scaled(
  path: &Path,
  width_cells: u16,
  height_cells: u16,
  cell_pixels: Option<(u16, u16)>,
) -> Result<DynamicImage> {
  let decoded = decode_image(path)?;
  let (cell_width, cell_height) = cell_pixels.unwrap_or(DEFAULT_CELL_PIXELS);
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

/// Decode pixels as 8-bit sRGB: deeper samples are reduced to 8 bits and an
/// embedded ICC profile is applied unless it already describes sRGB. A
/// profile that cannot be applied is ignored rather than failing the image.
fn decode_image_pixels(mut decoder: impl ImageDecoder) -> Result<DynamicImage> {
  let profile = decoder
    .icc_profile()
    .unwrap_or_default()
    .and_then(|icc| ColorProfile::new_from_slice(&icc).ok())
    .filter(icc_requires_transform);
  let Some(profile) = profile else {
    return Ok(into_8bit(DynamicImage::from_decoder(decoder)?));
  };

  let (width, height) = decoder.dimensions();
  let (layout, pixels) = match color_type_to_moxcms_layout(decoder.color_type()) {
    Some(layout) => {
      let total_bytes = usize::try_from(decoder.total_bytes()).context("image is too large")?;
      let mut pixels = vec![0_u8; total_bytes];
      decoder.read_image(&mut pixels)?;
      (layout, pixels)
    }
    None => {
      let image = into_8bit(DynamicImage::from_decoder(decoder)?);
      let Some(layout) = color_type_to_moxcms_layout(image.color()) else {
        return Ok(image);
      };
      (layout, image.into_bytes())
    }
  };

  match apply_icc_profile(&profile, layout, &pixels) {
    Ok(converted) => {
      let mut image = image_from_moxcms_pixels(layout, width, height, converted)?;
      image.set_rgb_primaries(Cicp::SRGB.primaries);
      image.set_transfer_function(Cicp::SRGB.transfer);
      Ok(image)
    }
    Err(error) => {
      warn!(%error, "ignoring embedded ICC profile");
      image_from_moxcms_pixels(layout, width, height, pixels)
    }
  }
}

fn apply_icc_profile(profile: &ColorProfile, layout: Layout, pixels: &[u8]) -> Result<Vec<u8>> {
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
    .transform(pixels, &mut converted)
    .context("failed to transform image ICC profile")?;
  Ok(converted)
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

/// Every terminal protocol carries 8-bit channels. Converting 16-bit and
/// float images up front keeps them on the SIMD resize path and gives the
/// encoders (JPEG in particular, which rejects deeper samples) the buffer
/// layouts they support.
fn into_8bit(image: DynamicImage) -> DynamicImage {
  match image {
    DynamicImage::ImageLuma8(_)
    | DynamicImage::ImageLumaA8(_)
    | DynamicImage::ImageRgb8(_)
    | DynamicImage::ImageRgba8(_) => image,
    DynamicImage::ImageLuma16(_) => DynamicImage::ImageLuma8(image.to_luma8()),
    DynamicImage::ImageLumaA16(_) => DynamicImage::ImageLumaA8(image.to_luma_alpha8()),
    image if image.color().has_alpha() => DynamicImage::ImageRgba8(image.to_rgba8()),
    image => DynamicImage::ImageRgb8(image.to_rgb8()),
  }
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
  let resize = |raw: &[u8], pixel_type| {
    resize_u8_pixels(
      raw,
      (image.width(), image.height()),
      (target_width, target_height),
      pixel_type,
      filter,
    )
  };
  let resized = match image {
    DynamicImage::ImageLuma8(image) => ImageBuffer::from_raw(
      target_width,
      target_height,
      resize(image.as_raw(), PixelType::U8)?,
    )
    .map(DynamicImage::ImageLuma8),
    DynamicImage::ImageLumaA8(image) => ImageBuffer::from_raw(
      target_width,
      target_height,
      resize(image.as_raw(), PixelType::U8x2)?,
    )
    .map(DynamicImage::ImageLumaA8),
    DynamicImage::ImageRgb8(image) => ImageBuffer::from_raw(
      target_width,
      target_height,
      resize(image.as_raw(), PixelType::U8x3)?,
    )
    .map(DynamicImage::ImageRgb8),
    DynamicImage::ImageRgba8(image) => ImageBuffer::from_raw(
      target_width,
      target_height,
      resize(image.as_raw(), PixelType::U8x4)?,
    )
    .map(DynamicImage::ImageRgba8),
    _ => bail!("fast resize supports only 8-bit native image buffers"),
  };
  resized.context("resized buffer has an invalid size")
}

fn resize_u8_pixels(
  pixels: &[u8],
  (width, height): (u32, u32),
  (target_width, target_height): (u32, u32),
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

#[cfg(test)]
mod tests {
  use std::io::Cursor;

  use image::{
    DynamicImage, ExtendedColorType, ImageBuffer, ImageEncoder, Rgb, codecs::png::PngDecoder,
    codecs::png::PngEncoder,
  };
  use moxcms::ColorProfile;

  use super::{decode_image_pixels, fit_pixel_size, into_8bit};

  /// PNG of `pixels` (big-endian samples for 16-bit) tagged with `icc`.
  fn png_with_profile(
    pixels: &[u8],
    size: (u32, u32),
    color: ExtendedColorType,
    icc: &[u8],
  ) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = PngEncoder::new(&mut out);
    encoder.set_icc_profile(icc.to_vec()).unwrap();
    encoder.write_image(pixels, size.0, size.1, color).unwrap();
    out
  }

  fn decode_png(png: &[u8]) -> DynamicImage {
    decode_image_pixels(PngDecoder::new(Cursor::new(png)).unwrap()).unwrap()
  }

  #[test]
  fn icc_profiles_apply_to_deep_color_images() {
    let adobe_rgb = ColorProfile::new_adobe_rgb().encode().unwrap();
    let color8 = [200_u8, 80, 40];
    let color16 = color8
      .iter()
      .flat_map(|sample| (u16::from(*sample) * 257).to_be_bytes())
      .collect::<Vec<_>>();

    let from8 = decode_png(&png_with_profile(
      &color8,
      (1, 1),
      ExtendedColorType::Rgb8,
      &adobe_rgb,
    ));
    let from16 = decode_png(&png_with_profile(
      &color16,
      (1, 1),
      ExtendedColorType::Rgb16,
      &adobe_rgb,
    ));

    let converted = from8.to_rgb8().get_pixel(0, 0).0;
    assert_ne!(converted, color8, "Adobe RGB differs from sRGB");
    assert!(matches!(from16, DynamicImage::ImageRgb8(_)));
    assert_eq!(from16.to_rgb8().get_pixel(0, 0).0, converted);
  }

  #[test]
  fn unusable_icc_profile_keeps_the_image() {
    let rgb_profile = ColorProfile::new_display_p3().encode().unwrap();
    let gray = decode_png(&png_with_profile(
      &[10, 20, 30, 40],
      (2, 2),
      ExtendedColorType::L8,
      &rgb_profile,
    ));
    assert_eq!(gray.as_bytes().len(), 4);
  }

  #[test]
  fn fit_pixel_size_never_upscales() {
    assert_eq!(fit_pixel_size((100, 50), (400, 400)), (100, 50));
    assert_eq!(fit_pixel_size((400, 200), (100, 100)), (100, 50));
    assert_eq!(fit_pixel_size((0, 0), (0, 0)), (1, 1));
  }

  #[test]
  fn deep_color_images_become_8bit() {
    let deep = DynamicImage::ImageRgb16(ImageBuffer::from_pixel(2, 2, Rgb([65535, 0, 32896])));
    let DynamicImage::ImageRgb8(image) = into_8bit(deep) else {
      panic!("expected an 8-bit RGB image");
    };
    assert_eq!(image.get_pixel(0, 0).0, [255, 0, 128]);
  }
}
