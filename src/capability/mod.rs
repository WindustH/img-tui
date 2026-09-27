//! Terminal image capability detection.
//!
//! [`detect`] combines environment heuristics (`TERM`, `TERM_PROGRAM`,
//! terminal-specific variables, tmux/Zellij/screen markers) with an active
//! probe of the controlling terminal (kitty graphics query, XTVERSION, cell
//! size in pixels, primary device attributes) and reports which pixel
//! protocols the terminal can display. [`TerminalCapability::preferred_render_modes`]
//! turns that into an ordered list of [`RenderMode`]s to try, ending with the
//! text fallbacks.

mod brand;
mod multiplexer;
mod probe;
#[cfg(unix)]
mod tty;

use std::env;

use tracing::warn;

use self::{
  brand::TerminalBrand,
  multiplexer::Multiplexer,
  probe::{ProbeRequests, probe_terminal, window_cell_pixels},
};

/// A pixel graphics protocol the terminal can display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelProtocol {
  Kitty,
  Sixel,
  Iterm2,
}

/// How an image is drawn: one of the pixel protocols, or chafa text output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RenderMode {
  Kitty,
  Sixel,
  Iterm2,
  Symbols,
  Ascii,
}

impl RenderMode {
  /// Value for chafa's `--format` option.
  pub fn chafa_format(self) -> &'static str {
    match self {
      Self::Kitty => "kitty",
      Self::Sixel => "sixels",
      Self::Iterm2 => "iterm",
      Self::Symbols | Self::Ascii => "symbols",
    }
  }

  /// Whether this mode draws pixels through a terminal graphics protocol.
  pub fn is_protocol(self) -> bool {
    matches!(self, Self::Kitty | Self::Sixel | Self::Iterm2)
  }

  pub fn label(self) -> &'static str {
    match self {
      Self::Kitty => "kitty",
      Self::Sixel => "sixel",
      Self::Iterm2 => "iterm",
      Self::Symbols => "symbols",
      Self::Ascii => "ascii",
    }
  }
}

/// Render modes forced through the environment variable `var`, if it is set
/// to anything other than `auto`.
///
/// Each app names its own variable (for example `MY_APP_RENDER_MODES`), so
/// overriding one app doesn't affect the others. The value is a list of
/// modes in order of preference, such as `kitty,symbols` or `sixel`; `auto`
/// (or an empty value) keeps detection, and `off`/`text` selects only the
/// text fallbacks. Unknown modes are logged and skipped.
pub fn render_modes_override_from_env(var: &str) -> Option<Vec<RenderMode>> {
  let value = env::var(var).ok()?;
  parse_render_modes_override(var, &value)
}

fn parse_render_modes_override(var: &str, value: &str) -> Option<Vec<RenderMode>> {
  if value.trim().is_empty() || value.trim().eq_ignore_ascii_case("auto") {
    return None;
  }

  let mut modes = Vec::new();
  for token in value
    .split(|ch: char| ch == ',' || ch == ':' || ch.is_ascii_whitespace())
    .map(str::trim)
    .filter(|token| !token.is_empty())
  {
    match token.to_ascii_lowercase().as_str() {
      "auto" => {}
      "kitty" | "kgp" => push_unique(&mut modes, RenderMode::Kitty),
      "sixel" | "sixels" => push_unique(&mut modes, RenderMode::Sixel),
      "iterm" | "iterm2" | "iip" => push_unique(&mut modes, RenderMode::Iterm2),
      "symbols" | "symbol" => push_unique(&mut modes, RenderMode::Symbols),
      "ascii" => push_unique(&mut modes, RenderMode::Ascii),
      "off" | "none" | "text" | "chafa" | "fallback" => {
        push_unique(&mut modes, RenderMode::Symbols);
        push_unique(&mut modes, RenderMode::Ascii);
      }
      unknown => warn!(
        env = var,
        value,
        token = unknown,
        "ignoring unknown render mode override"
      ),
    }
  }

  if modes.is_empty() {
    warn!(
      env = var,
      value, "render mode override did not contain any known modes"
    );
    None
  } else {
    Some(modes)
  }
}

fn push_unique<T: PartialEq>(items: &mut Vec<T>, item: T) {
  if !items.contains(&item) {
    items.push(item);
  }
}

/// What [`detect`] found out about the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalCapability {
  /// `TERM` (of the outer terminal when running inside tmux).
  pub term: Option<String>,
  /// `TERM_PROGRAM` (of the outer terminal when running inside tmux).
  pub term_program: Option<String>,
  pub colorterm: Option<String>,
  /// `"tmux"`, `"zellij"` or `"screen"`.
  pub multiplexer: Option<String>,
  /// Terminal name from the probe or, failing that, the environment.
  pub brand: Option<String>,
  pub probe: TerminalProbe,
  /// Supported pixel protocols, in detection order.
  pub pixel_protocols: Vec<PixelProtocol>,
  pub color_level: ColorLevel,
  /// Cell size in pixels, `(width, height)`, when known.
  pub cell_pixels: Option<(u16, u16)>,
}

/// Outcome of the active terminal probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalProbe {
  pub attempted: bool,
  pub response_bytes: usize,
  pub kitty_graphics: bool,
  pub sixel: bool,
  pub cell_pixels: Option<(u16, u16)>,
  pub brand: Option<String>,
  pub error: Option<String>,
}

impl TerminalProbe {
  fn skipped(message: impl Into<String>) -> Self {
    Self {
      attempted: false,
      response_bytes: 0,
      kitty_graphics: false,
      sixel: false,
      cell_pixels: None,
      brand: None,
      error: Some(message.into()),
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorLevel {
  TrueColor,
  Ansi256,
  Ansi16,
  Mono,
}

impl TerminalCapability {
  /// Render modes to try in order: detected pixel protocols (kitty, sixel,
  /// iTerm2) followed by the `Symbols` and `Ascii` text fallbacks.
  ///
  /// Inside Zellij only protocols confirmed by the probe are used, and sixel
  /// additionally requires `zellij_sixel` to be `on`/`true`/`yes`, or `auto`
  /// with sixel reported by the probe.
  pub fn preferred_render_modes(&self, zellij_sixel: &str) -> Vec<RenderMode> {
    let mut modes = Vec::new();
    if self.multiplexer.as_deref() == Some("zellij") {
      if self.pixel_protocols.contains(&PixelProtocol::Kitty) {
        modes.push(RenderMode::Kitty);
      }
      let allow_sixel = match zellij_sixel.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" => self.pixel_protocols.contains(&PixelProtocol::Sixel),
        "auto" => self.probe.sixel && self.pixel_protocols.contains(&PixelProtocol::Sixel),
        _ => false,
      };
      if allow_sixel {
        modes.push(RenderMode::Sixel);
      }
    } else {
      for (protocol, mode) in [
        (PixelProtocol::Kitty, RenderMode::Kitty),
        (PixelProtocol::Sixel, RenderMode::Sixel),
        (PixelProtocol::Iterm2, RenderMode::Iterm2),
      ] {
        if self.pixel_protocols.contains(&protocol) {
          modes.push(mode);
        }
      }
    }
    modes.push(RenderMode::Symbols);
    modes.push(RenderMode::Ascii);
    modes
  }

  /// chafa `--colors` argument matching the terminal's color depth.
  pub fn colors_arg(&self) -> &'static str {
    match self.color_level {
      ColorLevel::TrueColor => "--colors=full",
      ColorLevel::Ansi256 => "--colors=256",
      ColorLevel::Ansi16 => "--colors=16",
      ColorLevel::Mono => "--colors=none",
    }
  }

  /// chafa `--symbols` argument for the text fallback.
  pub fn symbols_arg(&self) -> &'static str {
    match self.color_level {
      ColorLevel::Mono => "--symbols=ascii",
      _ => "--symbols=block",
    }
  }

  /// Passthrough wrapping image sequences need (`"tmux"` or `"screen"`), for
  /// [`NativeImageConfig::passthrough`](crate::NativeImageConfig::passthrough).
  pub fn passthrough(&self) -> Option<&'static str> {
    match self.multiplexer.as_deref() {
      Some("tmux") => Some("tmux"),
      Some("screen") => Some("screen"),
      _ => None,
    }
  }

  /// Whether kitty images should be shown through Unicode placeholder cells
  /// (kitty, Ghostty and Rio outside Zellij), which lets text drawn over the
  /// image (dialogs, popups) hide it cell by cell.
  pub fn kitty_unicode_placeholders(&self) -> bool {
    self.multiplexer.as_deref() != Some("zellij")
      && matches!(self.brand.as_deref(), Some("kitty" | "ghostty" | "rio"))
  }
}

/// Detect the terminal's image capabilities.
///
/// On Unix this briefly switches the controlling terminal to raw mode and
/// queries it (up to about 800 ms when the terminal stays silent), so call it
/// before entering the TUI and before reading input. Inside tmux it also
/// enables `allow-passthrough` for the current pane.
pub fn detect() -> TerminalCapability {
  let multiplexer = Multiplexer::detect();
  if multiplexer == Some(Multiplexer::Tmux) {
    multiplexer::enable_tmux_passthrough();
  }
  let (term, term_program) = terminal_identity(multiplexer);
  let colorterm = env::var("COLORTERM").ok();
  let env_brand = TerminalBrand::from_env(term.as_deref(), term_program.as_deref());

  let requests = ProbeRequests::plan(env_brand.is_some(), multiplexer);
  let mut active_probe = probe_terminal(multiplexer, requests);
  if active_probe.summary.cell_pixels.is_none() {
    active_probe.summary.cell_pixels = window_cell_pixels();
  }
  let brand = active_probe.brand.or(env_brand);

  let term_lower = term.as_deref().unwrap_or_default().to_ascii_lowercase();
  let program_lower = term_program
    .as_deref()
    .unwrap_or_default()
    .to_ascii_lowercase();
  let pixel_protocols = pixel_protocols(
    &active_probe.summary,
    brand,
    &term_lower,
    &program_lower,
    multiplexer,
  );
  let color_level = color_level(&term_lower, &program_lower, colorterm.as_deref());

  TerminalCapability {
    term,
    term_program,
    colorterm,
    multiplexer: multiplexer.map(|multiplexer| multiplexer.label().to_string()),
    brand: brand.map(|brand| brand.label().to_string()),
    cell_pixels: active_probe.summary.cell_pixels,
    probe: active_probe.summary,
    pixel_protocols,
    color_level,
  }
}

/// `TERM` and `TERM_PROGRAM` describing the real terminal.
fn terminal_identity(multiplexer: Option<Multiplexer>) -> (Option<String>, Option<String>) {
  let (mux_term, mux_term_program) = if multiplexer == Some(Multiplexer::Tmux) {
    multiplexer::tmux_outer_terminal()
  } else {
    (None, None)
  };
  (
    mux_term.or_else(|| env::var("TERM").ok()),
    mux_term_program.or_else(|| env::var("TERM_PROGRAM").ok()),
  )
}

fn pixel_protocols(
  probe: &TerminalProbe,
  brand: Option<TerminalBrand>,
  term_lower: &str,
  program_lower: &str,
  multiplexer: Option<Multiplexer>,
) -> Vec<PixelProtocol> {
  let mut protocols = Vec::new();
  if probe.kitty_graphics {
    push_unique(&mut protocols, PixelProtocol::Kitty);
  }
  if probe.sixel {
    push_unique(&mut protocols, PixelProtocol::Sixel);
  }
  for protocol in brand.map(TerminalBrand::protocols).unwrap_or_default() {
    push_unique(&mut protocols, *protocol);
  }
  for protocol in brand::environment_protocol_hints(term_lower, program_lower) {
    push_unique(&mut protocols, protocol);
  }
  multiplexer::constrain_protocols(&mut protocols, multiplexer, probe);
  protocols
}

fn color_level(term_lower: &str, program_lower: &str, colorterm: Option<&str>) -> ColorLevel {
  let colorterm_lower = colorterm.unwrap_or_default().to_ascii_lowercase();
  if colorterm_lower.contains("truecolor")
    || colorterm_lower.contains("24bit")
    || program_lower.contains("wezterm")
    || term_lower.contains("kitty")
  {
    ColorLevel::TrueColor
  } else if term_lower.contains("256color") {
    ColorLevel::Ansi256
  } else if term_lower == "dumb" || term_lower.is_empty() {
    ColorLevel::Mono
  } else {
    ColorLevel::Ansi16
  }
}

#[cfg(test)]
mod tests {
  use super::{
    ColorLevel, PixelProtocol, RenderMode, TerminalCapability, TerminalProbe,
    multiplexer::{Multiplexer, constrain_protocols},
    parse_render_modes_override,
  };

  fn zellij_capability(
    kitty_graphics: bool,
    sixel: bool,
    pixel_protocols: Vec<PixelProtocol>,
  ) -> TerminalCapability {
    TerminalCapability {
      term: Some("xterm-kitty".to_string()),
      term_program: None,
      colorterm: Some("truecolor".to_string()),
      multiplexer: Some("zellij".to_string()),
      brand: Some("kitty".to_string()),
      probe: TerminalProbe {
        attempted: true,
        response_bytes: 32,
        kitty_graphics,
        sixel,
        cell_pixels: Some((10, 20)),
        brand: None,
        error: None,
      },
      pixel_protocols,
      color_level: ColorLevel::TrueColor,
      cell_pixels: Some((10, 20)),
    }
  }

  #[test]
  fn zellij_prefers_probed_kitty_graphics() {
    let capability =
      zellij_capability(true, true, vec![PixelProtocol::Kitty, PixelProtocol::Sixel]);

    assert_eq!(
      capability.preferred_render_modes("off"),
      vec![RenderMode::Kitty, RenderMode::Symbols, RenderMode::Ascii]
    );
    assert_eq!(
      capability.preferred_render_modes("auto"),
      vec![
        RenderMode::Kitty,
        RenderMode::Sixel,
        RenderMode::Symbols,
        RenderMode::Ascii,
      ]
    );
  }

  #[test]
  fn zellij_ignores_unprobed_outer_terminal_hints() {
    let probe = zellij_capability(false, false, Vec::new()).probe;
    let mut protocols = vec![
      PixelProtocol::Kitty,
      PixelProtocol::Sixel,
      PixelProtocol::Iterm2,
    ];

    constrain_protocols(&mut protocols, Some(Multiplexer::Zellij), &probe);

    assert!(protocols.is_empty());
  }

  #[test]
  fn zellij_keeps_protocols_confirmed_by_its_probe() {
    let probe = zellij_capability(true, true, Vec::new()).probe;
    let mut protocols = vec![
      PixelProtocol::Kitty,
      PixelProtocol::Sixel,
      PixelProtocol::Iterm2,
    ];

    constrain_protocols(&mut protocols, Some(Multiplexer::Zellij), &probe);

    assert_eq!(protocols, vec![PixelProtocol::Kitty, PixelProtocol::Sixel]);
  }

  #[test]
  fn zellij_disables_unsupported_unicode_placeholders() {
    let capability = zellij_capability(true, false, vec![PixelProtocol::Kitty]);

    assert!(!capability.kitty_unicode_placeholders());
  }

  #[test]
  fn outside_zellij_modes_follow_protocol_priority() {
    let mut capability = zellij_capability(
      false,
      false,
      vec![PixelProtocol::Iterm2, PixelProtocol::Sixel],
    );
    capability.multiplexer = None;

    assert_eq!(
      capability.preferred_render_modes(""),
      vec![
        RenderMode::Sixel,
        RenderMode::Iterm2,
        RenderMode::Symbols,
        RenderMode::Ascii,
      ]
    );
  }

  #[test]
  fn render_modes_override_parses_aliases() {
    assert_eq!(
      parse_render_modes_override("TEST_RENDER_MODES", "auto"),
      None
    );
    assert_eq!(parse_render_modes_override("TEST_RENDER_MODES", "  "), None);
    assert_eq!(
      parse_render_modes_override("TEST_RENDER_MODES", "bogus"),
      None
    );
    assert_eq!(
      parse_render_modes_override("TEST_RENDER_MODES", "KGP, sixels:text"),
      Some(vec![
        RenderMode::Kitty,
        RenderMode::Sixel,
        RenderMode::Symbols,
        RenderMode::Ascii,
      ])
    );
  }
}
