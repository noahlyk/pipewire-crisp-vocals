//! crisp-vocals — the one installed binary of pipewire-crisp-vocals.
//!
//! One executable, two long-running processes (both started by
//! `pipewire-crisp-vocals.service` through its wrapper script):
//!
//!   - `crisp-vocals dsp`   the realtime JACK client that runs the mic
//!     through the DSP chain (`dsp` module);
//!   - `crisp-vocals links` the PipeWire registry client that wires the
//!     graph (`links` module).
//!
//! They stay separate processes on purpose: the DSP process only ever runs
//! the `dsp` module (it never initialises PipeWire's client library or
//! opens a registry connection), so nothing the wiring side does can stall
//! or crash the audio path, and either can be restarted or debugged alone.
//! Sharing an executable only means one file to install, one version and
//! one `--help`.
//!
//! `crisp-vocals mic ...` edits the config and `--version` also reports
//! whether the running processes are an older build (`crisp_config::daemon`).

mod dsp;
mod links;
mod mic;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "crisp-vocals",
    about = "PipeWire mic DSP chain and auto-wiring into one virtual mic",
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
    /// Print version (and whether the running service is an older build)
    // Not clap's built-in version flag: that prints one fixed string, and
    // this also reports a running service that is out of date.
    #[arg(short = 'V', long)]
    version: bool,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run the DSP chain as a JACK client (started by the service, via pw-jack)
    Dsp,
    /// Run the PipeWire auto-wiring daemon (started by the service)
    Links {
        /// Log every connect/disconnect/vmic-provision/synth-start decision
        /// instead of performing it. Safe alongside the running service.
        #[arg(long)]
        dry_run: bool,
    },
    /// Get/set hardware.mic_node_name in ~/.config/pipewire/crisp-vocals.ron
    Mic(mic::MicArgs),
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    if cli.version {
        crisp_config::daemon::print_version();
        return Ok(());
    }
    match cli.command {
        Some(Commands::Dsp) => dsp::run(),
        Some(Commands::Links { dry_run }) => {
            links::run(dry_run);
            Ok(())
        }
        Some(Commands::Mic(args)) => mic::run(args),
        // `arg_required_else_help` covers a bare `crisp-vocals`.
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("crisp-vocals").chain(args.iter().copied()))
    }

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn daemon_subcommands_parse() {
        assert!(matches!(parse(&["dsp"]).unwrap().command, Some(Commands::Dsp)));
        assert!(matches!(parse(&["links"]).unwrap().command, Some(Commands::Links { dry_run: false })));
        assert!(matches!(parse(&["links", "--dry-run"]).unwrap().command, Some(Commands::Links { dry_run: true })));
        // `--dry-run` belongs to `links` only.
        assert!(parse(&["dsp", "--dry-run"]).is_err());
    }

    #[test]
    fn mic_subcommand_parses_its_three_forms() {
        let Some(Commands::Mic(args)) = parse(&["mic", "USB Mic"]).unwrap().command else { panic!("not mic") };
        assert_eq!(args.node_name.as_deref(), Some("USB Mic"));
        let Some(Commands::Mic(args)) = parse(&["mic", "--auto"]).unwrap().command else { panic!("not mic") };
        assert!(args.auto && !args.list);
        let Some(Commands::Mic(args)) = parse(&["mic", "--list"]).unwrap().command else { panic!("not mic") };
        assert!(args.list && !args.auto);
        // The forms are exclusive, and a bare `mic` is a usage error.
        assert!(parse(&["mic"]).is_err());
        assert!(parse(&["mic", "--auto", "--list"]).is_err());
        assert!(parse(&["mic", "USB Mic", "--auto"]).is_err());
    }

    #[test]
    fn version_flag_has_both_spellings_and_needs_no_subcommand() {
        for flag in ["--version", "-V"] {
            let cli = parse(&[flag]).unwrap();
            assert!(cli.version && cli.command.is_none());
        }
    }

    #[test]
    fn bare_invocation_and_removed_names_do_not_start_anything() {
        // No arguments shows help instead of starting a daemon.
        assert!(parse(&[]).is_err());
        assert!(parse(&["crisp-links"]).is_err());
        assert!(parse(&["--dry-run"]).is_err());
    }
}
