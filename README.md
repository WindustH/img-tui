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
- **Encode once, draw every frame for free.** An encoded image is shared,
  not copied, each time a frame shows it, and its bytes can be cached (in
  memory or on disk) and turned back into an image later without
  re-encoding.
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

Prepare an image for a `width` x `height` cell area and encode it for the
chosen mode. The functions are async and run their heavy work on Tokio's
blocking pool:

```rust
use img_tui::{ProtocolImage, ProtocolImageSpec, native_image};

let prepared = native_image::prepare(&path, width, height, config.cell_pixels).await?;
let spec = ProtocolImageSpec {
  // Kitty only: the id the terminal knows the image by, stable per image,
  // and a placement that lets the image move without being sent again.
  image_id: Some(native_image::kitty_image_id(key.as_bytes())),
  placement_id: Some(1),
  ..ProtocolImageSpec::new(mode, width, height)
};
let image = ProtocolImage::render(&prepared, &spec, &config).await?;
```

Kitty images are shown through Unicode placeholders when
`config.kitty_unicode_placeholders` is set; sixel and iTerm2 ignore the ids.
To cache the encoded bytes, split `render` in two:

```rust
let encoded = native_image::encode_protocol(&prepared, &spec, &config).await?;
// ... store encoded.data and encoded.refresh, read them back later ...
let image = ProtocolImage::from_encoded(encoded, &spec, &config)?;
```

Draw every frame through a `ProtocolFrameRenderer`, reserving the image
areas so ratatui does not paint over them. Keep the `ProtocolImage` around:
placing it in a frame does not copy it.

```rust
use img_tui::{ProtocolFrameOutput, ProtocolFrameRenderer, reserve_protocol_area};

let mut renderer = ProtocolFrameRenderer::default();
renderer.draw(&mut terminal, |frame| {
  // ... draw widgets ...
  reserve_protocol_area(frame, area);
  ProtocolFrameOutput::new(vec![image.overlay(area)], None)
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
