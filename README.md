# img-tui

Real images in [ratatui](https://ratatui.rs) applications: terminal detection,
image preparation and flicker-free drawing for the kitty graphics protocol,
sixel and iTerm2 inline images.

It powers the image views of pdf-tui, gallery-tui and music-tui.

## Features

- **Knows what the terminal can show.** Recognizes kitty, Ghostty, WezTerm,
  foot, Konsole, iTerm2, Windows Terminal, VS Code, Rio, Warp and others from
  their environment, asks the terminal directly when unsure, and reports the
  cell size in pixels so images fit their cells exactly.
- **Works inside multiplexers.** Images pass through tmux (passthrough is
  switched on for the pane automatically) and Zellij 0.45+, where kitty
  graphics are used once Zellij and the host terminal both confirm support.
- **Picks a sensible order of render modes**: pixel protocols first, then
  text fallbacks for [chafa](https://hpjansson.org/chafa/), with matching
  chafa arguments for the terminal's color depth. Users can force the modes
  with an environment variable.
- **Good-looking images, prepared fast.** Common image formats, EXIF
  orientation and embedded ICC color profiles are handled; images are scaled
  down to the cell area with SIMD resizing and never blown up.
- **Efficient protocols.** Kitty images are uploaded once and can be moved or
  cropped without being sent again; on kitty, Ghostty and Rio they are drawn
  through Unicode placeholders, so dialogs can cover part of an image. Sixel
  images use a 256-color palette with compact run-length output and keep
  transparent areas transparent. iTerm2 images are sent as PNG or JPEG.
- **No flicker, no leftovers.** Each frame writes only the images that
  changed, erases the ones that went away only after their replacements are
  on screen, keeps the text under images in step (hover highlights,
  transparent pixels), redraws correctly after the terminal is resized, and
  can keep old images up while new ones are still rendering.

Active terminal probing is Unix-only; on Windows detection relies on the
environment.

## Usage

```toml
[dependencies]
img-tui = { git = "https://github.com/WindustH/img-tui" }
```

Detect the terminal once, before entering the alternate screen, since the
probe talks to the terminal:

```rust
use img_tui::{NativeImageConfig, RenderMode, capability};

let terminal_capability = capability::detect();
let modes = capability::render_modes_override_from_env()
  .unwrap_or_else(|| terminal_capability.preferred_render_modes("auto"));
let config = NativeImageConfig {
  cell_pixels: terminal_capability.cell_pixels,
  passthrough: terminal_capability.passthrough().map(str::to_string),
  kitty_unicode_placeholders: terminal_capability.kitty_unicode_placeholders(),
};
```

Render an image for a `width` x `height` cell area as a `ProtocolOverlay`.
The functions are async and run their heavy work on Tokio's blocking pool.
For sixel and iTerm2:

```rust
use img_tui::{ProtocolOverlay, native_image};

let prepared = native_image::prepare(&path, width, height, config.cell_pixels).await?;
let data = native_image::render_prepared(&prepared, mode, &config, None).await?;
let overlay = ProtocolOverlay {
  area,        // where the image goes in the frame
  mode,
  data: String::from_utf8(data)?,
  refresh: None,
  placement: None,
  fingerprint, // any value that changes when the image changes
  erase: None,
};
```

For kitty, upload the image once and describe how to show it, so that moving
it later only needs the light `refresh` sequence. With Unicode placeholders
(`config.kitty_unicode_placeholders`):

```rust
use img_tui::ProtocolPlacement;

let image_id = native_image::kitty_image_id(key.as_bytes()); // stable per image key
let upload = native_image::render_prepared_kitty_upload(&prepared, &config, image_id).await?;
let place = native_image::render_kitty_virtual_placement(&config, image_id, width, height);
let overlay = ProtocolOverlay {
  area,
  mode: RenderMode::Kitty,
  data: String::from_utf8([upload.data, place.clone()].concat())?,
  refresh: Some(String::from_utf8(place)?),
  placement: Some(ProtocolPlacement::KittyUnicode { image_id }),
  fingerprint,
  erase: native_image::erase_sequence(RenderMode::Kitty, config.passthrough.as_deref(), Some(image_id)),
};
```

Without placeholders, use `render_kitty_viewport_from_upload` for `refresh`,
`ProtocolPlacement::KittyPlacement` and `erase_kitty_placement_sequence`
instead; the upload alone goes in `data`.

Draw every frame through a `ProtocolFrameRenderer`, reserving the image areas
so ratatui does not paint over them:

```rust
use img_tui::{ProtocolFrameOutput, ProtocolFrameRenderer, display::skip_protocol_areas};

let mut renderer = ProtocolFrameRenderer::default();
renderer.draw(&mut terminal, |frame| {
  // ... draw widgets ...
  skip_protocol_areas(frame.buffer_mut(), [overlay.area]);
  ProtocolFrameOutput::new(vec![overlay.clone()], None)
})?;

// Before suspending or exiting: remove the images.
let reset = native_image::erase_sequence(RenderMode::Kitty, config.passthrough.as_deref(), None);
renderer.clear_and_reset(terminal.backend_mut(), reset.as_deref())?;
```

`ProtocolFrameOutput` also carries the cursor position, rectangles of dialogs
drawn over kitty placeholder images (`occluders`), and whether images whose
replacements are not ready yet should stay on screen (`preserve_overlays`).

### Forcing render modes

Applications that call `render_modes_override_from_env` let users pick modes
with `GALLERY_TUI_RENDER_MODES`, a comma-separated list such as
`kitty,symbols`. Accepted values are `kitty`, `sixel`, `iterm`, `symbols`,
`ascii`, `off` (text only) and `auto` (detect).

## License

MIT
