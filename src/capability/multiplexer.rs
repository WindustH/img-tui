//! Terminal multiplexer detection and setup (tmux, Zellij, GNU screen).

use std::{
  env,
  process::{Command, Stdio},
};

use tracing::warn;

use super::{PixelProtocol, TerminalProbe};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Multiplexer {
  Tmux,
  Zellij,
  Screen,
}

impl Multiplexer {
  pub(super) fn detect() -> Option<Self> {
    if env::var_os("ZELLIJ").is_some() || env::var_os("ZELLIJ_SESSION_NAME").is_some() {
      Some(Self::Zellij)
    } else if env::var_os("TMUX").is_some() {
      Some(Self::Tmux)
    } else if env::var_os("STY").is_some() {
      Some(Self::Screen)
    } else {
      None
    }
  }

  pub(super) fn label(self) -> &'static str {
    match self {
      Self::Tmux => "tmux",
      Self::Zellij => "zellij",
      Self::Screen => "screen",
    }
  }
}

/// Let the current tmux pane forward DCS passthrough sequences (tmux 3.3+).
pub(super) fn enable_tmux_passthrough() {
  match Command::new("tmux")
    .args(["set", "-p", "allow-passthrough", "on"])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .status()
  {
    Ok(status) if status.success() => {}
    Ok(status) => warn!(?status, "failed to enable tmux passthrough"),
    Err(error) => warn!(%error, "failed to run tmux passthrough setup"),
  }
}

/// `TERM` and `TERM_PROGRAM` of the terminal tmux is attached to, taken from
/// the tmux session environment. The pane's own variables describe tmux.
pub(super) fn tmux_outer_terminal() -> (Option<String>, Option<String>) {
  let Ok(output) = Command::new("tmux").arg("show-environment").output() else {
    return (None, None);
  };

  let mut term = None;
  let mut term_program = None;
  for line in String::from_utf8_lossy(&output.stdout).lines() {
    let Some((key, value)) = line.trim().split_once('=') else {
      continue;
    };
    match key {
      "TERM" => term = Some(value.to_string()),
      "TERM_PROGRAM" => term_program = Some(value.to_string()),
      _ => {}
    }
    if term.is_some() && term_program.is_some() {
      break;
    }
  }
  (term, term_program)
}

/// Drop protocols the multiplexer itself cannot carry.
pub(super) fn constrain_protocols(
  protocols: &mut Vec<PixelProtocol>,
  multiplexer: Option<Multiplexer>,
  probe: &TerminalProbe,
) {
  if multiplexer != Some(Multiplexer::Zellij) {
    return;
  }

  // Zellij exposes the outer terminal's environment to panes. Those hints do
  // not prove that the corresponding protocol is enabled in Zellij itself.
  protocols.retain(|protocol| match protocol {
    PixelProtocol::Kitty => probe.kitty_graphics,
    PixelProtocol::Sixel => probe.sixel,
    PixelProtocol::Iterm2 => false,
  });
}
