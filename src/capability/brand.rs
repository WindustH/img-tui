//! Terminal identification from environment variables and probe replies.

use std::env;

use super::PixelProtocol;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TerminalBrand {
  Kitty,
  Konsole,
  Iterm2,
  WezTerm,
  Foot,
  Ghostty,
  Microsoft,
  Warp,
  Rio,
  BlackBox,
  VSCode,
  Tabby,
  Hyper,
  Mintty,
  Tmux,
  VTerm,
  Apple,
  Urxvt,
  Bobcat,
}

impl TerminalBrand {
  pub(super) fn label(self) -> &'static str {
    match self {
      Self::Kitty => "kitty",
      Self::Konsole => "konsole",
      Self::Iterm2 => "iterm2",
      Self::WezTerm => "wezterm",
      Self::Foot => "foot",
      Self::Ghostty => "ghostty",
      Self::Microsoft => "windows-terminal",
      Self::Warp => "warp",
      Self::Rio => "rio",
      Self::BlackBox => "blackbox",
      Self::VSCode => "vscode",
      Self::Tabby => "tabby",
      Self::Hyper => "hyper",
      Self::Mintty => "mintty",
      Self::Tmux => "tmux",
      Self::VTerm => "vterm",
      Self::Apple => "apple-terminal",
      Self::Urxvt => "urxvt",
      Self::Bobcat => "bobcat",
    }
  }

  /// Identify the terminal from its XTVERSION (or other) probe replies.
  pub(super) fn from_response(response: &str) -> Option<Self> {
    [
      ("kitty", Self::Kitty),
      ("Konsole", Self::Konsole),
      ("iTerm2", Self::Iterm2),
      ("WezTerm", Self::WezTerm),
      ("foot", Self::Foot),
      ("ghostty", Self::Ghostty),
      ("Warp", Self::Warp),
      ("Rio ", Self::Rio),
      ("tmux ", Self::Tmux),
      ("libvterm", Self::VTerm),
      ("Bobcat", Self::Bobcat),
    ]
    .into_iter()
    .find(|(needle, _)| response.contains(needle))
    .map(|(_, brand)| brand)
  }

  /// Identify the terminal from `TERM`, `TERM_PROGRAM` and terminal-specific
  /// environment variables.
  pub(super) fn from_env(term: Option<&str>, term_program: Option<&str>) -> Option<Self> {
    let term_lower = term.unwrap_or_default().to_ascii_lowercase();
    let program = term_program.unwrap_or_default();

    match term_lower.as_str() {
      "xterm-kitty" => return Some(Self::Kitty),
      "foot" | "foot-extra" => return Some(Self::Foot),
      "xterm-ghostty" => return Some(Self::Ghostty),
      "rio" => return Some(Self::Rio),
      "rxvt-unicode-256color" => return Some(Self::Urxvt),
      _ => {}
    }

    match program {
      "iTerm.app" => return Some(Self::Iterm2),
      "WezTerm" => return Some(Self::WezTerm),
      "WarpTerminal" => return Some(Self::Warp),
      "Apple_Terminal" => return Some(Self::Apple),
      _ => {}
    }
    match program.to_ascii_lowercase().as_str() {
      "ghostty" => return Some(Self::Ghostty),
      "rio" => return Some(Self::Rio),
      "blackbox" => return Some(Self::BlackBox),
      "vscode" => return Some(Self::VSCode),
      "tabby" => return Some(Self::Tabby),
      "hyper" => return Some(Self::Hyper),
      "mintty" => return Some(Self::Mintty),
      _ => {}
    }

    [
      ("KITTY_WINDOW_ID", Self::Kitty),
      ("KONSOLE_VERSION", Self::Konsole),
      ("ITERM_SESSION_ID", Self::Iterm2),
      ("WEZTERM_EXECUTABLE", Self::WezTerm),
      ("GHOSTTY_RESOURCES_DIR", Self::Ghostty),
      ("WT_SESSION", Self::Microsoft),
      ("WT_Session", Self::Microsoft),
      ("WARP_HONOR_PS1", Self::Warp),
      ("VSCODE_INJECTION", Self::VSCode),
      ("TABBY_CONFIG_DIRECTORY", Self::Tabby),
    ]
    .into_iter()
    .find(|(name, _)| env::var_os(name).is_some())
    .map(|(_, brand)| brand)
  }

  /// Pixel protocols this terminal is known to implement.
  pub(super) fn protocols(self) -> &'static [PixelProtocol] {
    match self {
      Self::Kitty | Self::Konsole | Self::Ghostty | Self::Rio => &[PixelProtocol::Kitty],
      Self::Iterm2 | Self::WezTerm | Self::VSCode | Self::Tabby | Self::Hyper | Self::Bobcat => {
        &[PixelProtocol::Iterm2, PixelProtocol::Sixel]
      }
      Self::Foot | Self::Microsoft | Self::BlackBox => &[PixelProtocol::Sixel],
      Self::Warp => &[PixelProtocol::Iterm2, PixelProtocol::Kitty],
      Self::Mintty => &[PixelProtocol::Iterm2],
      Self::Tmux | Self::VTerm | Self::Apple | Self::Urxvt => &[],
    }
  }
}

/// Protocols suggested by loose `TERM`/`TERM_PROGRAM` substrings and
/// terminal-specific variables, for terminals no brand rule matched exactly.
pub(super) fn environment_protocol_hints(
  term_lower: &str,
  program_lower: &str,
) -> impl Iterator<Item = PixelProtocol> {
  let kitty = env::var_os("KITTY_WINDOW_ID").is_some()
    || term_lower.contains("kitty")
    || program_lower.contains("ghostty")
    || program_lower.contains("rio");
  let iterm = program_lower.contains("iterm");
  let sixel = term_lower.contains("sixel")
    || program_lower.contains("foot")
    || program_lower.contains("wezterm")
    || env::var_os("MLTERM").is_some();
  [
    (kitty, PixelProtocol::Kitty),
    (iterm, PixelProtocol::Iterm2),
    (sixel, PixelProtocol::Sixel),
  ]
  .into_iter()
  .filter_map(|(hinted, protocol)| hinted.then_some(protocol))
}
