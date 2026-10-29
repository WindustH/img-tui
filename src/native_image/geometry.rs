//! Typed regular Kitty placements, decoded once at the encoded-cache boundary.

use anyhow::{Context, Result, ensure};

use super::{NativeImageViewport, envelope::ProtocolEnvelope, kitty};
use crate::slicing::PixelRegion;

/// Source pixels and destination cells of a regular Kitty placement.
#[derive(Debug, Clone, Copy)]
pub struct KittyPlacementGeometry {
  pub image_id: u32,
  pub placement_id: u32,
  pub pixels: PixelRegion,
  pub columns: u16,
  pub rows: u16,
  envelope: ProtocolEnvelope,
}

impl KittyPlacementGeometry {
  pub(crate) fn decode(
    command: &str,
    passthrough: Option<&str>,
    image_id: u32,
    placement_id: u32,
  ) -> Result<Self> {
    let envelope = ProtocolEnvelope::new(passthrough);
    let prefix = format!("{}_G", envelope.start);
    let suffix = format!("{}\\{}", envelope.escape, envelope.close);
    let body = command
      .strip_prefix(&prefix)
      .and_then(|s| s.strip_suffix(&suffix))
      .context("invalid Kitty placement envelope")?;
    let value = |name: &str| -> Result<u32> {
      let mut matches = body.split(',').filter_map(|field| field.strip_prefix(name));
      let number = matches
        .next()
        .context("missing Kitty placement field")?
        .parse()?;
      ensure!(matches.next().is_none(), "duplicate Kitty placement field");
      Ok(number)
    };
    ensure!(
      body.split(',').any(|f| f == "a=p"),
      "expected Kitty placement"
    );
    ensure!(
      value("i=")? == image_id && value("p=")? == placement_id,
      "Kitty placement identity mismatch"
    );
    let pixels = PixelRegion {
      x: value("x=")?,
      y: value("y=")?,
      width: value("w=")?,
      height: value("h=")?,
    };
    let columns = u16::try_from(value("c=")?)?;
    let rows = u16::try_from(value("r=")?)?;
    ensure!(
      pixels.width > 0 && pixels.height > 0 && columns > 0 && rows > 0,
      "empty Kitty placement"
    );
    ensure!(
      pixels.x.checked_add(pixels.width).is_some() && pixels.y.checked_add(pixels.height).is_some(),
      "Kitty source rectangle overflow"
    );
    Ok(Self {
      image_id,
      placement_id,
      pixels,
      columns,
      rows,
      envelope,
    })
  }

  /// Derive a separately removable crop; repeated crops reuse their IDs.
  pub(crate) fn crop(self, pixels: PixelRegion, columns: u16, rows: u16, piece: usize) -> Self {
    let placement_id = if piece == 0 {
      self.placement_id
    } else {
      kitty::kitty_image_id(
        format!("occlusion:{}:{}:{piece}", self.image_id, self.placement_id).as_bytes(),
      )
    };
    Self {
      placement_id,
      pixels,
      columns,
      rows,
      ..self
    }
  }

  pub(crate) fn refresh(self) -> String {
    let viewport = NativeImageViewport {
      full_width_cells: self.columns,
      full_height_cells: self.rows,
      visible_width_cells: self.columns,
      visible_height_cells: self.rows,
      left_cells: 0,
      top_cells: 0,
    };
    // The encoder produces ASCII protocol sequences.
    String::from_utf8(kitty::placement(
      self.pixels,
      viewport,
      self.envelope,
      self.image_id,
      self.placement_id,
    ))
    .expect("ASCII Kitty command")
  }

  pub(crate) fn erase(self) -> String {
    self.envelope.kitty_command(format_args!(
      "q=2,a=d,d=i,i={},p={}",
      self.image_id, self.placement_id
    ))
  }
}
