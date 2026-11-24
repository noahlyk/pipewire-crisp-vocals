//! `crisp-vocals mic ...` — get/set `hardware.mic_node_name` in the config.
//!
//! CLI-only config editing: no daemon and no PipeWire client involved (the
//! lookups shell out to `wpctl`/`pactl` through `crisp-config`).

use clap::Args;

/// Exactly one of the three forms; clap rejects a mix of them.
#[derive(Args, Debug)]
#[group(required = true, multiple = false)]
pub struct MicArgs {
    /// Set hardware.mic_node_name to this exact PipeWire node name.
    pub(crate) node_name: Option<String>,
    /// Re-run auto-detection (same logic as first-run bootstrap) and set
    /// hardware.mic_node_name to the result.
    #[arg(long)]
    pub(crate) auto: bool,
    /// List current PipeWire audio source nodes instead of setting anything.
    #[arg(long)]
    pub(crate) list: bool,
}

/// Run the command and exit the process (success or failure).
pub fn run(args: MicArgs) -> ! {
    if let Err(e) = crisp_config::bootstrap_if_missing() {
        eprintln!("[crisp-vocals mic] config bootstrap failed: {e}");
    }

    if args.list {
        match crisp_config::list_audio_sources() {
            Ok(sources) if sources.is_empty() => {
                eprintln!("[crisp-vocals mic] no audio sources found (is wpctl/PipeWire running?)");
                std::process::exit(1);
            }
            Ok(sources) => {
                for s in sources {
                    println!("{s}");
                }
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("[crisp-vocals mic] failed to list audio sources: {e}");
                std::process::exit(1);
            }
        }
    }

    let mic = if args.auto {
        match crisp_config::detect_default_mic() {
            Some(m) => m,
            None => {
                eprintln!("[crisp-vocals mic] could not auto-detect the default audio source");
                std::process::exit(1);
            }
        }
    } else if let Some(name) = args.node_name {
        name
    } else {
        eprintln!("usage: crisp-vocals mic <node-name> | crisp-vocals mic --auto | crisp-vocals mic --list");
        std::process::exit(2);
    };

    let path = crisp_config::config_path();
    match crisp_config::update_mic_node_name(&path, &mic) {
        Ok(()) => {
            println!("set hardware.mic_node_name = \"{mic}\" in {}", path.display());
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("[crisp-vocals mic] failed to update {}: {e}", path.display());
            std::process::exit(1);
        }
    }
}
