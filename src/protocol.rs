//! Protocol images and where a frame shows them.

use std::{
  fmt,
  hash::{DefaultHasher, Hash, Hasher},
  sync::Arc,
};

use anyhow::{Context, Result, bail};
use ratatui::layout::Rect;

use crate::{
  NativeImageConfig, RenderMode,
  native_image::{
    self, EncodedProtocolImage, KittyShow, PreparedNativeImage, ProtocolImageSpec,
    erase_kitty_placement_sequence, erase_sequence,
  },
};

/// How a kitty image is positioned on the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolPlacement {
  /// Virtual placement shown through Unicode placeholder cells, which
  /// [`ProtocolFrameRenderer`](crate::ProtocolFrameRenderer) fills in.
  KittyUnicode { image_id: u32 },
  /// Regular placement; moving it only needs a new placement command.
  KittyPlacement { image_id: u32, placement_id: u32 },
}

/// An image encoded for a terminal graphics protocol, ready to be shown in
/// any frame.
///
/// Build one with [`render`](Self::render), or with
/// [`native_image::encode_protocol`] and [`from_encoded`](Self::from_encoded)
/// when the encoded bytes are cached. The payloads are shared: cloning the
/// image or placing it with [`overlay`](Self::overlay) never copies them.
#[derive(Clone)]
pub struct ProtocolImage {
  pub mode: RenderMode,
  /// Escape sequences that draw the image, written with the cursor at the
  /// top-left of its area. For [`ProtocolPlacement::KittyPlacement`] this is
  /// the upload only and `refresh` holds the placement.
  pub data: Arc<str>,
  /// Lighter sequence that shows an image already on the terminal again
  /// (a kitty placement or virtual placement), used when only its position
  /// changes.
  pub refresh: Option<Arc<str>>,
  pub placement: Option<ProtocolPlacement>,
  /// Identifies the image content; a different value forces a rewrite.
  pub fingerprint: u64,
  /// Sequence that deletes the image from the terminal (kitty only).
  pub erase: Option<Arc<str>>,
}

/// An image to show in a frame: a [`ProtocolImage`] and the cells it covers.
#[derive(Debug, Clone)]
pub struct ProtocolOverlay {
  /// Cells the image covers.
  pub area: Rect,
  pub image: ProtocolImage,
}

impl ProtocolImage {
  /// Encode `prepared` as `spec` describes, in one step.
  pub async fn render(
    prepared: &PreparedNativeImage,
    spec: &ProtocolImageSpec,
    config: &NativeImageConfig,
  ) -> Result<Self> {
    let encoded = native_image::encode_protocol(prepared, spec, config).await?;
    Self::from_encoded(encoded, spec, config)
  }

  /// The image for bytes that [`native_image::encode_protocol`] produced
  /// with the same `spec` and `config`, for example read back from a cache.
  ///
  /// Fails when the bytes are not UTF-8, when a kitty placement sequence the
  /// spec calls for is missing, or for text render modes.
  pub fn from_encoded(
    encoded: EncodedProtocolImage,
    spec: &ProtocolImageSpec,
    config: &NativeImageConfig,
  ) -> Result<Self> {
    if !spec.mode.is_protocol() {
      bail!("{} is not a native image mode", spec.mode.label());
    }
    let passthrough = config.passthrough.as_deref();
    let (placement, erase) = match spec.kitty_show(config) {
      Some(KittyShow::Placeholders { image_id }) => (
        Some(ProtocolPlacement::KittyUnicode { image_id }),
        erase_sequence(RenderMode::Kitty, passthrough, Some(image_id)),
      ),
      Some(KittyShow::Placement {
        image_id,
        placement_id,
      }) => (
        Some(ProtocolPlacement::KittyPlacement {
          image_id,
          placement_id,
        }),
        erase_kitty_placement_sequence(passthrough, image_id, placement_id),
      ),
      Some(KittyShow::Direct { image_id }) => (
        None,
        erase_sequence(RenderMode::Kitty, passthrough, Some(image_id)),
      ),
      None => (None, None),
    };
    if placement.is_some() && encoded.refresh.is_none() {
      bail!("kitty image bytes lack the placement sequence");
    }

    let fingerprint = fingerprint(&encoded.data, encoded.refresh.as_deref());
    let data = String::from_utf8(encoded.data).context("image data is not UTF-8")?;
    let refresh = encoded
      .refresh
      .map(String::from_utf8)
      .transpose()
      .context("image refresh sequence is not UTF-8")?;
    Ok(Self {
      mode: spec.mode,
      data: data.into(),
      refresh: refresh.map(Arc::from),
      placement,
      fingerprint,
      erase: erase.map(Arc::from),
    })
  }

  /// Show the image over `area` in a frame. Only reference counts change;
  /// call it every frame.
  pub fn overlay(&self, area: Rect) -> ProtocolOverlay {
    ProtocolOverlay {
      area,
      image: self.clone(),
    }
  }

  /// Total length in bytes of the escape sequences the image holds, e.g.
  /// for cache accounting.
  pub fn payload_len(&self) -> usize {
    self.data.len()
      + self.refresh.as_ref().map_or(0, |refresh| refresh.len())
      + self.erase.as_ref().map_or(0, |erase| erase.len())
  }
}

/// Payloads are summarized by length: they can be megabytes long.
impl fmt::Debug for ProtocolImage {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("ProtocolImage")
      .field("mode", &self.mode)
      .field("data_len", &self.data.len())
      .field(
        "refresh_len",
        &self.refresh.as_ref().map(|refresh| refresh.len()),
      )
      .field("placement", &self.placement)
      .field("fingerprint", &self.fingerprint)
      .field("erase", &self.erase)
      .finish()
  }
}

/// Content hash of an encoded image. It only has to tell images apart within
/// one process, so the standard library's hasher is enough.
fn fingerprint(data: &[u8], refresh: Option<&[u8]>) -> u64 {
  let mut hasher = DefaultHasher::new();
  data.hash(&mut hasher);
  refresh.hash(&mut hasher);
  hasher.finish()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn config(kitty_unicode_placeholders: bool) -> NativeImageConfig {
    NativeImageConfig {
      cell_pixels: Some((4, 8)),
      passthrough: None,
      kitty_unicode_placeholders,
    }
  }

  fn encoded(data: &[u8], refresh: Option<&[u8]>) -> EncodedProtocolImage {
    EncodedProtocolImage {
      data: data.to_vec(),
      refresh: refresh.map(<[u8]>::to_vec),
    }
  }

  fn sixel(data: &[u8]) -> Result<ProtocolImage> {
    ProtocolImage::from_encoded(
      encoded(data, None),
      &ProtocolImageSpec::new(RenderMode::Sixel, 4, 2),
      &config(false),
    )
  }

  #[test]
  fn fingerprint_follows_the_encoded_bytes() {
    let spec = ProtocolImageSpec {
      image_id: Some(3),
      placement_id: Some(4),
      ..ProtocolImageSpec::new(RenderMode::Kitty, 4, 2)
    };
    let image = |data: &[u8], refresh: &[u8]| {
      ProtocolImage::from_encoded(encoded(data, Some(refresh)), &spec, &config(false))
        .unwrap()
        .fingerprint
    };

    assert_eq!(image(b"upload", b"place"), image(b"upload", b"place"));
    assert_ne!(image(b"upload", b"place"), image(b"upload2", b"place"));
    assert_ne!(image(b"upload", b"place"), image(b"upload", b"place2"));
    // Moving bytes between the parts is a different image too.
    assert_ne!(image(b"upload", b"place"), image(b"uploadp", b"lace"));
  }

  #[test]
  fn invalid_bytes_are_rejected() {
    assert!(sixel(b"\xff\xfe").is_err());

    // A kitty placement without its placement sequence could never show.
    let spec = ProtocolImageSpec {
      image_id: Some(3),
      placement_id: Some(4),
      ..ProtocolImageSpec::new(RenderMode::Kitty, 4, 2)
    };
    let error = ProtocolImage::from_encoded(encoded(b"upload", None), &spec, &config(false));
    assert!(error.is_err());
    let error = ProtocolImage::from_encoded(encoded(b"upload", None), &spec, &config(true));
    assert!(error.is_err());
  }

  #[test]
  fn overlays_share_the_payloads() {
    let mut image = sixel(b"\x1bP9;1q#0~-\x1b\\").unwrap();
    image.erase = Some("erase".into());
    let overlay = image.overlay(Rect::new(1, 2, 4, 2));

    assert_eq!(overlay.area, Rect::new(1, 2, 4, 2));
    assert!(Arc::ptr_eq(&overlay.image.data, &image.data));
    assert!(Arc::ptr_eq(
      overlay.image.erase.as_ref().unwrap(),
      image.erase.as_ref().unwrap()
    ));
    assert_eq!(overlay.image.fingerprint, image.fingerprint);
    assert_eq!(image.payload_len(), image.data.len() + "erase".len());
  }

  #[test]
  fn debug_output_leaves_payloads_out() {
    let image = sixel(&[b'x'; 4096]).unwrap();
    let debug = format!("{:?}", image.overlay(Rect::new(0, 0, 1, 1)));
    assert!(debug.contains("data_len: 4096"));
    assert!(!debug.contains("xxxx"));
  }
}
