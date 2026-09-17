//! crisp-links — declarative audio/MIDI wiring for the crisp-vocals stack.
//!
//! One plain 2-channel filter-chain node (99-crisp-vocals.conf), a trivial
//! passthrough -- there is no internal mixing anywhere in the filter-chain
//! config; every mix below is built by wiring multiple sources into the
//! same destination port and letting PipeWire sum them there natively:
//!
//!   virtual-input      -- "anything connected" (e.g. the synth) lands here. Its
//!                  automatic monitor_FL/FR (every Sink gets this for free,
//!                  mirroring whatever was fed in) is fanned by this file
//!                  into `virtual-mic`'s input.
//!   virtual-mic -- virtual-input's monitor + crisp-vocals' `out_L`/`out_R`,
//!                  summed at virtual-mic's own input. This is BOTH the
//!                  mic device apps (Discord/OBS/...) select AND, via its
//!                  own monitor tap, the user's self-monitor mix routed to
//!                  the real speaker device.
//!
//! Also:
//!   <hardware.mic_node_name> -> crisp-vocals (raw input, exclusive)
//!   apps -> the default speaker device (WirePlumber's normal routing; the
//!     mic arrives there through virtual-mic's monitor self-monitor tap,
//!     above)
//!   <synth.midi_keyboard_name> MIDI -> fluidsynth (on-demand) -> virtual-input
//!     (audible to you via virtual-mic's self-monitor tap AND to listeners
//!     via virtual-mic's playback input) -- only when `synth.enabled` in
//!     `crisp-vocals.ron`.
//!
//! The routing is described by the `routes()` table, the self-monitor rule
//! and the synth rule.
//!
//! Purely event-driven: this is a persistent PipeWire client (via the
//! `pipewire` crate) subscribed to the registry (node/port/link add+remove)
//! and to the "default" metadata object (default sink changes) -- no
//! polling loop, no `pw-cli`/`pw-link`/`pactl` subprocess spawns for the
//! routine path. A burst of registry events (e.g. one app launching, or
//! startup enumeration) is coalesced by a short debounce timer into a
//! single route-application pass instead of one per event. Links are
//! created/destroyed natively (`Core::create_object`/`Registry::destroy_global`)
//! rather than by shelling out to `pw-link`.
//!
//! `--dry-run`: logs every connect/disconnect/vmic-provision/synth-start
//! decision instead of performing it -- fully read-only against the live
//! graph, safe to run alongside the real service for diagnosis.

use clap::{Args, Parser, Subcommand};
use pipewire as pw;
use pw::loop_::Signal;
use pw::properties::properties;
use pw::registry::GlobalObject;
use pw::spa::utils::dict::DictRef;
use pw::types::ObjectType;
use serde::Deserialize;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

// ────────────────────────────────────────────────────────────────────
// CONFIG — `hardware`/`synth`/`linking` tables in `crisp-vocals.ron`
// (shared with crisp-vocals, RON not TOML; this crate only reads those
// three fields out of it). Field names are used as-is (snake_case),
// matching the structs below.
// ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize)]
struct RootConf {
    #[serde(default)]
    linking: LinkingConf,
    #[serde(default)]
    hardware: HardwareConf,
    /// Absent/`enabled: false` -- the synth fan-in is skipped entirely.
    synth: Option<SynthConf>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
struct LinkingConf {
    /// Master switch: false disables all linking activity (no connects, no
    /// disconnects, no virtual-sink provisioning, no synth lifecycle).
    #[serde(default = "default_true")]
    enabled: bool,
    /// true (default): wire each node up once, right as it (and its route
    /// peers) first appear, then leave its links alone -- manual relinking
    /// (pw-link/qpwgraph) afterward is never fought. false: keep enforcing
    /// the routing table on every registry event, forever (old behavior).
    #[serde(default = "default_true")]
    only_edit_links_on_node_init: bool,
    /// false (default): virtual-mic's own mixed output never gets tapped
    /// into your default speakers -- you don't hear yourself. true: opt in
    /// to routing virtual-mic's monitor into the default output device (the
    /// "self-monitor" tap), e.g. so a MIDI-keyboard synth fanned into
    /// virtual-input is audible to you.
    #[serde(default)]
    monitor_through_default_output: bool,
}

impl Default for LinkingConf {
    fn default() -> Self {
        LinkingConf { enabled: true, only_edit_links_on_node_init: true, monitor_through_default_output: false }
    }
}

/// This machine's hardware wiring -- previously hardcoded, now configurable
/// so the same binary works on any machine.
#[derive(Debug, Clone, Default, Deserialize)]
struct HardwareConf {
    /// Physical mic node's PipeWire `node.name` (substring match), e.g.
    /// "Komplete". Blank (the shipped default) means "no physical mic
    /// routed yet" -- `crisp_config::bootstrap_if_missing()` fills this in
    /// from the system's current default audio source on first run; see
    /// also `crisp-links mic --auto`.
    #[serde(default)]
    mic_node_name: String,
}

/// Optional MIDI-keyboard-triggered synth fan-in (`Oxygen 49` -> fluidsynth
/// -> virtual-input in the original single-machine setup). Entirely optional --
/// `synth` absent, or `enabled: false`, skips this whole code path.
#[derive(Debug, Clone, Default, Deserialize)]
struct SynthConf {
    #[serde(default)]
    enabled: bool,
    /// Path to a SoundFont (.sf2) file fluidsynth loads.
    #[serde(default)]
    soundfont_path: String,
    /// MIDI keyboard's PipeWire node name (substring match).
    #[serde(default)]
    midi_keyboard_name: String,
}

fn default_true() -> bool {
    true
}

/// Same resolution order as crisp-vocals' `config_path()`, so both daemons
/// agree on the one `crisp-vocals.ron` without either hardcoding the
/// other's path: `$CRISP_VOCALS_CONF`, else
/// `$XDG_CONFIG_HOME/pipewire/crisp-vocals.ron`, else
/// `~/.config/pipewire/crisp-vocals.ron`.
fn config_path() -> PathBuf {
    crisp_config::config_path()
}

/// Loaded once at startup (unlike crisp-vocals, this crate has no hot
/// reload) -- missing file or bad tables fall back to defaults rather than
/// refusing to start, since the rest of `crisp-vocals.ron` (modes, stages)
/// isn't this crate's business.
fn load_conf() -> RootConf {
    let path = config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[crisp-links] cannot read {}: {e}; using default settings", path.display());
            return RootConf::default();
        }
    };
    let opts = ron::Options::default().with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME);
    match opts.from_str::<RootConf>(&text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[crisp-links] bad config {}: {e}; using default settings", path.display());
            RootConf::default()
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// TIMING
// ────────────────────────────────────────────────────────────────────

/// How long to wait after the LAST registry/metadata event before actually
/// applying routes. Coalesces a burst (e.g. ~125 objects announced at
/// startup, or several events from one app launching) into one pass instead
/// of one per event.
const DEBOUNCE: Duration = Duration::from_millis(50);

/// How often to stat `crisp-vocals.ron` for config hot-reload. Routing
/// itself is purely event-driven, but nothing else generates a PipeWire
/// registry/metadata event when the file is just edited on disk.
const CONF_POLL: Duration = Duration::from_secs(1);

/// A just-issued link creation is considered "in flight" (and won't be
/// re-attempted) for this long, covering the round-trip before the server's
/// own Link-added event confirms it in `Manager::links`. If creation
/// silently failed for some reason, the route becomes eligible for a fresh
/// attempt again after this window -- a small self-healing fallback, not a
/// real retry loop (this version only ever attempts a route once the ports
/// it needs already exist in the registry).
const PENDING_LINK_TTL: Duration = Duration::from_secs(2);

// ────────────────────────────────────────────────────────────────────
// NODE NAMES
// ────────────────────────────────────────────────────────────────────

const NAME_CRISP_VOCALS: &str = "crisp-vocals";
/// "Anything connected" lands here, fanned out to `virtual-mic`.
const NAME_VIRTUAL_INPUT: &str = "virtual-input";
/// The single virtual mic device -- both what apps select AND the
/// self-monitor tap.
const NAME_VIRTUAL_MIC: &str = "virtual-mic";
// Matches the synth's single JACK node ("fluidsynth-midi": MIDI-in port +
// audio-out ports on one node), as well as the old PulseAudio layout
// ("FluidSynth" audio + "FLUID Synth (pid)" MIDI).
const NAME_SYNTH: &str = "Synth";

// ────────────────────────────────────────────────────────────────────
// DEVICES & LANES — the routing vocabulary.
// ────────────────────────────────────────────────────────────────────

/// A set of ports on a PipeWire node. `node` matches the node name by
/// substring; `port` additionally narrows to ports whose own name contains
/// that substring (`""` = every port).
#[derive(Debug, Clone)]
struct Device {
    node: String,
    port: &'static str,
}

fn dev(node: impl Into<String>, port: &'static str) -> Device {
    Device { node: node.into(), port }
}

// ────────────────────────────────────────────────────────────────────
// ROUTING TABLE — built from config so hardware/synth naming is not
// hardcoded.
// ────────────────────────────────────────────────────────────────────

enum Route {
    Channels { src: Device, dst: Device, map: &'static [(&'static str, &'static str)], exclusive: bool },
}

fn pairs(src: Device, dst: Device, map: &'static [(&'static str, &'static str)]) -> Route {
    Route::Channels { src, dst, map, exclusive: false }
}

fn pairs_exclusive(src: Device, dst: Device, map: &'static [(&'static str, &'static str)]) -> Route {
    Route::Channels { src, dst, map, exclusive: true }
}

fn routes(hardware: &HardwareConf) -> Vec<Route> {
    let mic_input = dev(hardware.mic_node_name.clone(), "capture_");
    let crisp_vocals_input = dev(NAME_CRISP_VOCALS, "in_");
    let crisp_vocals_out = dev(NAME_CRISP_VOCALS, "out_");
    let virtual_input_monitor = dev(NAME_VIRTUAL_INPUT, "monitor_");
    let virtual_mic_sink_in = dev(NAME_VIRTUAL_MIC, "playback_");

    let mut r = vec![
        // Processed voice -> virtual-mic's input.
        pairs(crisp_vocals_out, virtual_mic_sink_in.clone(), &[("L", "playback_FL"), ("R", "playback_FR")]),
        // "Anything connected" (virtual-input's raw monitor) fanned into virtual-mic.
        pairs(virtual_input_monitor, virtual_mic_sink_in, &[("FL", "playback_FL"), ("FR", "playback_FR")]),
    ];
    if !hardware.mic_node_name.is_empty() {
        r.insert(
            0,
            pairs_exclusive(mic_input, crisp_vocals_input, &[("FL", "in_L"), ("FR", "in_R")]),
        );
    }
    r
}

// ────────────────────────────────────────────────────────────────────
// PORT MODEL
// ────────────────────────────────────────────────────────────────────

/// Which stream kind a port carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortKind {
    AudioOut,
    AudioIn,
    MidiOut,
    MidiIn,
}

fn port_kind(format: &str, direction: &str) -> PortKind {
    let is_midi = format.contains("midi");
    let out = direction == "out";
    match (is_midi, out) {
        (true, true) => PortKind::MidiOut,
        (true, false) => PortKind::MidiIn,
        (false, true) => PortKind::AudioOut,
        (false, false) => PortKind::AudioIn,
    }
}

/// What the registry told us about one Port global.
struct PortInfo {
    node_id: u32,
    name: String,
    format: String,
    direction: String,
}

/// A port resolved against the current node-name table, for matching
/// against `Device` patterns and for issuing link create/destroy calls.
#[derive(Debug, Clone)]
struct RPort {
    id: u32,
    node_id: u32,
    device: String,
    name: String,
}

impl RPort {
    /// The part after the last `_` in the port name, e.g. `FL` from
    /// `capture_FL`, `1` from `monitor_1`. Used as a routing key.
    fn channel(&self) -> Option<&str> {
        self.name.rsplit_once('_').map(|(_, ch)| ch)
    }
}

impl std::fmt::Display for RPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.device, self.name)
    }
}

fn prop<'a>(props: Option<&'a DictRef>, key: &str) -> Option<&'a str> {
    props.and_then(|p| p.get(key))
}

// ────────────────────────────────────────────────────────────────────
// MANAGER — all live state, rebuilt incrementally from registry events.
// ────────────────────────────────────────────────────────────────────

struct Manager {
    core: pw::core::CoreRc,
    registry: pw::registry::RegistryRc,
    /// `--dry-run`: log every connect/disconnect/vmic-provision decision
    /// instead of actually issuing it. Read-only against the live graph --
    /// used to validate this against a real running system before it's ever
    /// allowed to mutate anything.
    dry_run: bool,
    linking: LinkingConf,
    hardware: HardwareConf,
    synth_conf: Option<SynthConf>,

    nodes: HashMap<u32, String>,
    ports: HashMap<u32, PortInfo>,
    /// link global id -> (output port id, input port id).
    links: HashMap<u32, (u32, u32)>,
    /// A link create we just issued, not yet confirmed via a registry Link
    /// event -- see `PENDING_LINK_TTL`.
    pending_links: HashMap<(u32, u32), Instant>,
    /// Node ids that have already been an endpoint of an actual `connect`/
    /// `disconnect` decision (i.e. both sides had real, resolved ports and a
    /// routing call was genuinely made) -- only populated/consulted when
    /// `linking.only_edit_links_on_node_init` is true. Once a node id is
    /// here, its links are never touched again (neither created nor torn
    /// down as "stray") until it disappears and a genuinely new node id
    /// takes its place; see `connect`/`disconnect`/`on_global_remove`.
    settled_nodes: HashSet<u32>,
    /// Node ids touched (as either endpoint of a `connect`/`disconnect`
    /// call) during the apply_routes pass currently in progress. Drained
    /// into `settled_nodes` at the end of the pass.
    pass_touched_nodes: HashSet<u32>,

    default_sink: Option<String>,
    link_factory: Option<String>,

    // Kept alive only so the "default" metadata subscription keeps running;
    // never read directly.
    _default_metadata: Option<pw::metadata::Metadata>,
    _default_metadata_listener: Option<pw::metadata::MetadataListener>,

    synth: Option<Child>,
    synth_stdin: Option<ChildStdin>,

    /// Last-seen mtime of `crisp-vocals.ron`, so `hardware`/`synth`/`linking`
    /// re-read on save like crisp-vocals' DSP chain does -- checked cheaply
    /// once per (already-debounced) routing pass rather than via its own
    /// filesystem watcher.
    conf_mtime: Option<SystemTime>,
}

impl Manager {
    fn new(
        core: pw::core::CoreRc,
        registry: pw::registry::RegistryRc,
        dry_run: bool,
        linking: LinkingConf,
        hardware: HardwareConf,
        synth_conf: Option<SynthConf>,
    ) -> Self {
        Manager {
            core,
            registry,
            dry_run,
            linking,
            hardware,
            synth_conf,
            nodes: HashMap::new(),
            ports: HashMap::new(),
            links: HashMap::new(),
            pending_links: HashMap::new(),
            settled_nodes: HashSet::new(),
            pass_touched_nodes: HashSet::new(),
            default_sink: None,
            link_factory: None,
            _default_metadata: None,
            _default_metadata_listener: None,
            synth: None,
            synth_stdin: None,
            conf_mtime: std::fs::metadata(config_path()).and_then(|m| m.modified()).ok(),
        }
    }

    /// Re-read `hardware`/`synth`/`linking` from `crisp-vocals.ron` if its
    /// mtime moved since we last looked -- makes config changes (e.g.
    /// `linking.monitor_through_default_output`, a new `mic_node_name`)
    /// take effect without restarting the service, matching crisp-vocals'
    /// hot-reload of the DSP chain from the same file.
    fn maybe_reload_conf(&mut self) {
        let path = config_path();
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if mtime.is_none() || mtime == self.conf_mtime {
            return;
        }
        self.conf_mtime = mtime;
        let conf = load_conf();
        if conf.linking != self.linking || conf.hardware.mic_node_name != self.hardware.mic_node_name {
            // A deliberate config edit is exactly the case where
            // `only_edit_links_on_node_init`'s "leave settled nodes alone"
            // freeze should NOT apply -- otherwise toggling e.g.
            // `monitor_through_default_output` after crisp-vocals/virtual-input/
            // virtual-mic have already settled (which, in steady state,
            // they always have) would silently no-op forever. Un-freeze
            // everything so this pass re-evaluates the whole routing table
            // against the new config, then it re-settles as normal.
            println!("[crisp-links] config changed: {:?} -> {:?}; re-applying routes", self.linking, conf.linking);
            self.settled_nodes.clear();
        }
        self.linking = conf.linking;
        self.hardware = conf.hardware;
        self.synth_conf = conf.synth;
    }

    fn synth_enabled(&self) -> bool {
        self.synth_conf.as_ref().is_some_and(|s| s.enabled)
    }

    // ── registry event handlers ───────────────────────────────────

    fn on_global(&mut self, obj: &GlobalObject<&DictRef>) {
        match obj.type_ {
            ObjectType::Node => {
                if let Some(name) = prop(obj.props, "node.name") {
                    self.nodes.insert(obj.id, name.to_string());
                }
            }
            ObjectType::Port => {
                if let (Some(format), Some(direction)) =
                    (prop(obj.props, "format.dsp"), prop(obj.props, "port.direction"))
                {
                    let node_id = prop(obj.props, "node.id").and_then(|s| s.parse().ok());
                    let name = prop(obj.props, "port.name").unwrap_or_default();
                    if let Some(node_id) = node_id {
                        self.ports.insert(
                            obj.id,
                            PortInfo {
                                node_id,
                                name: name.to_string(),
                                format: format.to_string(),
                                direction: direction.to_string(),
                            },
                        );
                    }
                }
            }
            ObjectType::Link => {
                let out_port = prop(obj.props, "link.output.port").and_then(|s| s.parse::<u32>().ok());
                let in_port = prop(obj.props, "link.input.port").and_then(|s| s.parse::<u32>().ok());
                if let (Some(o), Some(i)) = (out_port, in_port) {
                    self.pending_links.remove(&(o, i));
                    self.links.insert(obj.id, (o, i));
                }
            }
            ObjectType::Factory => {
                if prop(obj.props, "factory.type.name") == Some(ObjectType::Link.to_str()) {
                    if let Some(name) = prop(obj.props, "factory.name") {
                        self.link_factory = Some(name.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    fn on_global_remove(&mut self, id: u32) {
        self.nodes.remove(&id);
        self.ports.remove(&id);
        self.links.remove(&id);
        // If a node dies, a later reappearance gets a fresh global id and so
        // is treated as genuinely new -- forget it was ever settled.
        self.settled_nodes.remove(&id);
    }

    // ── port queries ───────────────────────────────────────────────

    /// All ports matching a device pattern and stream kind. Matching is
    /// case-insensitive so node names like `fluidsynth`, `FluidSynth` and
    /// `FLUID Synth (pid)` all match the same device pattern.
    fn resolved_ports(&self, d: &Device, kind: PortKind) -> Vec<RPort> {
        let node_q = d.node.to_lowercase();
        let port_q = d.port.to_lowercase();
        if node_q.is_empty() {
            return Vec::new();
        }
        self.ports
            .iter()
            .filter_map(|(&id, info)| {
                let device = self.nodes.get(&info.node_id)?;
                if !device.to_lowercase().contains(&node_q) || !info.name.to_lowercase().contains(&port_q) {
                    return None;
                }
                if port_kind(&info.format, &info.direction) != kind {
                    return None;
                }
                Some(RPort { id, node_id: info.node_id, device: device.clone(), name: info.name.clone() })
            })
            .collect()
    }

    fn sink_inputs(&self, sink_name: &str) -> Vec<RPort> {
        self.ports
            .iter()
            .filter_map(|(&id, info)| {
                let device = self.nodes.get(&info.node_id)?;
                if device != sink_name || !info.name.starts_with("playback_") {
                    return None;
                }
                if port_kind(&info.format, &info.direction) != PortKind::AudioIn {
                    return None;
                }
                Some(RPort { id, node_id: info.node_id, device: device.clone(), name: info.name.clone() })
            })
            .collect()
    }

    // ── link management (native create_object / destroy_global) ───

    fn link_exists(&mut self, out_id: u32, in_id: u32) -> bool {
        if self.links.values().any(|&(o, i)| o == out_id && i == in_id) {
            return true;
        }
        match self.pending_links.get(&(out_id, in_id)) {
            Some(t) if t.elapsed() < PENDING_LINK_TTL => true,
            _ => {
                self.pending_links.remove(&(out_id, in_id));
                false
            }
        }
    }

    /// Both endpoints' nodes are frozen (`only_edit_links_on_node_init` and
    /// neither is newly-appeared) -- this pair is not this daemon's business
    /// anymore, however the user has since rewired it by hand.
    fn pair_settled(&self, a_node: u32, b_node: u32) -> bool {
        self.linking.only_edit_links_on_node_init
            && self.settled_nodes.contains(&a_node)
            && self.settled_nodes.contains(&b_node)
    }

    fn connect(&mut self, source: &RPort, sink: &RPort) {
        if self.pair_settled(source.node_id, sink.node_id) {
            return;
        }
        self.pass_touched_nodes.insert(source.node_id);
        self.pass_touched_nodes.insert(sink.node_id);
        if self.link_exists(source.id, sink.id) {
            return;
        }
        if self.dry_run {
            // Deliberately NOT marked pending: re-logs every debounce pass
            // this route is still wanted, which is exactly what makes a
            // flapping/incorrect decision visible during dry-run.
            println!("[dry-run] would create link: {source} -> {sink}");
            return;
        }
        let Some(factory) = self.link_factory.clone() else {
            eprintln!("No link factory discovered yet; can't link {source} -> {sink}");
            return;
        };
        let props = properties! {
            "link.output.port" => source.id.to_string(),
            "link.input.port" => sink.id.to_string(),
            "link.output.node" => source.node_id.to_string(),
            "link.input.node" => sink.node_id.to_string(),
            // Persist independently of our local proxy -- we track/destroy
            // links by id via the registry, not by holding proxies.
            "object.linger" => "1",
        };
        match self.core.create_object::<pw::link::Link>(&factory, &props) {
            Ok(_link) => {
                self.pending_links.insert((source.id, sink.id), Instant::now());
                println!("Created link: {source} -> {sink}");
            }
            Err(e) => eprintln!("Failed to create link: {source} -> {sink}: {e}"),
        }
    }

    fn disconnect(&mut self, source: &RPort, sink: &RPort) {
        if self.pair_settled(source.node_id, sink.node_id) {
            return;
        }
        self.pass_touched_nodes.insert(source.node_id);
        self.pass_touched_nodes.insert(sink.node_id);
        let id = self.links.iter().find(|(_, &(o, i))| o == source.id && i == sink.id).map(|(&id, _)| id);
        if let Some(id) = id {
            if self.dry_run {
                println!("[dry-run] would remove link: {source} -> {sink}");
            } else {
                let _ = self.registry.destroy_global(id);
                println!("Removed link: {source} -> {sink}");
            }
        }
        self.pending_links.remove(&(source.id, sink.id));
    }

    // ── synth lifecycle (on-demand) ────────────────────────────────

    fn synth_running(&mut self) -> bool {
        self.synth_stdin.is_some() && self.synth.as_mut().is_some_and(|c| is_alive(c))
    }

    fn ensure_synth_running(&mut self) {
        if self.synth_running() {
            return;
        }
        let Some(synth_conf) = self.synth_conf.clone() else { return };
        if self.dry_run {
            println!("[dry-run] keyboard plugged in; would start {}", NAME_SYNTH);
            return;
        }
        if let Some(mut child) = self.synth.take() {
            if child.try_wait().ok().flatten().is_none() {
                eprintln!("{} died unexpectedly; restarting", NAME_SYNTH);
            }
            self.synth_stdin.take();
            let _ = child.kill();
            let _ = child.wait();
        }

        // JACK driver (`-a jack -o midi.driver=jack`) so the synth appears
        // in qpwgraph as ONE node ("fluidsynth-midi") with a MIDI-in port
        // and two audio-out ports. `-r 48000` matches the PipeWire JACK
        // sample rate. Stdin is piped and held open (fluidsynth's
        // interactive shell panics on stdin EOF; no `-i` since it exits
        // when stdin isn't a live shell).
        let mut child = match Command::new("fluidsynth")
            .args(["-a", "jack", "-r", "48000", "-c", "2", "-g", "1.0"])
            .args(["-o", "midi.driver=jack", &synth_conf.soundfont_path])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Failed to start {}: {}", NAME_SYNTH, e);
                return;
            }
        };

        self.synth_stdin = child.stdin.take();
        self.synth = Some(child);
        println!("Started {}", NAME_SYNTH);
    }

    fn stop_synth(&mut self) {
        self.synth_stdin.take();
        if let Some(mut child) = self.synth.take() {
            let _ = child.kill();
            let _ = child.wait();
            println!("Stopped {}", NAME_SYNTH);
        }
    }

    // ── route engine ───────────────────────────────────────────────

    fn apply_routes(&mut self) {
        self.maybe_reload_conf();
        if !self.linking.enabled {
            return;
        }
        self.ensure_virtual_sinks();
        for route in routes(&self.hardware) {
            self.apply_route(route);
        }
        self.apply_self_monitor_route();
        self.apply_virtual_input_speaker_route();
        if self.synth_enabled() {
            self.apply_synth_route();
        }
        self.unroute_stray_mix_links();

        // Only nodes that were an endpoint of a real connect/disconnect
        // decision this pass (i.e. had actual resolved ports on both sides)
        // get frozen -- see the doc comment on `settled_nodes` for why NOT
        // "every node known by now". A node with no ports yet is simply left
        // alone and gets its real first pass once they exist.
        if self.linking.only_edit_links_on_node_init {
            self.settled_nodes.extend(self.pass_touched_nodes.drain());
        } else {
            self.pass_touched_nodes.clear();
        }
    }

    /// `virtual-mic`'s own (mixed: virtual-input + processed voice) output -> the
    /// default speaker device. This is the ONLY self-listen path.
    fn apply_self_monitor_route(&mut self) {
        if !self.linking.monitor_through_default_output {
            self.remove_self_monitor_taps();
            return;
        }

        let Some(default) = self.default_sink.clone() else { return };
        if default.contains(NAME_VIRTUAL_MIC) || default.contains(NAME_VIRTUAL_INPUT) {
            return;
        }

        let sinks = self.sink_inputs(&default);
        if sinks.is_empty() {
            return;
        }

        let virtual_mic_monitor = dev(NAME_VIRTUAL_MIC, "monitor_");
        for monitor in self.resolved_ports(&virtual_mic_monitor, PortKind::AudioOut) {
            let Some(ch) = monitor.channel() else { continue };
            let want = format!("playback_{ch}");
            if let Some(sink) = sinks.iter().find(|s| s.name == want) {
                self.connect(&monitor, sink);
            }
        }

        let stray: Vec<(RPort, RPort)> = {
            let mut out = Vec::new();
            for &(out_id, in_id) in self.links.values() {
                let Some(src) = self.rport(out_id) else { continue };
                let Some(dst) = self.rport(in_id) else { continue };
                let from_self_monitor_tap = src.device.contains(NAME_VIRTUAL_MIC) && src.name.starts_with("monitor_");
                if from_self_monitor_tap && !(dst.device == default && dst.name.starts_with("playback_")) {
                    out.push((src, dst));
                }
            }
            out
        };
        for (src, dst) in stray {
            self.disconnect(&src, &dst);
        }
    }

    /// virtual-input's monitor ("anything connected", e.g. the synth) ALWAYS also
    /// reaches the default speaker device, unconditionally -- unlike
    /// virtual-mic's own self-monitor tap (gated by
    /// `linking.monitor_through_default_output`), this one is not optional:
    /// whatever you feed into virtual-input should always be audible to you.
    fn apply_virtual_input_speaker_route(&mut self) {
        let Some(default) = self.default_sink.clone() else { return };
        if default.contains(NAME_VIRTUAL_MIC) || default.contains(NAME_VIRTUAL_INPUT) {
            return;
        }

        let sinks = self.sink_inputs(&default);
        if sinks.is_empty() {
            return;
        }

        let virtual_input_monitor = dev(NAME_VIRTUAL_INPUT, "monitor_");
        for monitor in self.resolved_ports(&virtual_input_monitor, PortKind::AudioOut) {
            let Some(ch) = monitor.channel() else { continue };
            let want = format!("playback_{ch}");
            if let Some(sink) = sinks.iter().find(|s| s.name == want) {
                self.connect(&monitor, sink);
            }
        }
    }

    /// `linking.monitor_through_default_output` is false (the default) or
    /// was just toggled off: tear down any virtual-mic-monitor -> speaker
    /// links that may still be around from before.
    fn remove_self_monitor_taps(&mut self) {
        let taps: Vec<(RPort, RPort)> = self
            .links
            .values()
            .filter_map(|&(out_id, in_id)| {
                let src = self.rport(out_id)?;
                let dst = self.rport(in_id)?;
                let from_self_monitor_tap = src.device.contains(NAME_VIRTUAL_MIC) && src.name.starts_with("monitor_");
                from_self_monitor_tap.then_some((src, dst))
            })
            .collect();
        for (src, dst) in taps {
            self.disconnect(&src, &dst);
        }
    }

    /// Resolve a live port id to an `RPort`, or `None` if it's no longer
    /// (or not yet) in `self.ports`/`self.nodes`.
    fn rport(&self, id: u32) -> Option<RPort> {
        let info = self.ports.get(&id)?;
        let device = self.nodes.get(&info.node_id)?;
        Some(RPort { id, node_id: info.node_id, device: device.clone(), name: info.name.clone() })
    }

    /// Make sure both virtual nodes (`virtual-input`, `virtual-mic`) exist. The
    /// filter-chain (pipewire.conf.d/99-crisp-vocals.conf) provides them at
    /// PipeWire startup; as a fallback we can provision a Pulse null-sink
    /// each so there's always something to route to/pick. This is the one
    /// remaining subprocess spawn in the routine path, and only actually
    /// runs if one is somehow missing (in practice: never, once
    /// 99-crisp-vocals.conf is loaded).
    fn ensure_virtual_sinks(&mut self) {
        self.ensure_named_sink(NAME_VIRTUAL_INPUT);
        self.ensure_named_sink(NAME_VIRTUAL_MIC);
    }

    fn ensure_named_sink(&mut self, name: &str) {
        if self.nodes.values().any(|n| n.contains(name)) {
            return;
        }
        if self.dry_run {
            println!("[dry-run] {name} missing; would provision a Pulse null-sink fallback");
            return;
        }
        let out = Command::new("pactl").args(["load-module", "module-null-sink", &format!("sink_name={name}")]).output();
        match out {
            Ok(o) => println!("Loaded {name} null-sink: {}", String::from_utf8_lossy(&o.stdout).trim()),
            Err(e) => eprintln!("Failed to provision {name} fallback sink: {e}"),
        }
    }

    fn apply_route(&mut self, route: Route) {
        match route {
            Route::Channels { src, dst, map, exclusive } => self.route_channels(src, dst, map, exclusive),
        }
    }

    fn route_channels(&mut self, src: Device, dst: Device, map: &[(&str, &str)], exclusive: bool) {
        let sources = self.resolved_ports(&src, PortKind::AudioOut);
        let sinks = self.resolved_ports(&dst, PortKind::AudioIn);

        for source in &sources {
            let Some(ch) = source.channel() else { continue };
            let intended: Vec<&str> = map.iter().filter(|(key, _)| key == &ch).map(|(_, name)| *name).collect();
            if intended.is_empty() {
                continue;
            }
            for sink_name in &intended {
                if let Some(sink) = sinks.iter().find(|s| &s.name == sink_name) {
                    self.connect(source, sink);
                }
            }
            if exclusive {
                let leaks: Vec<RPort> =
                    sinks.iter().filter(|s| !intended.contains(&s.name.as_str())).cloned().collect();
                for leak in leaks {
                    self.disconnect(source, &leak);
                }
            }
        }
    }

    /// Anything on the crisp-vocals/virtual-input/virtual-mic nodes that isn't the
    /// routing table above is stray, so links stay exact even when apps
    /// auto-connect. crisp-vocals' output may ONLY reach virtual-mic's
    /// input, and virtual-input's monitor may ONLY reach virtual-mic's input or
    /// the current default speaker device (its permanent, always-on tap --
    /// see `apply_virtual_input_speaker_route`).
    fn unroute_stray_mix_links(&mut self) {
        let default = self.default_sink.clone();
        let stray: Vec<(RPort, RPort)> = {
            let mut out = Vec::new();
            for &(out_id, in_id) in self.links.values() {
                let Some(src) = self.rport(out_id) else { continue };
                let Some(dst) = self.rport(in_id) else { continue };

                let crisp_vocals_out = src.device.contains(NAME_CRISP_VOCALS) && src.name.starts_with("out_");
                let to_virtual_mic = dst.device.contains(NAME_VIRTUAL_MIC) && dst.name.starts_with("playback_");

                let mic_to_proc = !self.hardware.mic_node_name.is_empty()
                    && src.device.to_lowercase().contains(&self.hardware.mic_node_name.to_lowercase())
                    && dst.device.contains(NAME_CRISP_VOCALS)
                    && ((src.name == "capture_FL" && dst.name == "in_L")
                        || (src.name == "capture_FR" && dst.name == "in_R"));
                let bad_crisp_vocals_in =
                    dst.device.contains(NAME_CRISP_VOCALS) && dst.name.starts_with("in_") && !mic_to_proc;
                let bad_crisp_vocals_out = crisp_vocals_out && !to_virtual_mic;

                let from_virtual_input_monitor = src.device.contains(NAME_VIRTUAL_INPUT) && src.name.starts_with("monitor_");
                let to_default_speaker =
                    default.as_deref().is_some_and(|d| dst.device == d) && dst.name.starts_with("playback_");
                let bad_virtual_input_out = from_virtual_input_monitor && !to_virtual_mic && !to_default_speaker;

                if bad_crisp_vocals_in || bad_crisp_vocals_out || bad_virtual_input_out {
                    out.push((src, dst));
                }
            }
            out
        };
        for (src, dst) in stray {
            self.disconnect(&src, &dst);
        }
    }

    /// MIDI keyboard -> fluidsynth -> virtual-input. The synth runs only while the
    /// keyboard is plugged in. No-op unless `synth.enabled` in
    /// `crisp-vocals.ron`.
    fn apply_synth_route(&mut self) {
        let Some(synth_conf) = self.synth_conf.clone() else { return };
        let keyboard = dev(synth_conf.midi_keyboard_name.clone(), "");
        let keyboard_plugged = !self.resolved_ports(&keyboard, PortKind::MidiOut).is_empty();

        if !keyboard_plugged {
            if self.synth.is_some() {
                self.stop_synth();
            }
            return;
        }

        self.ensure_synth_running();
        if !self.synth_running() {
            return;
        }

        let synth_any = dev(NAME_SYNTH, "");
        let virtual_input_sink_in = dev(NAME_VIRTUAL_INPUT, "playback_");

        let synth_midi_in = self.resolved_ports(&synth_any, PortKind::MidiIn).into_iter().next();
        if let Some(synth_in) = synth_midi_in {
            for kb in self.resolved_ports(&keyboard, PortKind::MidiOut) {
                self.connect(&kb, &synth_in);
            }
        }

        // Feed virtual-input once; its monitor is fanned into virtual-mic by
        // `routes()`, so the synth ends up audible in your own monitor as
        // well as to whoever captures the mic.
        let virtual_input_ins = self.resolved_ports(&virtual_input_sink_in, PortKind::AudioIn);
        for port in self.resolved_ports(&synth_any, PortKind::AudioOut) {
            let dest = match port.name.as_str() {
                "left" | "output_FL" | "FL" => "playback_FL",
                "right" | "output_FR" | "FR" => "playback_FR",
                _ => continue,
            };
            if let Some(sink) = virtual_input_ins.iter().find(|s| s.name == dest) {
                self.connect(&port, sink);
            }
        }

        self.unroute_stray_synth_links();
    }

    fn unroute_stray_synth_links(&mut self) {
        let stray: Vec<(RPort, RPort)> = {
            let mut out = Vec::new();
            for &(out_id, in_id) in self.links.values() {
                let Some(src) = self.rport(out_id) else { continue };
                let Some(dst) = self.rport(in_id) else { continue };
                let from_synth = src.device.to_lowercase().contains(&NAME_SYNTH.to_lowercase());
                let to_virtual_input = dst.device.contains(NAME_VIRTUAL_INPUT) && dst.name.starts_with("playback_");
                if from_synth && !to_virtual_input {
                    out.push((src, dst));
                }
            }
            out
        };
        for (src, dst) in stray {
            self.disconnect(&src, &dst);
        }
    }
}

fn is_alive(child: &mut Child) -> bool {
    child.try_wait().ok().flatten().is_none()
}

/// Pull `"name"` out of a PipeWire metadata JSON value, e.g.
/// `{"name":"alsa_output...."}` -> `alsa_output....`. Metadata values for
/// `default.audio.sink` are always this shape in practice; a tiny ad-hoc
/// extractor keeps this crate free of a JSON dependency for one field.
fn extract_json_name(value: &str) -> Option<String> {
    let after_key = &value[value.find("\"name\"")? + 6..];
    let after_colon = after_key[after_key.find(':')? + 1..].trim_start();
    let after_quote = after_colon.strip_prefix('"')?;
    let end = after_quote.find('"')?;
    Some(after_quote[..end].to_string())
}

// ────────────────────────────────────────────────────────────────────
// MAIN — persistent PipeWire client, event-driven.
// ────────────────────────────────────────────────────────────────────

/// `crisp-links mic ...` -- CLI-only config editing, no daemon/PipeWire
/// client involved. Exits the process directly (success or failure) rather
/// than returning, so `main()`'s daemon path below never runs alongside it.
#[derive(Parser, Debug)]
#[command(name = "crisp-links", about = "PipeWire auto-wiring for the crisp-vocals stack")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
    /// Log every connect/disconnect/vmic-provision/synth-start decision
    /// instead of performing it. Ignored when a subcommand is given.
    #[arg(long, global = true)]
    dry_run: bool,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Get/set hardware.mic_node_name in ~/.config/pipewire/crisp-vocals.ron
    Mic(MicArgs),
}

#[derive(Args, Debug)]
struct MicArgs {
    /// Set hardware.mic_node_name to this exact PipeWire node name.
    node_name: Option<String>,
    /// Re-run auto-detection (same logic as first-run bootstrap) and set
    /// hardware.mic_node_name to the result.
    #[arg(long)]
    auto: bool,
    /// List current PipeWire audio source nodes instead of setting anything.
    #[arg(long)]
    list: bool,
}

fn run_mic_command(args: MicArgs) -> ! {
    if let Err(e) = crisp_config::bootstrap_if_missing() {
        eprintln!("[crisp-links] config bootstrap failed: {e}");
    }

    if args.list {
        match crisp_config::list_audio_sources() {
            Ok(sources) if sources.is_empty() => {
                eprintln!("[crisp-links] no audio sources found (is wpctl/PipeWire running?)");
                std::process::exit(1);
            }
            Ok(sources) => {
                for s in sources {
                    println!("{s}");
                }
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("[crisp-links] failed to list audio sources: {e}");
                std::process::exit(1);
            }
        }
    }

    let mic = if args.auto {
        match crisp_config::detect_default_mic() {
            Some(m) => m,
            None => {
                eprintln!("[crisp-links] could not auto-detect the default audio source");
                std::process::exit(1);
            }
        }
    } else if let Some(name) = args.node_name {
        name
    } else {
        eprintln!("usage: crisp-links mic <node-name> | crisp-links mic --auto | crisp-links mic --list");
        std::process::exit(2);
    };

    let path = config_path();
    match crisp_config::update_mic_node_name(&path, &mic) {
        Ok(()) => {
            println!("set hardware.mic_node_name = \"{mic}\" in {}", path.display());
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("[crisp-links] failed to update {}: {e}", path.display());
            std::process::exit(1);
        }
    }
}

fn main() {
    let cli = Cli::parse();
    if let Some(Commands::Mic(args)) = cli.command {
        run_mic_command(args);
    }
    let dry_run = cli.dry_run;

    if let Err(e) = crisp_config::bootstrap_if_missing() {
        eprintln!("[crisp-links] config bootstrap failed: {e}");
    }
    let conf = load_conf();

    if conf.hardware.mic_node_name.is_empty() {
        eprintln!(
            "[crisp-links] hardware.mic_node_name is blank in crisp-vocals.ron -- the physical mic won't be \
             routed to crisp-vocals until it's set. Run `crisp-links mic --auto` or `crisp-links mic <node-name>`, \
             or edit the config by hand."
        );
    }

    pw::init();

    // Intentionally leaked: this is the one main loop for the process's
    // entire lifetime, so a genuine `'static` reference to it (rather than
    // fighting the borrow checker over a local variable's lexical scope) is
    // the natural fit -- the OS reclaims it at process exit regardless.
    let main_loop: &'static pw::main_loop::MainLoopRc =
        Box::leak(Box::new(pw::main_loop::MainLoopRc::new(None).expect("failed to create PipeWire main loop")));

    let ml = main_loop.clone();
    let _sig_int = main_loop.loop_().add_signal_local(Signal::INT, move || ml.quit());
    let ml = main_loop.clone();
    let _sig_term = main_loop.loop_().add_signal_local(Signal::TERM, move || ml.quit());

    let context = pw::context::ContextRc::new(main_loop, None).expect("failed to create PipeWire context");
    let core = context.connect_rc(None).expect("failed to connect to PipeWire");
    let registry = core.get_registry_rc().expect("failed to get PipeWire registry");

    let manager = Rc::new(RefCell::new(Manager::new(
        core.clone(),
        registry.clone(),
        dry_run,
        conf.linking.clone(),
        conf.hardware.clone(),
        conf.synth.clone(),
    )));

    // The debounce timer: (re)armed on every registry/metadata event, fires
    // `apply_routes()` once no further event has arrived for `DEBOUNCE`.
    let timer_manager = Rc::clone(&manager);
    let timer: Rc<pw::loop_::TimerSource<'static>> =
        Rc::new(main_loop.loop_().add_timer(move |_expirations| {
            timer_manager.borrow_mut().apply_routes();
        }));

    let registry_weak = registry.downgrade();
    let manager_for_global = Rc::clone(&manager);
    let timer_for_global = Rc::clone(&timer);
    let _registry_listener = registry
        .add_listener_local()
        .global(move |obj| {
            manager_for_global.borrow_mut().on_global(obj);

            // The "default" metadata object needs its own bound listener to
            // receive property (default sink) changes -- registry `global`
            // events alone don't carry metadata's internal key/value store.
            if obj.type_ == ObjectType::Metadata && prop(obj.props, "metadata.name") == Some("default") {
                if let Some(registry) = registry_weak.upgrade() {
                    if let Ok(metadata) = registry.bind::<pw::metadata::Metadata, _>(obj) {
                        let manager_for_prop = Rc::clone(&manager_for_global);
                        let timer_for_prop = Rc::clone(&timer_for_global);
                        let listener = metadata
                            .add_listener_local()
                            .property(move |_subject, key, _type, value| {
                                if key == Some("default.audio.sink") {
                                    let sink = value.and_then(extract_json_name);
                                    manager_for_prop.borrow_mut().default_sink = sink;
                                    let _ = timer_for_prop.update_timer(Some(DEBOUNCE), None);
                                }
                                0
                            })
                            .register();
                        let mut m = manager_for_global.borrow_mut();
                        m._default_metadata = Some(metadata);
                        m._default_metadata_listener = Some(listener);
                    }
                }
            }

            let _ = timer_for_global.update_timer(Some(DEBOUNCE), None);
        })
        .global_remove({
            let manager = Rc::clone(&manager);
            let timer = Rc::clone(&timer);
            move |id| {
                manager.borrow_mut().on_global_remove(id);
                let _ = timer.update_timer(Some(DEBOUNCE), None);
            }
        })
        .register();

    // Config-reload timer: routing itself stays event-driven (registry +
    // "default" metadata, above), but nothing else generates a PipeWire
    // event when you just edit crisp-vocals.ron -- so a cheap once-a-second
    // mtime check is the only way `linking`/`hardware`/`synth` changes take
    // effect without waiting for an unrelated app to open/close. A no-op
    // `fs::metadata` stat every second is negligible; only an actual
    // mtime change triggers `apply_routes()`.
    let reload_manager = Rc::clone(&manager);
    let reload_timer: Rc<pw::loop_::TimerSource<'static>> =
        Rc::new(main_loop.loop_().add_timer(move |_expirations| {
            reload_manager.borrow_mut().apply_routes();
        }));
    let _ = reload_timer.update_timer(Some(CONF_POLL), Some(CONF_POLL));

    println!(
        "crisp-links starting (event-driven, no polling; linking.enabled={}, only-edit-links-on-node-init={}, \
         synth-enabled={}){}...",
        conf.linking.enabled,
        conf.linking.only_edit_links_on_node_init,
        conf.synth.as_ref().is_some_and(|s| s.enabled),
        if dry_run { " [DRY RUN -- no links will be created/destroyed]" } else { "" }
    );
    main_loop.run();

    // Deliberately no `pw::deinit()` here: this only returns after
    // `main_loop.quit()` (SIGINT/SIGTERM), and several live objects above
    // (`_sig_int`/`_sig_term`, `context`, `registry`, `manager`'s metadata
    // proxy+listener, `timer`, `_registry_listener`) still need to run their
    // Drop impls -- which make FFI calls back into PipeWire -- AFTER this
    // point, as `main` returns. Calling `deinit()` before that segfaults
    // here on exactly that. Simplest correct fix for a long-running daemon
    // that only ever exits via process termination: let the OS reclaim
    // everything instead.
}
