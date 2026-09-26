//! Sixel encoding: palette quantization and band-wise run-length output.

use std::{borrow::Cow, io::Write as _};

use anyhow::{Result, bail};
use image::DynamicImage;
use palette::{Srgb, cast::ComponentsAs};
use quantette::{
  PaletteSize,
  color_map::IndexedColorMap,
  wu::{BinnerU8x3, WuU8x3},
};
use rayon::prelude::*;

use super::envelope::ProtocolEnvelope;

/// Pixel rows per sixel band.
const BAND_HEIGHT: usize = 6;
/// Sixel data character with no pixels set; used to skip pixels.
const EMPTY_SIXEL: u8 = b'?';

/// Encode `image` as a sixel image with up to 256 colors.
///
/// Fully transparent pixels are not painted at all, so the cells beneath the
/// image stay visible there (the sequence sets the transparent-background
/// parameter).
pub(super) fn encode(image: &DynamicImage, envelope: ProtocolEnvelope) -> Result<Vec<u8>> {
  if image.width() == 0 || image.height() == 0 {
    bail!("image is empty");
  }
  let (width, height) = (image.width(), image.height());
  let pixels = SixelPixels::new(image);
  let has_alpha = pixels.alpha.is_some();
  let quantized = quantize(&pixels.rgb, has_alpha)?;
  let indexed = match &pixels.alpha {
    // Register 0 stands for "transparent"; the palette starts at 1.
    Some(alpha) => quantized
      .indices
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
    None => quantized.indices,
  };

  let mut out = Vec::with_capacity(indexed.len() / 2 + 4096);
  write!(out, "{}P9;1q\"1;1;{width};{height}", envelope.start)?;
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
  encode_bands(&indexed, width as usize, has_alpha, &mut out);
  write!(out, "{}\\{}", envelope.escape, envelope.close)?;
  Ok(out)
}

struct SixelPixels<'a> {
  rgb: Cow<'a, [u8]>,
  alpha: Option<Vec<u8>>,
}

impl<'a> SixelPixels<'a> {
  fn new(image: &'a DynamicImage) -> Self {
    if image.color().has_alpha() {
      let rgba = match image {
        DynamicImage::ImageRgba8(rgba) => Cow::Borrowed(rgba),
        image => Cow::Owned(image.to_rgba8()),
      };
      let raw = rgba.as_raw();
      return Self {
        rgb: Cow::Owned(
          raw
            .par_chunks_exact(4)
            .flat_map_iter(|pixel| [pixel[0], pixel[1], pixel[2]])
            .collect(),
        ),
        alpha: Some(raw.par_chunks_exact(4).map(|pixel| pixel[3]).collect()),
      };
    }

    let rgb = match image {
      DynamicImage::ImageRgb8(rgb) => Cow::Borrowed(rgb.as_raw().as_slice()),
      image => Cow::Owned(image.to_rgb8().into_raw()),
    };
    Self { rgb, alpha: None }
  }
}

struct Quantized {
  indices: Vec<u8>,
  palette: Vec<Srgb<u8>>,
}

fn quantize(rgb: &[u8], has_alpha: bool) -> Result<Quantized> {
  let colors: &[Srgb<u8>] = rgb.components_as();
  let palette_size = PaletteSize::try_from(256_u16 - u16::from(has_alpha))?;
  let color_map = WuU8x3::run_slice(colors, BinnerU8x3::rgb())?.color_map(palette_size);
  Ok(Quantized {
    indices: color_map.map_to_indices(colors),
    palette: color_map.into_palette().into_vec(),
  })
}

/// Append the sixel data for `indexed` (one palette register per pixel),
/// band by band: for each color present in a band, one line of sixels
/// covering the columns that use it. Register 0 is left unpainted when
/// `transparent_zero` is set.
fn encode_bands(indexed: &[u8], width: usize, transparent_zero: bool, out: &mut Vec<u8>) {
  let bands = indexed
    .par_chunks(width * BAND_HEIGHT)
    .map_init(
      || BandScratch::new(width),
      |scratch, band| {
        let mut out = Vec::new();
        scratch.encode(band, transparent_zero, &mut out);
        out
      },
    )
    .collect::<Vec<_>>();
  for band in bands {
    out.extend_from_slice(&band);
  }
}

/// Per-band working memory, reused across the bands one worker encodes.
struct BandScratch {
  width: usize,
  /// Sixel bits for every (color, column) pair of the band.
  bits: Vec<u8>,
  /// First and last column that uses each color; `first` is `usize::MAX`
  /// for colors absent from the band.
  first: [usize; 256],
  last: [usize; 256],
}

impl BandScratch {
  fn new(width: usize) -> Self {
    Self {
      width,
      bits: vec![0; 256 * width],
      first: [usize::MAX; 256],
      last: [0; 256],
    }
  }

  fn encode(&mut self, band: &[u8], transparent_zero: bool, out: &mut Vec<u8>) {
    let width = self.width;
    let rows = band.len() / width;
    for (row, pixels) in band.chunks_exact(width).enumerate() {
      let bit = 1_u8 << row;
      for (x, &color) in pixels.iter().enumerate() {
        if transparent_zero && color == 0 {
          continue;
        }
        let color = usize::from(color);
        self.bits[color * width + x] |= bit;
        if self.first[color] == usize::MAX {
          self.first[color] = x;
          self.last[color] = x;
        } else {
          self.first[color] = self.first[color].min(x);
          self.last[color] = self.last[color].max(x);
        }
      }
    }

    for color in 0..256 {
      let first = std::mem::replace(&mut self.first[color], usize::MAX);
      if first == usize::MAX {
        continue;
      }
      let line = &mut self.bits[color * width + first..=color * width + self.last[color]];
      out.push(b'#');
      push_decimal(out, color);
      push_run(out, 0, first);
      let mut run_bits = line[0];
      let mut run_len = 0;
      for &bits in line.iter() {
        if bits != run_bits {
          push_run(out, run_bits, run_len);
          run_bits = bits;
          run_len = 0;
        }
        run_len += 1;
      }
      push_run(out, run_bits, run_len);
      out.push(b'$');
      line.fill(0);
    }
    if rows == BAND_HEIGHT {
      out.push(b'-');
    }
  }
}

fn push_run(out: &mut Vec<u8>, bits: u8, len: usize) {
  let sixel = EMPTY_SIXEL + bits;
  if len > 3 {
    out.push(b'!');
    push_decimal(out, len);
    out.push(sixel);
  } else {
    out.extend(std::iter::repeat_n(sixel, len));
  }
}

fn push_decimal(out: &mut Vec<u8>, mut value: usize) {
  let mut digits = [0_u8; 20];
  let mut start = digits.len();
  loop {
    start -= 1;
    digits[start] = b'0' + (value % 10) as u8;
    value /= 10;
    if value == 0 {
      break;
    }
  }
  out.extend_from_slice(&digits[start..]);
}

#[cfg(test)]
mod tests {
  use std::collections::HashMap;

  use image::{DynamicImage, Rgba, RgbaImage};

  use super::{ProtocolEnvelope, encode, encode_bands};

  /// Minimal sixel data decoder: register per painted pixel, `None` where no
  /// sixel bit was set.
  fn decode_bands(data: &[u8], width: usize, height: usize) -> Vec<Option<u8>> {
    let mut pixels = vec![None; width * height];
    let (mut x, mut band, mut color) = (0_usize, 0_usize, 0_u8);
    let mut index = 0;
    let number = |index: &mut usize| {
      let start = *index;
      while data[*index].is_ascii_digit() {
        *index += 1;
      }
      std::str::from_utf8(&data[start..*index])
        .unwrap()
        .parse::<usize>()
        .unwrap()
    };
    while index < data.len() {
      let byte = data[index];
      index += 1;
      let (sixel, repeat) = match byte {
        b'#' => {
          color = number(&mut index) as u8;
          continue;
        }
        b'$' => {
          x = 0;
          continue;
        }
        b'-' => {
          x = 0;
          band += 1;
          continue;
        }
        b'!' => {
          let repeat = number(&mut index);
          index += 1;
          (data[index - 1], repeat)
        }
        _ => (byte, 1),
      };
      let bits = sixel - b'?';
      for _ in 0..repeat {
        for row in 0..6 {
          let y = band * 6 + row;
          if bits & (1 << row) != 0 && y < height && x < width {
            assert!(pixels[y * width + x].is_none(), "pixel painted twice");
            pixels[y * width + x] = Some(color);
          }
        }
        x += 1;
      }
    }
    pixels
  }

  #[test]
  fn bands_round_trip_every_pixel() {
    let (width, height) = (23, 14);
    let indexed = (0..width * height)
      .map(|i| ((i * 7 + i / width * 3) % 5) as u8)
      .collect::<Vec<_>>();

    let mut out = Vec::new();
    encode_bands(&indexed, width, false, &mut out);

    let decoded = decode_bands(&out, width, height);
    let expected = indexed.iter().copied().map(Some).collect::<Vec<_>>();
    assert_eq!(decoded, expected);
    // Two complete bands end with a graphics new line; the partial third
    // band does not.
    assert_eq!(out.iter().filter(|byte| **byte == b'-').count(), 2);
  }

  #[test]
  fn transparent_pixels_are_not_painted() {
    let (width, height) = (8, 7);
    let indexed = (0..width * height)
      .map(|i| if i % 3 == 0 { 0 } else { (i % 4 + 1) as u8 })
      .collect::<Vec<_>>();

    let mut out = Vec::new();
    encode_bands(&indexed, width, true, &mut out);

    let decoded = decode_bands(&out, width, height);
    for (pixel, index) in decoded.iter().zip(&indexed) {
      assert_eq!(*pixel, (*index != 0).then_some(*index));
    }
    assert!(!out.windows(2).any(|pair| pair == b"#0"));
  }

  #[test]
  fn long_runs_use_repeat_introducer() {
    let indexed = vec![3_u8; 40 * 6];
    let mut out = Vec::new();
    encode_bands(&indexed, 40, false, &mut out);
    assert_eq!(out, b"#3!40~$-");
  }

  /// Palette definitions (`#register;2;r;g;b`) and the sixel data after them.
  fn split_palette(body: &str) -> (HashMap<u8, [u16; 3]>, &str) {
    let mut palette = HashMap::new();
    let mut rest = body;
    while let Some(definition) = rest.strip_prefix('#') {
      let digits = definition.chars().take_while(char::is_ascii_digit).count();
      let Some(params) = definition[digits..].strip_prefix(";2;") else {
        break;
      };
      let end = params
        .find(|ch: char| !(ch.is_ascii_digit() || ch == ';'))
        .unwrap_or(params.len());
      let rgb = params[..end]
        .split(';')
        .map(|value| value.parse().unwrap())
        .collect::<Vec<u16>>();
      palette.insert(
        definition[..digits].parse().unwrap(),
        [rgb[0], rgb[1], rgb[2]],
      );
      rest = &params[end..];
    }
    (palette, rest)
  }

  #[test]
  fn encoded_image_leaves_transparent_pixels_unpainted() {
    let mut image = RgbaImage::from_pixel(5, 3, Rgba([255, 0, 0, 255]));
    image.put_pixel(0, 0, Rgba([0, 0, 0, 0]));
    let out = encode(
      &DynamicImage::ImageRgba8(image),
      ProtocolEnvelope::new(None),
    )
    .unwrap();
    let out = String::from_utf8(out).unwrap();
    let body = out
      .strip_prefix("\x1bP9;1q\"1;1;5;3")
      .and_then(|body| body.strip_suffix("\x1b\\"))
      .expect("sixel header and terminator");

    let (palette, data) = split_palette(body);
    assert!(!palette.contains_key(&0), "register 0 is reserved");
    let decoded = decode_bands(data.as_bytes(), 5, 3);
    assert_eq!(decoded[0], None);
    for pixel in &decoded[1..] {
      let register = pixel.expect("opaque pixel is painted");
      assert_eq!(palette[&register], [100, 0, 0]);
    }
  }
}
