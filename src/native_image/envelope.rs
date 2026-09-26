//! Multiplexer passthrough wrapping for image escape sequences.

/// Pieces that wrap one escape sequence for the configured passthrough.
///
/// A sequence `ESC <body> ESC \` is written as
/// `{start}<body>{escape}\{close}`; `start` replaces the leading `ESC` and
/// `escape` the `ESC` of the string terminator. Inside tmux both are doubled
/// and the whole sequence is wrapped in `DCS tmux; ... ST`.
#[derive(Debug, Clone, Copy)]
pub(super) struct ProtocolEnvelope {
  pub(super) start: &'static str,
  pub(super) escape: &'static str,
  pub(super) close: &'static str,
}

impl ProtocolEnvelope {
  pub(super) fn new(passthrough: Option<&str>) -> Self {
    match passthrough {
      Some("tmux") => Self {
        start: "\x1bPtmux;\x1b\x1b",
        escape: "\x1b\x1b",
        close: "\x1b\\",
      },
      Some("screen") => Self {
        start: "\x1bP\x1b",
        escape: "\x1b",
        close: "\x1b\\",
      },
      _ => Self {
        start: "\x1b",
        escape: "\x1b",
        close: "",
      },
    }
  }

  /// Wrap a kitty graphics command (`ESC _G<control> ESC \`).
  pub(super) fn kitty_command(self, control: std::fmt::Arguments<'_>) -> String {
    format!("{}_G{control}{}\\{}", self.start, self.escape, self.close)
  }
}
