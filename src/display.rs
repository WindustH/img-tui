use std::io::Write;

use anyhow::Result;
use crossterm::{
  cursor::{Hide, MoveTo, RestorePosition, SavePosition, Show},
  queue,
};
use ratatui::{
  Frame, Terminal,
  backend::Backend,
  buffer::{Buffer, CellDiffOption},
  layout::Rect,
};

use crate::{ProtocolOverlay, ProtocolPlacement, RenderMode};

#[derive(Debug, Default)]
pub struct ProtocolOverlayRenderer {
  state: Vec<ProtocolOverlayState>,
}

#[derive(Debug, Default)]
pub struct ProtocolFrameRenderer {
  overlays: ProtocolOverlayRenderer,
}

#[derive(Debug, Clone, Default)]
pub struct ProtocolFrameOutput {
  pub overlays: Vec<ProtocolOverlay>,
  pub cursor_position: Option<(u16, u16)>,
}

#[derive(Debug)]
pub struct ProtocolOverlayCommit<'a> {
  next_state: Vec<ProtocolOverlayState>,
  added: Vec<&'a ProtocolOverlay>,
  clear_areas: Vec<Rect>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProtocolOverlayState {
  area: Rect,
  mode: RenderMode,
  placement: Option<ProtocolPlacement>,
  fingerprint: u64,
  erase: Option<String>,
}

impl ProtocolOverlayRenderer {
  pub fn begin<'a>(
    &self,
    writer: &mut impl Write,
    overlays: &'a [ProtocolOverlay],
  ) -> Result<ProtocolOverlayCommit<'a>> {
    let update = self.update(overlays);
    erase_protocol_state(writer, &update.removed)?;
    for area in &update.clear_areas {
      clear_protocol_area(writer, *area)?;
    }
    Ok(ProtocolOverlayCommit {
      next_state: update.next_state,
      added: update.added,
      clear_areas: update.clear_areas,
    })
  }

  pub fn finish(
    &mut self,
    writer: &mut impl Write,
    commit: ProtocolOverlayCommit<'_>,
  ) -> Result<()> {
    for overlay in commit.added {
      write_protocol_overlay(writer, overlay)?;
    }
    self.state = commit.next_state;
    Ok(())
  }

  pub fn clear(&mut self, writer: &mut impl Write) -> Result<()> {
    let old_state = std::mem::take(&mut self.state);
    erase_protocol_state(writer, &old_state)?;
    for overlay in old_state {
      clear_protocol_area(writer, overlay.area)?;
    }
    Ok(())
  }

  fn update<'a>(&self, overlays: &'a [ProtocolOverlay]) -> ProtocolOverlayUpdate<'a> {
    let next = overlays
      .iter()
      .map(|overlay| {
        (
          ProtocolOverlayState {
            area: overlay.area,
            mode: overlay.mode,
            placement: overlay.placement.clone(),
            fingerprint: overlay.fingerprint,
            erase: overlay.erase.clone(),
          },
          overlay,
        )
      })
      .collect::<Vec<_>>();
    let next_state = next
      .iter()
      .map(|(state, _)| state.clone())
      .collect::<Vec<_>>();

    if next_state == self.state {
      return ProtocolOverlayUpdate {
        next_state,
        removed: Vec::new(),
        added: Vec::new(),
        clear_areas: Vec::new(),
      };
    }

    let removed = self
      .state
      .iter()
      .filter(|state| !next_state.contains(state))
      .cloned()
      .collect::<Vec<_>>();
    let mut clear_areas = Vec::new();
    for removed_overlay in &removed {
      if !next
        .iter()
        .any(|(_, overlay)| rect_contains(overlay.area, removed_overlay.area))
      {
        clear_areas.push(removed_overlay.area);
      }
    }

    let added = if clear_areas.is_empty() {
      next
        .iter()
        .filter(|(state, _)| !self.state.contains(state))
        .map(|(_, overlay)| *overlay)
        .collect()
    } else {
      next.iter().map(|(_, overlay)| *overlay).collect()
    };

    ProtocolOverlayUpdate {
      next_state,
      removed,
      added,
      clear_areas,
    }
  }
}

impl ProtocolFrameRenderer {
  pub fn draw<B, F>(&mut self, terminal: &mut Terminal<B>, render: F) -> Result<()>
  where
    B: Backend + Write,
    B::Error: std::error::Error + Send + Sync + 'static,
    F: FnOnce(&mut Frame) -> ProtocolFrameOutput,
  {
    terminal.autoresize()?;

    let output = {
      let mut frame = terminal.get_frame();
      render(&mut frame)
    };

    let commit = {
      let backend = terminal.backend_mut();
      self.overlays.begin(backend, &output.overlays)?
    };
    force_update_areas(terminal.current_buffer_mut(), commit.clear_areas());

    terminal.flush()?;
    terminal.swap_buffers();

    {
      let backend = terminal.backend_mut();
      self.overlays.finish(backend, commit)?;
      queue_cursor_state(backend, output.cursor_position)?;
      Write::flush(backend)?;
    }

    Ok(())
  }

  pub fn clear(&mut self, writer: &mut impl Write) -> Result<()> {
    self.overlays.clear(writer)
  }

  pub fn clear_and_reset(
    &mut self,
    writer: &mut impl Write,
    reset_sequence: Option<&str>,
  ) -> Result<()> {
    self.clear(writer)?;
    reset_protocol_images(writer, reset_sequence)?;
    writer.flush()?;
    Ok(())
  }
}

impl ProtocolFrameOutput {
  pub fn new(overlays: Vec<ProtocolOverlay>, cursor_position: Option<(u16, u16)>) -> Self {
    Self {
      overlays,
      cursor_position,
    }
  }

  pub fn without_images(mut self) -> Self {
    self.overlays.clear();
    self
  }

  pub fn without_cursor(mut self) -> Self {
    self.cursor_position = None;
    self
  }
}

impl ProtocolOverlayCommit<'_> {
  pub fn clear_areas(&self) -> &[Rect] {
    &self.clear_areas
  }
}

pub fn force_update_areas(buffer: &mut Buffer, areas: &[Rect]) {
  if areas.is_empty() {
    return;
  }

  for area in areas {
    for y in area.y..area.y.saturating_add(area.height) {
      for x in area.x..area.x.saturating_add(area.width) {
        let Some(cell) = buffer.cell_mut((x, y)) else {
          continue;
        };
        if !matches!(cell.diff_option, CellDiffOption::Skip) {
          cell.set_diff_option(CellDiffOption::AlwaysUpdate);
        }
      }
    }
  }
}

pub fn reset_protocol_images(writer: &mut impl Write, sequence: Option<&str>) -> Result<()> {
  let Some(sequence) = sequence else {
    return Ok(());
  };
  queue!(writer, SavePosition)?;
  writer.write_all(sequence.as_bytes())?;
  queue!(writer, RestorePosition)?;
  writer.flush()?;
  Ok(())
}

struct ProtocolOverlayUpdate<'a> {
  next_state: Vec<ProtocolOverlayState>,
  removed: Vec<ProtocolOverlayState>,
  added: Vec<&'a ProtocolOverlay>,
  clear_areas: Vec<Rect>,
}

fn rect_contains(outer: Rect, inner: Rect) -> bool {
  let outer_right = outer.x.saturating_add(outer.width);
  let outer_bottom = outer.y.saturating_add(outer.height);
  let inner_right = inner.x.saturating_add(inner.width);
  let inner_bottom = inner.y.saturating_add(inner.height);
  outer.x <= inner.x
    && outer.y <= inner.y
    && outer_right >= inner_right
    && outer_bottom >= inner_bottom
}

fn erase_protocol_state(writer: &mut impl Write, state: &[ProtocolOverlayState]) -> Result<()> {
  if state.is_empty() {
    return Ok(());
  }

  queue!(writer, SavePosition)?;
  for overlay in state {
    if let Some(sequence) = &overlay.erase {
      writer.write_all(sequence.as_bytes())?;
    }
  }
  queue!(writer, RestorePosition)?;
  Ok(())
}

fn clear_protocol_area(writer: &mut impl Write, area: Rect) -> Result<()> {
  if area.width == 0 || area.height == 0 {
    return Ok(());
  }
  let blank = " ".repeat(area.width as usize);
  queue!(writer, SavePosition)?;
  for y in area.y..area.y.saturating_add(area.height) {
    queue!(writer, MoveTo(area.x, y))?;
    writer.write_all(blank.as_bytes())?;
  }
  queue!(writer, RestorePosition)?;
  Ok(())
}

fn write_protocol_overlay(writer: &mut impl Write, overlay: &ProtocolOverlay) -> Result<()> {
  clear_protocol_area(writer, overlay.area)?;
  queue!(writer, SavePosition)?;
  move_to_protocol_area(writer, overlay.area, is_tmux_passthrough(&overlay.data))?;
  writer.write_all(overlay.data.as_bytes())?;
  if let Some(ProtocolPlacement::KittyUnicode { image_id }) = &overlay.placement {
    write_kitty_unicode_placeholders(writer, overlay.area, *image_id)?;
  }
  writer.write_all(b"\x1b[0m")?;
  queue!(writer, RestorePosition)?;
  Ok(())
}

fn move_to_protocol_area(
  writer: &mut impl Write,
  area: Rect,
  tmux_passthrough: bool,
) -> Result<()> {
  queue!(writer, MoveTo(area.x, area.y))?;
  if tmux_passthrough {
    queue!(writer, MoveTo(area.x, area.y), MoveTo(area.x, area.y))?;
  }
  Ok(())
}

fn is_tmux_passthrough(data: &str) -> bool {
  data.starts_with("\x1bPtmux;")
}

fn write_kitty_unicode_placeholders(
  writer: &mut impl Write,
  area: Rect,
  image_id: u32,
) -> Result<()> {
  if area.width == 0 || area.height == 0 {
    return Ok(());
  }

  let red = (image_id >> 16) & 0xff;
  let green = (image_id >> 8) & 0xff;
  let blue = image_id & 0xff;
  write!(writer, "\x1b[0m\x1b[38;2;{red};{green};{blue}m")?;

  for y in 0..area.height {
    queue!(writer, MoveTo(area.x, area.y.saturating_add(y)))?;
    let row = kitty_placeholder_diacritic(y);
    for x in 0..area.width {
      write!(
        writer,
        "{}{}{}",
        KITTY_PLACEHOLDER,
        row,
        kitty_placeholder_diacritic(x)
      )?;
    }
  }

  Ok(())
}

fn queue_cursor_state(writer: &mut impl Write, cursor_position: Option<(u16, u16)>) -> Result<()> {
  match cursor_position {
    Some((x, y)) => queue!(writer, Show, MoveTo(x, y))?,
    None => queue!(writer, Hide)?,
  }
  Ok(())
}

const KITTY_PLACEHOLDER: char = '\u{10EEEE}';
const KITTY_DIACRITICS: &[char] = &[
  '\u{0305}', '\u{030D}', '\u{030E}', '\u{0310}', '\u{0312}', '\u{033D}', '\u{033E}', '\u{033F}',
  '\u{0346}', '\u{034A}', '\u{034B}', '\u{034C}', '\u{0350}', '\u{0351}', '\u{0352}', '\u{0357}',
  '\u{035B}', '\u{0363}', '\u{0364}', '\u{0365}', '\u{0366}', '\u{0367}', '\u{0368}', '\u{0369}',
  '\u{036A}', '\u{036B}', '\u{036C}', '\u{036D}', '\u{036E}', '\u{036F}', '\u{0483}', '\u{0484}',
  '\u{0485}', '\u{0486}', '\u{0487}', '\u{0592}', '\u{0593}', '\u{0594}', '\u{0595}', '\u{0597}',
  '\u{0598}', '\u{0599}', '\u{059C}', '\u{059D}', '\u{059E}', '\u{059F}', '\u{05A0}', '\u{05A1}',
  '\u{05A8}', '\u{05A9}', '\u{05AB}', '\u{05AC}', '\u{05AF}', '\u{05C4}', '\u{0610}', '\u{0611}',
  '\u{0612}', '\u{0613}', '\u{0614}', '\u{0615}', '\u{0616}', '\u{0617}', '\u{0618}', '\u{0619}',
  '\u{061A}', '\u{064B}', '\u{064C}', '\u{064D}', '\u{064E}', '\u{064F}', '\u{0650}', '\u{0651}',
  '\u{0652}', '\u{0653}', '\u{0654}', '\u{0655}', '\u{0656}', '\u{0657}', '\u{0658}', '\u{0659}',
  '\u{065A}', '\u{065B}', '\u{065C}', '\u{065D}', '\u{065E}', '\u{06D6}', '\u{06D7}', '\u{06D8}',
  '\u{06D9}', '\u{06DA}', '\u{06DB}', '\u{06DC}', '\u{06DF}', '\u{06E0}', '\u{06E1}', '\u{06E2}',
  '\u{06E3}', '\u{06E4}', '\u{06E7}', '\u{06E8}', '\u{06EA}', '\u{06EB}', '\u{06EC}', '\u{0730}',
  '\u{0731}', '\u{0732}', '\u{0733}', '\u{0734}', '\u{0735}', '\u{0736}', '\u{0737}', '\u{0738}',
  '\u{0739}', '\u{073A}', '\u{073B}', '\u{073C}', '\u{073D}', '\u{073E}', '\u{073F}', '\u{0740}',
  '\u{0741}', '\u{0742}', '\u{0743}', '\u{0744}', '\u{0745}', '\u{0746}', '\u{0747}', '\u{0748}',
  '\u{0749}', '\u{074A}', '\u{07EB}', '\u{07EC}', '\u{07ED}', '\u{07EE}', '\u{07EF}', '\u{07F0}',
  '\u{07F1}', '\u{07F2}', '\u{07F3}', '\u{0816}', '\u{0817}', '\u{0818}', '\u{0819}', '\u{081B}',
  '\u{081C}', '\u{081D}', '\u{081E}', '\u{081F}', '\u{0820}', '\u{0821}', '\u{0822}', '\u{0823}',
  '\u{0825}', '\u{0826}', '\u{0827}', '\u{0829}', '\u{082A}', '\u{082B}', '\u{082C}', '\u{082D}',
  '\u{0859}', '\u{085A}', '\u{085B}', '\u{08D4}', '\u{08D5}', '\u{08D6}', '\u{08D7}', '\u{08D8}',
  '\u{08D9}', '\u{08DA}', '\u{08DB}', '\u{08DC}', '\u{08DD}', '\u{08DE}', '\u{08DF}', '\u{08E0}',
  '\u{08E1}', '\u{08E3}', '\u{08E4}', '\u{08E5}', '\u{08E6}', '\u{08E7}', '\u{08E8}', '\u{08E9}',
  '\u{08EA}', '\u{08EB}', '\u{08EC}', '\u{08ED}', '\u{08EE}', '\u{08EF}', '\u{08F0}', '\u{08F1}',
  '\u{08F2}', '\u{08F3}', '\u{08F4}', '\u{08F5}', '\u{08F6}', '\u{08F7}', '\u{08F8}', '\u{08F9}',
  '\u{08FA}', '\u{08FB}', '\u{08FC}', '\u{08FD}', '\u{08FE}', '\u{08FF}',
];

fn kitty_placeholder_diacritic(index: u16) -> char {
  KITTY_DIACRITICS[index as usize % KITTY_DIACRITICS.len()]
}
