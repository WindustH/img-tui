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
