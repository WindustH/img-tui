//! Shared Chafa text fallback, with application configuration supplied explicitly.
use std::path::Path;

use crate::RenderMode;
use tokio::process::Command;

/// Application-supplied text-renderer settings.
pub struct ChafaConfig<'a> {
  pub chafa_bin: &'a str,
  pub chafa_args: &'a [String],
  pub chafa_threads: usize,
}

pub async fn run(
  image_path: &Path,
  width: u16,
  height: u16,
  config: &ChafaConfig<'_>,
  mode: RenderMode,
) -> Result<Vec<u8>, String> {
  let mut command = chafa_command(width, height, config, mode)?;
  command.arg(image_path);

  let chafa_bin = config.chafa_bin;
  let output = command
    .output()
    .await
    .map_err(|error| format!("failed to run {chafa_bin}: {error}"))?;
  check_chafa_output(output, config.chafa_bin)
}

fn chafa_command(
  width: u16,
  height: u16,
  config: &ChafaConfig<'_>,
  mode: RenderMode,
) -> Result<Command, String> {
  if mode.is_protocol() {
    return Err(format!(
      "{} must be rendered by native image driver, not chafa",
      mode.label()
    ));
  }

  let mut command = Command::new(config.chafa_bin);
  let mut args: Vec<String> = config
    .chafa_args
    .iter()
    .filter(|arg| {
      !arg.starts_with("--format=")
        && !arg.starts_with("--colors=")
        && !arg.starts_with("--symbols=")
        && !arg.starts_with("--passthrough=")
        && !arg.starts_with("--probe=")
        && !arg.starts_with("--relative=")
    })
    .cloned()
    .collect();

  args.push(format!("--format={}", mode.chafa_format()));
  args.push("--probe=off".to_string());
  args.push("--relative=off".to_string());
  args.push("--passthrough=none".to_string());
  if !args.iter().any(|arg| arg.starts_with("--scale=")) {
    args.push("--scale=max".to_string());
  }
  if config.chafa_threads > 0
    && !config
      .chafa_args
      .iter()
      .any(|arg| arg.starts_with("--threads="))
  {
    args.push(format!("--threads={}", config.chafa_threads));
  }
  match mode {
    RenderMode::Symbols => {
      for arg in config.chafa_args {
        if arg.starts_with("--colors=") || arg.starts_with("--symbols=") {
          args.push(arg.clone());
        }
      }
    }
    RenderMode::Ascii => {
      args.push("--colors=none".to_string());
      args.push("--symbols=ascii".to_string());
    }
    _ => {}
  }

  command
    .args(args)
    .arg("--size")
    .arg(format!("{width}x{height}"));

  Ok(command)
}

fn check_chafa_output(output: std::process::Output, chafa_bin: &str) -> Result<Vec<u8>, String> {
  if !output.status.success() {
    let stderr = String::from_utf8_lossy(&output.stderr);
    return Err(format!(
      "{chafa_bin} exited with {}: {}",
      output.status,
      stderr.trim()
    ));
  }
  Ok(output.stdout)
}

/// Decode ANSI text output into owned ratatui text.
pub fn decode(bytes: &[u8]) -> Result<ratatui::text::Text<'static>, String> {
  use ansi_to_tui::IntoText;
  bytes.into_text().map_err(|error| error.to_string())
}
