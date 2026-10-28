//! Shared mapping between terminal cells and source pixels.
use super::NativeImageViewport;
pub(super) use crate::slicing::PixelRegion as PixelViewport;
use anyhow::Result;
use image::DynamicImage;
pub(super) fn pixel_viewport(
  (width, height): (u32, u32),
  viewport: NativeImageViewport,
) -> Result<PixelViewport> {
  viewport.pixels(width, height)
}
pub(super) fn crop(image: &DynamicImage, viewport: NativeImageViewport) -> Result<DynamicImage> {
  viewport.pixels(image.width(), image.height())?.crop(image)
}
