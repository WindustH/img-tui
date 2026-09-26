//! Kitty graphics protocol encoding: uploads, placements and deletes.

use std::{
  collections::HashMap,
  fmt::Write as _,
  hash::{DefaultHasher, Hash, Hasher},
  sync::{Mutex, OnceLock},
  time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use image::DynamicImage;

use super::{NativeImageViewport, envelope::ProtocolEnvelope, viewport::PixelViewport};
use crate::RenderMode;

/// An image transmitted to the terminal without being displayed (`a=t`), plus
/// the geometry needed to place it later.
#[derive(Debug, Clone)]
pub struct KittyImageUpload {
  /// Escape sequences that upload the image.
  pub data: Vec<u8>,
  pub image_id: u32,
  pub pixel_width: u32,
  pub pixel_height: u32,
  pub cell_pixels: Option<(u16, u16)>,
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

#[derive(Debug, Clone, Copy)]
pub(super) enum KittyTransmit {
  /// Transmit and display at the cursor (`a=T`).
  Display { unicode_placeholders: bool },
  /// Transmit only (`a=t`); place later with [`placement`].
  UploadOnly,
}

impl KittyTransmit {
  fn control(self) -> &'static str {
    match self {
      Self::Display {
        unicode_placeholders: true,
      } => "a=T,C=1,U=1",
      Self::Display {
        unicode_placeholders: false,
      } => "a=T,z=-1,C=1",
      Self::UploadOnly => "a=t",
    }
  }
}

/// Transmit `image` as raw RGB/RGBA pixels in 4096-byte base64 chunks.
pub(super) fn encode_image(
  image: &DynamicImage,
  image_id: u32,
  transmit: KittyTransmit,
  envelope: ProtocolEnvelope,
) -> Vec<u8> {
  let size = (image.width(), image.height());
  match image {
    DynamicImage::ImageRgb8(image) => {
      encode_raw(image.as_raw(), 24, size, image_id, transmit, envelope)
    }
    DynamicImage::ImageRgba8(image) => {
      encode_raw(image.as_raw(), 32, size, image_id, transmit, envelope)
    }
    image if image.color().has_alpha() => encode_raw(
      image.to_rgba8().as_raw(),
      32,
      size,
      image_id,
      transmit,
      envelope,
    ),
    image => encode_raw(
      image.to_rgb8().as_raw(),
      24,
      size,
      image_id,
      transmit,
      envelope,
    ),
  }
}

fn encode_raw(
  raw: &[u8],
  format: u8,
  (width, height): (u32, u32),
  image_id: u32,
  transmit: KittyTransmit,
  envelope: ProtocolEnvelope,
) -> Vec<u8> {
  // 3072 raw bytes encode to 4096 base64 bytes, kitty's chunk size limit.
  const RAW_CHUNK_SIZE: usize = 3072;

  let chunk_count = raw.len().div_ceil(RAW_CHUNK_SIZE);
  let mut out = String::with_capacity(raw.len().div_ceil(3) * 4 + chunk_count * 64 + 64);
  for (index, chunk) in raw.chunks(RAW_CHUNK_SIZE).enumerate() {
    let more = u8::from(index + 1 < chunk_count);
    out.push_str(envelope.start);
    if index == 0 {
      let control = transmit.control();
      let _ = write!(
        out,
        "_Gq=2,{control},f={format},s={width},v={height},i={image_id},m={more};"
      );
    } else {
      let _ = write!(out, "_Gm={more};");
    }
    STANDARD.encode_string(chunk, &mut out);
    out.push_str(envelope.escape);
    out.push('\\');
    out.push_str(envelope.close);
  }
  out.into_bytes()
}

/// Place a previously uploaded image over `viewport`'s visible cells, showing
/// the `pixels` part of it, below text (`z=-1`) without moving the cursor.
pub(super) fn placement(
  pixels: PixelViewport,
  viewport: NativeImageViewport,
  envelope: ProtocolEnvelope,
  image_id: u32,
  placement_id: u32,
) -> Vec<u8> {
  envelope
    .kitty_command(format_args!(
      "q=2,a=p,C=1,z=-1,i={image_id},p={placement_id},x={},y={},w={},h={},c={},r={}",
      pixels.x,
      pixels.y,
      pixels.width,
      pixels.height,
      viewport.visible_width_cells,
      viewport.visible_height_cells,
    ))
    .into_bytes()
}

/// Virtual placement (`U=1`) scaling the image to `cols` x `rows` cells.
pub(super) fn virtual_placement(
  envelope: ProtocolEnvelope,
  image_id: u32,
  cols: u16,
  rows: u16,
) -> Vec<u8> {
  envelope
    .kitty_command(format_args!(
      "q=2,a=p,U=1,i={},c={},r={}",
      image_id.max(1),
      cols.max(1),
      rows.max(1)
    ))
    .into_bytes()
}

/// Sequence that deletes a kitty image's placements: those of `image_id`, or
/// every image when `image_id` is `None`. Other modes have nothing to delete.
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
    Some(image_id) => envelope.kitty_command(format_args!("q=2,a=d,d=i,i={image_id}")),
    None => envelope.kitty_command(format_args!("q=2,a=d,d=A")),
  })
}

/// Sequence that deletes one placement of a kitty image.
pub fn erase_kitty_placement_sequence(
  passthrough: Option<&str>,
  image_id: u32,
  placement_id: u32,
) -> Option<String> {
  let envelope = ProtocolEnvelope::new(passthrough);
  Some(envelope.kitty_command(format_args!("q=2,a=d,d=i,i={image_id},p={placement_id}")))
}

#[cfg(test)]
mod tests {
  use image::{DynamicImage, RgbImage};

  use super::{
    KittyTransmit, ProtocolEnvelope, RenderMode, encode_image, erase_kitty_placement_sequence,
    erase_sequence, kitty_image_id, virtual_placement,
  };

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
  fn upload_is_split_into_protocol_sized_chunks() {
    // 40x40 RGB = 4800 raw bytes: one full 3072-byte chunk and a remainder.
    let image = DynamicImage::ImageRgb8(RgbImage::new(40, 40));
    let out = encode_image(
      &image,
      9,
      KittyTransmit::UploadOnly,
      ProtocolEnvelope::new(None),
    );
    let out = String::from_utf8(out).unwrap();
    let chunks = out
      .split("\x1b\\")
      .filter(|chunk| !chunk.is_empty())
      .collect::<Vec<_>>();

    assert_eq!(chunks.len(), 2);
    assert!(chunks[0].starts_with("\x1b_Gq=2,a=t,f=24,s=40,v=40,i=9,m=1;"));
    assert_eq!(chunks[0].split_once(';').unwrap().1.len(), 4096);
    assert!(chunks[1].starts_with("\x1b_Gm=0;"));
  }

  #[test]
  fn sequences_are_wrapped_for_tmux() {
    let envelope = ProtocolEnvelope::new(Some("tmux"));
    assert_eq!(
      String::from_utf8(virtual_placement(envelope, 5, 0, 3)).unwrap(),
      "\x1bPtmux;\x1b\x1b_Gq=2,a=p,U=1,i=5,c=1,r=3\x1b\x1b\\\x1b\\"
    );
    assert_eq!(
      erase_sequence(RenderMode::Kitty, None, None).as_deref(),
      Some("\x1b_Gq=2,a=d,d=A\x1b\\")
    );
    assert_eq!(erase_sequence(RenderMode::Sixel, None, Some(1)), None);
    assert_eq!(
      erase_kitty_placement_sequence(Some("tmux"), 7, 11).as_deref(),
      Some("\x1bPtmux;\x1b\x1b_Gq=2,a=d,d=i,i=7,p=11\x1b\x1b\\\x1b\\")
    );
  }
}
