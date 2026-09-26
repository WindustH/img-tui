//! Active terminal probe: which queries to send and how to read the replies.

use tracing::{debug, warn};

use super::{TerminalProbe, brand::TerminalBrand, multiplexer::Multiplexer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ProbeRequests {
  /// Kitty graphics protocol query (`a=q`).
  pub(super) kitty_graphics: bool,
  /// XTVERSION, which names the terminal.
  pub(super) terminal_version: bool,
  /// `CSI 16 t`, the cell size in pixels.
  pub(super) cell_pixels: bool,
  /// Read sixel support from the primary device attributes (DA1) reply.
  ///
  /// DA1 is sent as the last query regardless, because its reply marks the
  /// end of the probe; this flag decides whether it is routed to the outer
  /// terminal and whether its attributes are trusted.
  pub(super) device_attributes: bool,
}

impl ProbeRequests {
  pub(super) fn plan(env_brand_known: bool, multiplexer: Option<Multiplexer>) -> Self {
    let in_zellij = multiplexer == Some(Multiplexer::Zellij);
    let needs_identity_probe = !env_brand_known && !in_zellij;
    Self {
      // Zellij 0.45+ implements KGP and answers the standard query only when
      // both its KGP support and the attached host terminal support are active.
      // Do not trust outer-terminal environment variables inside the mux.
      kitty_graphics: in_zellij || needs_identity_probe,
      terminal_version: needs_identity_probe,
      cell_pixels: true,
      device_attributes: !env_brand_known || in_zellij,
    }
  }
}

pub(super) struct ActiveProbe {
  pub(super) summary: TerminalProbe,
  pub(super) brand: Option<TerminalBrand>,
}

pub(super) fn probe_terminal(
  multiplexer: Option<Multiplexer>,
  requests: ProbeRequests,
) -> ActiveProbe {
  match query_terminal(multiplexer, requests) {
    Ok(response) => {
      let brand = TerminalBrand::from_response(&response);
      let summary = TerminalProbe {
        attempted: true,
        response_bytes: response.len(),
        kitty_graphics: supports_kitty_graphics(&response),
        sixel: requests.device_attributes && device_attributes_report_sixel(response.as_bytes()),
        cell_pixels: cell_pixels_reply(response.as_bytes()),
        brand: brand.map(|brand| brand.label().to_string()),
        error: None,
      };
      debug!(?summary, "terminal probe completed");
      ActiveProbe { summary, brand }
    }
    Err(error) => {
      warn!(error, "terminal probe failed");
      ActiveProbe {
        summary: TerminalProbe::skipped(error),
        brand: None,
      }
    }
  }
}

#[cfg(unix)]
use super::tty::query_terminal;

#[cfg(not(unix))]
fn query_terminal(
  _multiplexer: Option<Multiplexer>,
  _requests: ProbeRequests,
) -> Result<String, String> {
  Err("active terminal probing is only implemented on Unix".to_string())
}

/// Cell size derived from the window size in pixels, for terminals that do
/// not answer `CSI 16 t`.
#[cfg(unix)]
pub(super) use super::tty::window_cell_pixels;

#[cfg(not(unix))]
pub(super) fn window_cell_pixels() -> Option<(u16, u16)> {
  None
}

fn supports_kitty_graphics(response: &str) -> bool {
  response.contains("\x1b_Gi=31;OK")
}

/// Whether any DA1 reply (`CSI ? Ps ; ... c`) lists attribute 4 (sixel).
fn device_attributes_report_sixel(response: &[u8]) -> bool {
  device_attribute_replies(response).any(|params| {
    params
      .split(|byte| *byte == b';')
      .any(|param| param == b"4")
  })
}

/// Parameter bytes of each DA1 reply in `response`.
pub(super) fn device_attribute_replies(response: &[u8]) -> impl Iterator<Item = &[u8]> {
  let mut rest = response;
  std::iter::from_fn(move || {
    loop {
      let start = find_subslice(rest, b"\x1b[?")? + 3;
      let params_len = rest[start..]
        .iter()
        .take_while(|byte| byte.is_ascii_digit() || **byte == b';')
        .count();
      let end = start + params_len;
      let found = rest.get(end) == Some(&b'c');
      let params = &rest[start..end];
      rest = &rest[end..];
      if found {
        return Some(params);
      }
    }
  })
}

/// Cell size in pixels, `(width, height)`, from a `CSI 6 ; height ; width t`
/// reply. Terminals that do not know their pixel size may report zero, which
/// is treated as no answer so the window-size fallback can apply.
pub(super) fn cell_pixels_reply(response: &[u8]) -> Option<(u16, u16)> {
  cell_size_reply(response).filter(|(width, height)| *width > 0 && *height > 0)
}

/// Raw `CSI 6 ; height ; width t` reply as `(width, height)`, zeros included.
pub(super) fn cell_size_reply(response: &[u8]) -> Option<(u16, u16)> {
  let start = find_subslice(response, b"\x1b[6;")? + 4;
  let (height, rest) = split_number(&response[start..])?;
  let (width, rest) = split_number(rest.strip_prefix(b";")?)?;
  rest.starts_with(b"t").then_some((width, height))
}

fn split_number(bytes: &[u8]) -> Option<(u16, &[u8])> {
  let digits = bytes
    .iter()
    .take_while(|byte| byte.is_ascii_digit())
    .count();
  let value = std::str::from_utf8(&bytes[..digits]).ok()?.parse().ok()?;
  Some((value, &bytes[digits..]))
}

pub(super) fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
  if needle.is_empty() {
    return Some(0);
  }
  haystack
    .windows(needle.len())
    .position(|candidate| candidate == needle)
}

#[cfg(test)]
mod tests {
  use super::{cell_pixels_reply, cell_size_reply, device_attributes_report_sixel};

  #[test]
  fn cell_size_reply_parses_width_and_height() {
    assert_eq!(cell_size_reply(b"\x1b[6;20;10t"), Some((10, 20)));
    assert_eq!(cell_size_reply(b"junk\x1b[6;17;8tmore"), Some((8, 17)));
    assert_eq!(cell_size_reply(b"\x1b[6;20;10"), None);
    assert_eq!(cell_size_reply(b"\x1b[6;;10t"), None);
    assert_eq!(cell_size_reply(b"\x1b[6;99999;10t"), None);
  }

  #[test]
  fn zero_cell_size_reply_is_not_a_cell_size() {
    // Reported by terminals and multiplexers that do not know the pixel size
    // of the window. Accepting it would size every image at one pixel per
    // cell and suppress the TIOCGWINSZ fallback.
    assert_eq!(cell_size_reply(b"\x1b[6;0;0t"), Some((0, 0)));
    assert_eq!(cell_pixels_reply(b"\x1b[6;0;0t"), None);
    assert_eq!(cell_pixels_reply(b"\x1b[6;20;0t"), None);
    assert_eq!(cell_pixels_reply(b"\x1b[6;20;10t"), Some((10, 20)));
  }

  #[test]
  fn sixel_is_read_from_device_attributes_only() {
    assert!(device_attributes_report_sixel(b"\x1b[?62;4;22c"));
    assert!(device_attributes_report_sixel(b"\x1b[?4c"));
    assert!(device_attributes_report_sixel(b"\x1b[?1;2c\x1b[?65;4c"));
    assert!(!device_attributes_report_sixel(b"\x1b[?62;22c"));
    assert!(!device_attributes_report_sixel(b"\x1b[?64;44c"));
    // A 4-pixel-tall cell size reply is not a sixel attribute.
    assert!(!device_attributes_report_sixel(b"\x1b[6;4;8t\x1b[?1;2c"));
  }
}
