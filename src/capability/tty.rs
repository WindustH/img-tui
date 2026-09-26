//! Unix terminal I/O for the active probe: raw mode, queries and replies.

use std::{
  fs::{File, OpenOptions},
  io::{ErrorKind, Read, Write},
  os::fd::{AsRawFd, RawFd},
  time::{Duration, Instant},
};

use tracing::warn;

use super::{
  multiplexer::Multiplexer,
  probe::{ProbeRequests, cell_size_reply, device_attribute_replies, find_subslice},
};

const KITTY_GRAPHICS_QUERY: &str = "\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\";
const REQUEST_XT_VERSION: &str = "\x1b[>q";
const REQUEST_CELL_PIXEL_SIZE: &str = "\x1b[16t";
const REQUEST_DA1: &str = "\x1b[0c";
const SAVE_CURSOR: &str = "\x1b[s";
const RESTORE_CURSOR: &str = "\x1b[u";
const PROBE_TIMEOUT: Duration = Duration::from_millis(800);
const PROBE_QUIET_TIMEOUT: Duration = Duration::from_millis(120);
const PROBE_POLL_INTERVAL: Duration = Duration::from_millis(30);

pub(super) fn query_terminal(
  multiplexer: Option<Multiplexer>,
  requests: ProbeRequests,
) -> Result<String, String> {
  let mut tty = open_tty().map_err(|err| format!("failed to open /dev/tty: {err}"))?;
  let _raw = RawModeGuard::enable(tty.as_raw_fd())
    .map_err(|err| format!("failed to enter raw mode for probe: {err}"))?;

  tty
    .write_all(probe_request(requests, multiplexer).as_bytes())
    .and_then(|_| tty.flush())
    .map_err(|err| format!("failed to write terminal probe: {err}"))?;

  let bytes = read_probe_responses(&mut tty, requests)
    .map_err(|err| format!("failed to read terminal probe response: {err}"))?;
  flush_terminal_input(tty.as_raw_fd());
  Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub(super) fn window_cell_pixels() -> Option<(u16, u16)> {
  let tty = open_tty().ok()?;
  let mut winsize = std::mem::MaybeUninit::<libc::winsize>::zeroed();
  if unsafe { libc::ioctl(tty.as_raw_fd(), libc::TIOCGWINSZ, winsize.as_mut_ptr()) } == -1 {
    return None;
  }

  let winsize = unsafe { winsize.assume_init() };
  if winsize.ws_col == 0 || winsize.ws_row == 0 || winsize.ws_xpixel == 0 || winsize.ws_ypixel == 0
  {
    return None;
  }

  Some((
    (winsize.ws_xpixel / winsize.ws_col).max(1),
    (winsize.ws_ypixel / winsize.ws_row).max(1),
  ))
}

fn open_tty() -> std::io::Result<File> {
  OpenOptions::new().read(true).write(true).open("/dev/tty")
}

fn probe_request(requests: ProbeRequests, multiplexer: Option<Multiplexer>) -> String {
  let mut request = String::from(SAVE_CURSOR);
  if requests.kitty_graphics {
    request.push_str(&wrap_for_multiplexer(KITTY_GRAPHICS_QUERY, multiplexer));
  }
  if requests.terminal_version {
    request.push_str(&wrap_for_multiplexer(REQUEST_XT_VERSION, multiplexer));
  }
  if requests.cell_pixels {
    request.push_str(REQUEST_CELL_PIXEL_SIZE);
  }
  // DA1 always goes last. Terminals answer queries in order and every
  // terminal answers DA1, so its reply ends the probe promptly even when the
  // terminal ignores the other queries. It is forwarded to the outer terminal
  // only when its attributes are used; otherwise whoever answered the
  // (unwrapped) cell size query answers it too.
  if requests.device_attributes {
    request.push_str(&wrap_for_multiplexer(REQUEST_DA1, multiplexer));
  } else {
    request.push_str(REQUEST_DA1);
  }
  request.push_str(RESTORE_CURSOR);
  request
}

fn wrap_for_multiplexer(sequence: &str, multiplexer: Option<Multiplexer>) -> String {
  if multiplexer != Some(Multiplexer::Tmux) {
    return sequence.to_string();
  }

  let escaped = sequence
    .trim_start_matches('\x1b')
    .replace('\x1b', "\x1b\x1b");
  format!("\x1bPtmux;\x1b\x1b{escaped}\x1b\\")
}

fn read_probe_responses(tty: &mut File, requests: ProbeRequests) -> std::io::Result<Vec<u8>> {
  let started = Instant::now();
  let mut last_byte_at = None;
  let mut buf = Vec::with_capacity(256);
  while started.elapsed() < PROBE_TIMEOUT {
    if probe_responses_complete(&buf, requests) {
      break;
    }
    let probe_went_quiet =
      last_byte_at.is_some_and(|last: Instant| last.elapsed() >= PROBE_QUIET_TIMEOUT);
    if !buf.is_empty() && probe_went_quiet {
      break;
    }

    let remaining = PROBE_TIMEOUT.saturating_sub(started.elapsed());
    if !poll_readable(tty.as_raw_fd(), remaining.min(PROBE_POLL_INTERVAL))? {
      continue;
    }

    let mut byte = [0_u8; 1];
    match tty.read(&mut byte) {
      Ok(0) => break,
      Ok(_) => {
        buf.push(byte[0]);
        last_byte_at = Some(Instant::now());
      }
      Err(err) if err.kind() == ErrorKind::Interrupted => continue,
      Err(err) => return Err(err),
    }
  }
  Ok(buf)
}

fn probe_responses_complete(buf: &[u8], requests: ProbeRequests) -> bool {
  (!requests.kitty_graphics || terminated_after(buf, b"\x1b_Gi=31", b"\x1b\\"))
    && (!requests.terminal_version || terminated_after(buf, b"\x1bP>|", b"\x1b\\"))
    && (!requests.cell_pixels || cell_size_reply(buf).is_some())
    && device_attribute_replies(buf).next().is_some()
}

fn terminated_after(buf: &[u8], start: &[u8], terminator: &[u8]) -> bool {
  let Some(start_index) = find_subslice(buf, start) else {
    return false;
  };
  find_subslice(&buf[start_index + start.len()..], terminator).is_some()
}

fn flush_terminal_input(fd: RawFd) {
  if unsafe { libc::tcflush(fd, libc::TCIFLUSH) } == -1 {
    warn!(
      error = %std::io::Error::last_os_error(),
      "failed to flush terminal input after probe"
    );
  }
}

fn poll_readable(fd: RawFd, timeout: Duration) -> std::io::Result<bool> {
  let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
  let mut pollfd = libc::pollfd {
    fd,
    events: libc::POLLIN,
    revents: 0,
  };
  loop {
    match unsafe { libc::poll(&mut pollfd, 1, timeout_ms) } {
      -1 => {
        let error = std::io::Error::last_os_error();
        if error.kind() == ErrorKind::Interrupted {
          continue;
        }
        return Err(error);
      }
      0 => return Ok(false),
      _ => return Ok((pollfd.revents & libc::POLLIN) != 0),
    }
  }
}

struct RawModeGuard {
  fd: RawFd,
  original: libc::termios,
}

impl RawModeGuard {
  fn enable(fd: RawFd) -> std::io::Result<Self> {
    let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } == -1 {
      return Err(std::io::Error::last_os_error());
    }
    let original = unsafe { original.assume_init() };
    let mut raw = original;
    unsafe { libc::cfmakeraw(&mut raw) };
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } == -1 {
      return Err(std::io::Error::last_os_error());
    }
    Ok(Self { fd, original })
  }
}

impl Drop for RawModeGuard {
  fn drop(&mut self) {
    if unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.original) } == -1 {
      warn!(
        error = %std::io::Error::last_os_error(),
        "failed to restore terminal mode after probe"
      );
    }
  }
}

#[cfg(test)]
mod tests {
  use super::{Multiplexer, ProbeRequests, probe_request, probe_responses_complete};

  const KNOWN_BRAND: ProbeRequests = ProbeRequests {
    kitty_graphics: false,
    terminal_version: false,
    cell_pixels: true,
    device_attributes: false,
  };

  #[test]
  fn device_attributes_query_is_always_last() {
    let request = probe_request(KNOWN_BRAND, None);
    assert_eq!(request, "\x1b[s\x1b[16t\x1b[0c\x1b[u");

    // Inside tmux the sentinel is answered by tmux itself, like the cell size
    // query, unless its attributes are needed from the outer terminal.
    assert_eq!(
      probe_request(KNOWN_BRAND, Some(Multiplexer::Tmux)),
      "\x1b[s\x1b[16t\x1b[0c\x1b[u"
    );
    let identify = ProbeRequests::plan(false, Some(Multiplexer::Tmux));
    assert!(
      probe_request(identify, Some(Multiplexer::Tmux))
        .ends_with("\x1bPtmux;\x1b\x1b[0c\x1b\\\x1b[u")
    );
  }

  #[test]
  fn probe_waits_for_device_attributes_sentinel() {
    // A terminal that ignores `CSI 16 t` still ends the probe with its DA1
    // reply instead of stalling until the timeout.
    assert!(!probe_responses_complete(b"", KNOWN_BRAND));
    assert!(!probe_responses_complete(b"\x1b[6;20;10t", KNOWN_BRAND));
    assert!(probe_responses_complete(
      b"\x1b[6;20;10t\x1b[?62;22c",
      KNOWN_BRAND
    ));

    let identify = ProbeRequests::plan(false, None);
    assert!(!probe_responses_complete(
      b"\x1bP>|foot(1.2)\x1b\\\x1b[6;20;10t\x1b[?62;4c",
      identify
    ));
    assert!(probe_responses_complete(
      b"\x1b_Gi=31;OK\x1b\\\x1bP>|kitty(0.40)\x1b\\\x1b[6;20;10t\x1b[?62;c",
      identify
    ));
  }
}
