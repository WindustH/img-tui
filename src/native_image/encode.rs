//! One encoder for every pixel protocol: picks how a kitty image is shown and
//! produces the bytes a [`ProtocolImage`](crate::ProtocolImage) is built from.

use anyhow::Result;

use super::{
  NativeImageConfig, NativeImageViewport, PreparedNativeImage, render_kitty_viewport_from_upload,
  render_kitty_virtual_placement, render_prepared, render_prepared_kitty_upload,
};
use crate::RenderMode;

/// What to encode an image as: the protocol, the cell area it fills and, for
/// kitty, the ids it is known by on the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProtocolImageSpec {
  pub mode: RenderMode,
  /// Size of the area the image fills, in cells; normally the size the image
  /// was [prepared](super::prepare) for. Kitty scales the image to it.
  pub width_cells: u16,
  pub height_cells: u16,
  /// Kitty image id, stable per image (see
  /// [`kitty_image_id`](super::kitty_image_id)); `None` means 1. Other modes
  /// ignore it.
  pub image_id: Option<u32>,
  /// Kitty placement id. With one, the image is uploaded once and shown by a
  /// separate placement command, so moving it does not resend the pixels.
  /// Ignored when [`NativeImageConfig::kitty_unicode_placeholders`] is set
  /// (placeholder images move with their cells) and by other modes.
  pub placement_id: Option<u32>,
}

impl ProtocolImageSpec {
  /// A spec for `mode` filling `width_cells` x `height_cells`, without kitty
  /// ids.
  pub fn new(mode: RenderMode, width_cells: u16, height_cells: u16) -> Self {
    Self {
      mode,
      width_cells,
      height_cells,
      image_id: None,
      placement_id: None,
    }
  }

  /// How a kitty image is shown under `config`; `None` for other modes.
  pub(crate) fn kitty_show(&self, config: &NativeImageConfig) -> Option<KittyShow> {
    if self.mode != RenderMode::Kitty {
      return None;
    }
    let image_id = self.image_id.unwrap_or(1).max(1);
    Some(if config.kitty_unicode_placeholders {
      KittyShow::Placeholders { image_id }
    } else if let Some(placement_id) = self.placement_id {
      KittyShow::Placement {
        image_id,
        placement_id: placement_id.max(1),
      }
    } else {
      KittyShow::Direct { image_id }
    })
  }
}

/// How a kitty image reaches the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KittyShow {
  /// Uploaded with a virtual placement that Unicode placeholder cells show.
  Placeholders { image_id: u32 },
  /// Uploaded, then shown by a placement command of its own.
  Placement { image_id: u32, placement_id: u32 },
  /// Transmitted and displayed at the cursor in one command.
  Direct { image_id: u32 },
}

/// Escape sequences for one image as plain bytes, e.g. to store in a cache.
/// Turn them into a [`ProtocolImage`](crate::ProtocolImage) with
/// [`ProtocolImage::from_encoded`](crate::ProtocolImage::from_encoded).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EncodedProtocolImage {
  /// Sequences that draw the image (for kitty placements, the upload).
  pub data: Vec<u8>,
  /// Kitty placement or virtual placement that shows the uploaded image.
  pub refresh: Option<Vec<u8>>,
}

/// Encode `prepared` as `spec` describes.
///
/// Kitty images are uploaded and shown through Unicode placeholders when
/// `config` enables them, otherwise through a placement with
/// `spec.placement_id`, otherwise displayed at the cursor directly. Sixel and
/// iTerm2 images are encoded as they are. Text modes are rejected.
pub async fn encode_protocol(
  prepared: &PreparedNativeImage,
  spec: &ProtocolImageSpec,
  config: &NativeImageConfig,
) -> Result<EncodedProtocolImage> {
  match spec.kitty_show(config) {
    Some(KittyShow::Placeholders { image_id }) => {
      let upload = render_prepared_kitty_upload(prepared, config, image_id).await?;
      let place =
        render_kitty_virtual_placement(config, image_id, spec.width_cells, spec.height_cells);
      let mut data = upload.data;
      data.extend_from_slice(&place);
      Ok(EncodedProtocolImage {
        data,
        refresh: Some(place),
      })
    }
    Some(KittyShow::Placement {
      image_id,
      placement_id,
    }) => {
      let upload = render_prepared_kitty_upload(prepared, config, image_id).await?;
      let viewport = NativeImageViewport {
        full_width_cells: spec.width_cells,
        full_height_cells: spec.height_cells,
        visible_width_cells: spec.width_cells,
        visible_height_cells: spec.height_cells,
        left_cells: 0,
        top_cells: 0,
      };
      let place = render_kitty_viewport_from_upload(&upload, viewport, config, placement_id)?;
      Ok(EncodedProtocolImage {
        data: upload.data,
        refresh: Some(place),
      })
    }
    Some(KittyShow::Direct { image_id }) => Ok(EncodedProtocolImage {
      data: render_prepared(prepared, RenderMode::Kitty, config, Some(image_id)).await?,
      refresh: None,
    }),
    None => Ok(EncodedProtocolImage {
      data: render_prepared(prepared, spec.mode, config, None).await?,
      refresh: None,
    }),
  }
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use image::{DynamicImage, RgbImage};

  use super::*;
  use crate::{ProtocolImage, ProtocolPlacement, erase_kitty_placement_sequence};

  fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
      .build()
      .expect("runtime")
  }

  /// An 8x16 pixel image: 2x2 cells of 4x8 pixels.
  fn prepared() -> PreparedNativeImage {
    PreparedNativeImage {
      image: Arc::new(DynamicImage::ImageRgb8(RgbImage::from_fn(8, 16, |x, y| {
        image::Rgb([(x * 30) as u8, (y * 15) as u8, 99])
      }))),
    }
  }

  fn config(kitty_unicode_placeholders: bool, passthrough: Option<&str>) -> NativeImageConfig {
    NativeImageConfig {
      cell_pixels: Some((4, 8)),
      passthrough: passthrough.map(str::to_string),
      kitty_unicode_placeholders,
    }
  }

  fn kitty_spec(image_id: Option<u32>, placement_id: Option<u32>) -> ProtocolImageSpec {
    ProtocolImageSpec {
      image_id,
      placement_id,
      ..ProtocolImageSpec::new(RenderMode::Kitty, 2, 2)
    }
  }

  fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).expect("utf-8")
  }

  #[test]
  fn placeholder_images_upload_then_place_virtually() {
    runtime().block_on(async {
      let prepared = prepared();
      let config = config(true, None);
      // The placement id is ignored: placeholder images need none.
      let spec = kitty_spec(Some(42), Some(7));

      let encoded = encode_protocol(&prepared, &spec, &config).await.unwrap();
      let upload = render_prepared_kitty_upload(&prepared, &config, 42)
        .await
        .unwrap();
      let place = render_kitty_virtual_placement(&config, 42, 2, 2);
      assert_eq!(encoded.data, [upload.data, place.clone()].concat());
      assert_eq!(encoded.refresh.as_deref(), Some(&place[..]));
      assert!(text(&place).contains("a=p,U=1,i=42,c=2,r=2"));

      let image = ProtocolImage::from_encoded(encoded, &spec, &config).unwrap();
      assert_eq!(
        image.placement,
        Some(ProtocolPlacement::KittyUnicode { image_id: 42 })
      );
      assert_eq!(image.erase.as_deref(), Some("\x1b_Gq=2,a=d,d=i,i=42\x1b\\"));
      assert!(image.data.starts_with("\x1b_Gq=2,a=t,"));
    });
  }

  #[test]
  fn placement_images_keep_upload_and_placement_apart() {
    runtime().block_on(async {
      let prepared = prepared();
      let config = config(false, Some("tmux"));
      let spec = kitty_spec(Some(42), Some(7));

      let image = ProtocolImage::render(&prepared, &spec, &config)
        .await
        .unwrap();
      assert!(image.data.contains("a=t,f=24,s=8,v=16,i=42,"));
      assert!(!image.data.contains("a=p"));
      let refresh = image.refresh.as_deref().expect("placement sequence");
      assert!(refresh.starts_with("\x1bPtmux;"));
      assert!(refresh.contains("a=p,C=1,z=-1,i=42,p=7,x=0,y=0,w=8,h=16,c=2,r=2"));
      assert_eq!(
        image.placement,
        Some(ProtocolPlacement::KittyPlacement {
          image_id: 42,
          placement_id: 7,
        })
      );
      assert_eq!(
        image.erase.as_deref(),
        erase_kitty_placement_sequence(Some("tmux"), 42, 7).as_deref()
      );
    });
  }

  #[test]
  fn kitty_images_without_placement_display_at_the_cursor() {
    runtime().block_on(async {
      let prepared = prepared();
      let config = config(false, None);
      let spec = kitty_spec(None, None);

      let image = ProtocolImage::render(&prepared, &spec, &config)
        .await
        .unwrap();
      assert!(
        image
          .data
          .starts_with("\x1b_Gq=2,a=T,z=-1,C=1,f=24,s=8,v=16,i=1,")
      );
      assert!(image.refresh.is_none() && image.placement.is_none());
      assert_eq!(image.erase.as_deref(), Some("\x1b_Gq=2,a=d,d=i,i=1\x1b\\"));
    });
  }

  #[test]
  fn sixel_and_iterm_images_ignore_kitty_ids() {
    runtime().block_on(async {
      let prepared = prepared();
      let config = config(true, None);
      for (mode, start) in [
        (RenderMode::Sixel, "\x1bP9;1q"),
        (RenderMode::Iterm2, "\x1b]1337;File=inline=1;"),
      ] {
        let spec = ProtocolImageSpec {
          image_id: Some(42),
          placement_id: Some(7),
          ..ProtocolImageSpec::new(mode, 2, 2)
        };
        let encoded = encode_protocol(&prepared, &spec, &config).await.unwrap();
        let direct = render_prepared(&prepared, mode, &config, None)
          .await
          .unwrap();
        assert_eq!(encoded.data, direct);
        assert!(encoded.refresh.is_none());

        let image = ProtocolImage::from_encoded(encoded, &spec, &config).unwrap();
        assert_eq!(image.mode, mode);
        assert!(image.data.starts_with(start));
        assert!(image.placement.is_none() && image.erase.is_none());
      }
    });
  }

  #[test]
  fn text_modes_are_not_encoded() {
    runtime().block_on(async {
      let spec = ProtocolImageSpec::new(RenderMode::Symbols, 2, 2);
      let config = config(false, None);
      assert!(encode_protocol(&prepared(), &spec, &config).await.is_err());
      assert!(
        ProtocolImage::from_encoded(EncodedProtocolImage::default(), &spec, &config).is_err()
      );
    });
  }

  #[test]
  fn render_prepared_shows_kitty_images_even_with_placeholders_enabled() {
    runtime().block_on(async {
      let out = render_prepared(&prepared(), RenderMode::Kitty, &config(true, None), Some(5))
        .await
        .unwrap();
      let out = text(&out);
      assert!(out.starts_with("\x1b_Gq=2,a=T,z=-1,C=1,"));
      assert!(!out.contains("U=1"));
    });
  }
}
