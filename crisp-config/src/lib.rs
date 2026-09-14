//! crisp-config — shared config-bootstrap and mic-detection logic used by
//! both `crisp-vocals` and `crisp-links`, so neither duplicates it.
//!
//! Responsibilities:
//!   - resolving the one shared `crisp-vocals.ron` path (same rule both
//!     binaries used to implement separately)
//!   - first-run bootstrap: copy the packaged example config into place and
//!     fill in `hardware.mic_node_name` from the system's current default
//!     audio source, replacing the old `first-run-setup.sh` +
//!     `crisp-vocals-setup.service` one-shot unit
//!   - a targeted in-place text edit of the `mic_node_name` line, shared by
//!     the bootstrap path and `crisp-links mic <node-name>`/`--auto`
//!   - `wpctl status` parsing for `crisp-links mic --list`

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where the package ships the example config (see PKGBUILD/aur/PKGBUILD
/// `package()`). Bootstrapped from here into `config_path()` on first run.
pub const EXAMPLE_CONF_PATH: &str = "/usr/share/pipewire-crisp-vocals/crisp-vocals.ron.example";

/// Same resolution order `crisp-vocals` and `crisp-links` each used to
/// implement independently: `$CRISP_VOCALS_CONF`, else
/// `$XDG_CONFIG_HOME/pipewire/crisp-vocals.ron`, else
/// `~/.config/pipewire/crisp-vocals.ron`.
pub fn config_path() -> PathBuf {
    let dir = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").expect("HOME unset")).join(".config"))
        .join("pipewire");
    std::env::var("CRISP_VOCALS_CONF").map(PathBuf::from).unwrap_or_else(|_| dir.join("crisp-vocals.ron"))
}

/// Ensure `config_path()` exists, copying the packaged example config into
/// place (with `hardware.mic_node_name` filled in from the current default
/// audio source, if one can be detected) the first time either binary runs
/// on a machine. Never overwrites an existing config.
///
/// Both `crisp-vocals` and `crisp-links` call this at startup (replacing
/// the old separate `crisp-vocals-setup.service` unit), so it has to be
/// race-safe against the two binaries starting at roughly the same time
/// (e.g. both launched by the new single systemd unit's wrapper script).
/// The race is handled by opening the destination with `create_new` --
/// atomic "create iff it doesn't exist yet" at the OS level -- so whichever
/// binary gets there first wins and the other's `AlreadyExists` is treated
/// as success rather than an error. This is a one-time, small (a few KB)
/// text file write; a half-written file being observed by the loser is not
/// a real concern in practice (the winner's single `write_all` completes
/// before the loser's `create_new` could even be attempted after losing the
/// race), so this skips a temp-file-plus-rename dance for simplicity.
pub fn bootstrap_if_missing() -> io::Result<()> {
    let path = config_path();
    if path.exists() {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }

    let mut content = fs::read_to_string(EXAMPLE_CONF_PATH)?;
    match detect_default_mic() {
        Some(mic) => {
            content = set_mic_node_name_in_text(&content, &mic);
            eprintln!("[crisp-config] bootstrapping {} (mic_node_name = \"{mic}\")", path.display());
        }
        None => {
            eprintln!(
                "[crisp-config] bootstrapping {} -- could not determine the default audio source; \
                 edit hardware.mic_node_name by hand.",
                path.display()
            );
        }
    }

    match fs::OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut f) => f.write_all(content.as_bytes()),
        // Lost the race to the other binary's own bootstrap call -- the
        // config now exists either way, which is all this function promises.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// Detect the system's current default audio source's PipeWire node name.
/// Prefers `wpctl inspect @DEFAULT_AUDIO_SOURCE@` (PipeWire/WirePlumber
/// native); falls back to `pactl get-default-source` + `pactl list sources`
/// if `wpctl` is unavailable or doesn't resolve one. Ported from the old
/// `scripts/first-run-setup.sh`.
pub fn detect_default_mic() -> Option<String> {
    if let Some(name) = detect_via_wpctl() {
        return Some(name);
    }
    detect_via_pactl()
}

fn detect_via_wpctl() -> Option<String> {
    let out = Command::new("wpctl").args(["inspect", "@DEFAULT_AUDIO_SOURCE@"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().find_map(|line| if line.contains("node.name") { extract_quoted(line) } else { None })
}

fn detect_via_pactl() -> Option<String> {
    let out = Command::new("pactl").arg("get-default-source").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let default_source = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if default_source.is_empty() {
        return None;
    }

    let list = Command::new("pactl").args(["list", "sources"]).output().ok()?;
    let text = String::from_utf8_lossy(&list.stdout);
    let mut current_name: Option<String> = None;
    for line in text.lines() {
        if line.starts_with("Source #") {
            current_name = None;
        } else if let Some(rest) = line.trim_start().strip_prefix("Name: ") {
            current_name = Some(rest.trim().to_string());
        } else if current_name.as_deref() == Some(default_source.as_str()) && line.contains("node.description") {
            if let Some(desc) = extract_after_eq_quoted(line) {
                return Some(desc);
            }
        }
    }
    // Fall back to the raw source name if the description lookup above
    // didn't find anything usable.
    Some(default_source)
}

/// Pull the first `"..."` quoted string out of a line, e.g.
/// `        node.name = "alsa_input.usb-..."` -> `alsa_input.usb-...`.
fn extract_quoted(line: &str) -> Option<String> {
    let start = line.find('"')? + 1;
    let end = start + line[start..].find('"')?;
    Some(line[start..end].to_string())
}

/// Pull the quoted string after an `=` sign, e.g.
/// `                node.description = "Foo Mic"` -> `Foo Mic`.
fn extract_after_eq_quoted(line: &str) -> Option<String> {
    let after_eq = line.split_once('=')?.1;
    extract_quoted(after_eq)
}

/// Targeted in-place text edit of the `hardware.mic_node_name` line -- NOT a
/// full RON parse+reserialize, which would destroy the hand-written comments
/// in the shipped example config (e.g. `// active_mode: "raw"`). If a
/// `mic_node_name:` line already exists, its value is replaced in place,
/// preserving indentation; otherwise a minimal `hardware: (mic_node_name:
/// "...")` block is appended, matching the shape used in the example
/// config's `hardware:` section.
pub fn set_mic_node_name_in_text(text: &str, mic: &str) -> String {
    let escaped = mic.replace('\\', "\\\\").replace('"', "\\\"");
    if text.contains("mic_node_name:") {
        let mut out = String::with_capacity(text.len() + 32);
        for line in text.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("mic_node_name:") {
                let indent = &line[..line.len() - trimmed.len()];
                out.push_str(indent);
                out.push_str("mic_node_name: \"");
                out.push_str(&escaped);
                out.push_str("\",");
            } else {
                out.push_str(line);
            }
            out.push('\n');
        }
        out
    } else {
        let mut out = text.to_string();
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("hardware: (mic_node_name: \"{escaped}\"),\n"));
        out
    }
}

/// Apply `set_mic_node_name_in_text` to the config file on disk. Used by
/// `crisp-links mic <node-name>` / `crisp-links mic --auto`.
pub fn update_mic_node_name(path: &Path, mic: &str) -> io::Result<()> {
    let text = fs::read_to_string(path)?;
    let new_text = set_mic_node_name_in_text(&text, mic);
    fs::write(path, new_text)
}

/// List current PipeWire audio source nodes (display names), for
/// `crisp-links mic --list`. Parses `wpctl status`'s `Sources:` section --
/// simpler than standing up a full PipeWire registry client just to
/// enumerate nodes for a one-shot CLI command.
pub fn list_audio_sources() -> io::Result<Vec<String>> {
    let out = Command::new("wpctl").arg("status").output()?;
    let text = String::from_utf8_lossy(&out.stdout);

    let mut in_sources = false;
    let mut sources = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.ends_with("Sources:") {
            in_sources = true;
            continue;
        }
        if !in_sources {
            continue;
        }
        // A new top-level section (e.g. "Filters:", "Streams:") ends the
        // Sources section -- it's a header line with no entry id/asterisk.
        if trimmed.ends_with(':') && !trimmed.contains('.') {
            break;
        }
        if trimmed.is_empty() {
            continue;
        }
        let cleaned = trimmed.trim_start_matches(['│', '├', '└', '─', ' ', '*']);
        if let Some(dot) = cleaned.find(". ") {
            let name = cleaned[dot + 2..].trim();
            let name = name.split(" [").next().unwrap_or(name).trim();
            if !name.is_empty() {
                sources.push(name.to_string());
            }
        }
    }
    Ok(sources)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_existing_mic_node_name_line_preserving_indent_and_comments() {
        let text = "(\n    // active_mode: \"raw\",\n    hardware: (\n        mic_node_name: \"\",\n    ),\n)\n";
        let out = set_mic_node_name_in_text(text, "USB Mic");
        assert!(out.contains("        mic_node_name: \"USB Mic\","));
        assert!(out.contains("// active_mode: \"raw\","), "comment must survive untouched");
    }

    #[test]
    fn inserts_hardware_block_when_missing() {
        let text = "(\n    preamp_db: 0.0,\n)\n";
        let out = set_mic_node_name_in_text(text, "USB Mic");
        assert!(out.contains("hardware: (mic_node_name: \"USB Mic\"),"));
    }

    #[test]
    fn escapes_quotes_and_backslashes_in_mic_name() {
        let text = "mic_node_name: \"\",\n";
        let out = set_mic_node_name_in_text(text, "weird\"name\\here");
        assert!(out.contains("mic_node_name: \"weird\\\"name\\\\here\","));
    }

    #[test]
    fn extract_quoted_pulls_first_quoted_string() {
        let line = "        node.name = \"alsa_input.usb-Foo\"";
        assert_eq!(extract_quoted(line).as_deref(), Some("alsa_input.usb-Foo"));
    }
}
