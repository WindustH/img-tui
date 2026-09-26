//! iTerm2 inline image protocol (OSC 1337) encoding.

use std::{borrow::Cow, fmt::Write as _};

use anyhow::Result;
use base64::{Engine, engine::general_purpose::STANDARD};
use image::{
  DynamicImage, ExtendedColorType, ImageEncoder,
  codecs::{jpeg::JpegEncoder, png::PngEncoder},
};

use super::envelope::ProtocolEnvelope;

/// Encode `image` as PNG (when it has alpha) or JPEG and wrap it in an
/// `OSC 1337 ; File=` sequence sized to the image's pixels.
pub(super) fn encode(image: &DynamicImage, envelope: ProtocolEnvelope) -> Result<Vec<u8>> {
  let (width, height) = (image.width(), image.height());
  let mut image_bytes = Vec::new();
  if image.color().has_alpha() {
    let rgba = match image {
      DynamicImage::ImageRgba8(rgba) => Cow::Borrowed(rgba),
      image => Cow::Owned(image.to_rgba8()),
    };
    PngEncoder::new(&mut image_bytes).write_image(
      rgba.as_raw(),
      width,
      height,
      ExtendedColorType::Rgba8,
    )?;
  } else {
    // Hand the encoder a typed buffer: through `DynamicImage` it converts
    // every pixel to RGBA individually.
    let mut encoder = JpegEncoder::new_with_quality(&mut image_bytes, 85);
    match image {
      DynamicImage::ImageRgb8(rgb) => encoder.encode_image(rgb)?,
      DynamicImage::ImageLuma8(gray) => encoder.encode_image(gray)?,
      image => encoder.encode_image(&image.to_rgb8())?,
    }
  }

  let mut out = String::with_capacity(256 + image_bytes.len().div_ceil(3) * 4);
  write!(
    out,
    "{}]1337;File=inline=1;size={};width={width}px;height={height}px;doNotMoveCursor=1:",
    envelope.start,
    image_bytes.len()
  )?;
  STANDARD.encode_string(image_bytes, &mut out);
  write!(out, "\x07{}", envelope.close)?;
  Ok(out.into_bytes())
}

#[cfg(test)]
mod tests {
  use image::{DynamicImage, ImageBuffer, Rgb};

  use super::{ProtocolEnvelope, encode};

  #[test]
  fn opaque_non_rgb8_image_encodes_as_jpeg() {
    let image = DynamicImage::ImageRgb16(ImageBuffer::from_pixel(4, 2, Rgb([1000, 2000, 3000])));
    let out = encode(&image, ProtocolEnvelope::new(None)).expect("16-bit iTerm2 image");
    let out = String::from_utf8(out).unwrap();
    assert!(out.starts_with("\x1b]1337;File=inline=1;"));
    assert!(out.contains("width=4px;height=2px"));
    assert!(out.ends_with('\x07'));
  }
}
