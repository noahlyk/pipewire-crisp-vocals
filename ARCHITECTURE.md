# pipewire-crisp-vocals — Architecture

## Overview

Two small, single-purpose daemons plus two PipeWire config drop-ins, plus a
tiny shared library crate:

- **`crisp-vocals`** (JACK client) — the DSP chain. Reads raw mic audio,
  writes processed audio. Nothing else.
- **`crisp-links`** (native PipeWire registry client) — the wiring. Watches
  the graph and issues link create/destroy calls so the right nodes are
  connected to the right nodes. Also owns the `mic` CLI subcommand
  (`crisp-links mic <node-name> | --auto | --list`) for editing
  `hardware.mic_node_name` without hand-editing RON.
- **`crisp-config`** (library crate, no binary) — config-path resolution,
  first-run bootstrap (copy the packaged example config + auto-detect the
  mic), and the targeted `mic_node_name` text edit. Used by both binaries
  above so neither duplicates this logic.
- **`virtual-devices.conf`** — defines the two virtual devices (`virtual-input`,
  `virtual-mic`) crisp-links wires into. Run as a standalone `pipewire -c`
  client process (supervised by the wrapper script), not a conf.d drop-in,
  so both nodes only exist while the service is up.
- **`99-crisp-vocals-low-latency.conf`** — a tighter PipeWire clock
  quantum, unrelated to the DSP/wiring split above but shipped alongside it
  since it's what makes the whole chain feel instant.

Splitting DSP from wiring means either can be restarted, tested, or
reasoned about independently — `crisp-vocals` never touches the PipeWire
registry, `crisp-links` never touches a sample. Both binaries are started
and supervised together by ONE systemd unit,
`pipewire-crisp-vocals.service`, via a small wrapper script
(`scripts/pipewire-crisp-vocals-wrapper.sh`) that forwards signals to both
children and treats either one exiting on its own as a failure of the whole
unit -- see "Supervision" below.

## Signal flow

```
                    ┌──────────────────┐
 physical mic ─────►│   crisp-vocals   │
 (capture_FL/FR)    │  (JACK client)   │
                    │                  │
                    │ stereo2mono      │
                    │      ↓           │
                    │ rnnoise          │
                    │      ↓           │
                    │ expander         │
                    │      ↓           │
                    │ compressor       │
                    │      ↓           │
                    │ gate             │
                    │      ↓           │
                    │ eq               │
                    └────────┬─────────┘
                     out_L/out_R
                             │
                             ▼
  MIDI keyboard      ┌──────────────┐
       │             │  virtual-mic │───► apps (Discord/OBS/...)
       ▼             │ (filter-chain│         select this as their mic
  fluidsynth ──────► │  Audio/Sink+ │
  (on-demand,        │  Source pair)│───► monitor_FL/FR
   via virtual-input)        └──────┬───────┘         │
       ▲                     │                 ▼
       │              playback_FL/FR    (self-monitor tap,
    virtual-input ◄──── MIDI keyboard           crisp-links only route)
  (null-sink,                                  │
   monitor fanned                              ▼
   into virtual-mic)                   real speaker device
```

All mixing happens by PipeWire summing multiple links landing on the same
destination port (`virtual-mic`'s `playback_FL`/`playback_FR`) — neither
`crisp-vocals` nor the filter-chain config does any internal mixing.
`crisp-links` is the only thing that decides which sources land where.

## Why one virtual-mic lane, not two

The original single-machine setup (`micproc`/`pw-links`) ran TWO parallel
output lanes from the same DSP chain: `out_good_*` (RNNoise on, ~10ms
delay, feeding a `vmic_good` device apps captured) and `out_fast_*`
(RNNoise off, sub-1ms latency, feeding a separate `vmic_fast` device used
only for self-monitoring, so your own voice in your headphones wasn't
delayed by the denoiser).

This package collapses that to one lane (`out_L`/`out_R` → `virtual-mic`)
for two reasons:

1. **Packaging for other machines/setups, not one tuned rig.** The dual-lane
   split exists to solve one very specific problem — RNNoise's fixed
   pipeline delay being audible in a self-monitor path — which is a
   legitimate optimization but adds a whole second device, a second set of
   per-stage `disable_output_*` config flags, and a second `Denoiser`
   instance to explain and maintain. A general-purpose package should ship
   the simple, well-understood version first.
2. **One mic selection surface.** Apps and the self-monitor tap both point
   at the same `virtual-mic` device; there's no risk of accidentally
   capturing the wrong lane, and no "why do I sound different to Discord
   than to myself" surprise.

If RNNoise's added latency in the self-monitor path becomes a problem for a
given setup, the dual-lane design is a well-defined re-expansion: reintroduce
`out_good_*`/`out_fast_*` ports, a second `LaneChain`, and route the fast
lane's monitor to speakers while the good lane feeds `virtual-mic`. Nothing
in the single-lane `Stage`/`Dynamics`/`Gate`/`Biquad` code needs to change to
do that — only the port count and `VocalDsp::process`'s return type.

## Hot-reload design (`crisp-vocals`)

1. A background thread watches the config's *directory* (not the file
   itself) via `notify`'s inotify backend, non-recursively.
2. Watching the directory — and filtering events by file name — survives
   editors that save via rename-over-original (vim, and most "atomic save"
   tools): those invalidate a watch on the file's own inode, but the
   directory watch keeps seeing every event under it regardless of which
   inode currently backs the file name.
3. `EventKind::Access` events are explicitly excluded. The reload itself
   calls `fs::read_to_string` on the watched file, which generates an
   Access event for that same file — treating Access as "relevant" creates
   a self-sustaining reload loop (observed live upstream as thousands of
   chain rebuilds per minute, each one audibly resetting the dynamics/EQ
   envelope state).
4. A short burst of events from one save (write + rename + chmod, common
   with several editors) is coalesced into one reload via a 50ms
   `recv_timeout` window, not one rebuild per event.
5. All expensive work — parsing, string matching over stage types, biquad
   coefficient trig (`sin`/`cos`/`powf`), and the announce log line — runs
   on this background thread, never on the realtime audio thread.
6. The result is published via `ArcSwap<DspSnapshot>`. The JACK process
   callback does a lock-free `load()` every block and, only on a version
   change, clones the (small) prebuilt `Vec<Stage>` into its own RT-owned
   state (`VocalDsp::adopt`) — the realtime thread never allocates for
   anything but that clone, never locks, and never does string/config work.

`crisp-links` does not hot-reload: its `hardware`/`synth`/`linking` tables
are read once at startup, since they describe wiring topology (which nodes
to connect), not tunable runtime parameters — changing them meaningfully
usually means the physical setup changed too, which is a restart-worthy
event in practice.

## Wiring design (`crisp-links`)

- **Event-driven, not polling.** A persistent PipeWire client subscribes to
  the registry (node/port/link add+remove) and to the `"default"` metadata
  object (default sink changes). A debounce timer (50ms) coalesces a burst
  of events — e.g. ~100+ objects announced at PipeWire startup — into one
  route-application pass.
- **Native link creation**, not `pw-link` subprocess calls: links are
  created via `Core::create_object::<Link>()` and destroyed via
  `Registry::destroy_global()`, tracked by global id.
- **`only_edit_links_on_node_init`** (default true): once both endpoints of
  a link have been the subject of a real connect/disconnect decision, that
  node pair is "settled" and never touched again — manual relinking via
  `pw-link`/`qpwgraph` afterward is never fought. A node is only marked
  settled once it actually had resolved ports on both sides of a real
  routing decision, not merely once its `Node` global appeared (a node
  can — and on a full PipeWire restart, routinely does — appear a debounce
  tick before its own ports register).
- **Exclusive routes** (the physical-mic → `crisp-vocals` input mapping)
  additionally tear down anything landing on those input ports that isn't
  the intended source — so accidentally auto-connected apps don't leak into
  the mic chain's input.
- **Stray-link sweeps** (`unroute_stray_mix_links`,
  `unroute_stray_synth_links`) disconnect anything on the crisp-vocals/
  virtual-input/virtual-mic/synth nodes that isn't exactly the routing table above,
  so the graph stays exact even as apps auto-connect.
- **On-demand synth lifecycle**: if `synth.enabled`, fluidsynth is spawned
  only while `synth.midi_keyboard_name` is plugged in, and killed when it's
  unplugged — not run continuously.

## Config bootstrap (`crisp-config`)

Neither binary requires a separate setup step or unit anymore. At startup,
both `crisp-vocals::main` and `crisp-links::main` call
`crisp_config::bootstrap_if_missing()` before touching `crisp-vocals.ron`:

1. If `config_path()` already exists, it's a no-op.
2. Otherwise, it reads the packaged example config from
   `/usr/share/pipewire-crisp-vocals/crisp-vocals.ron.example` (installed
   there by `PKGBUILD`/`aur/PKGBUILD`), auto-detects the current default
   audio source (`wpctl inspect @DEFAULT_AUDIO_SOURCE@`, falling back to
   `pactl get-default-source` + `pactl list sources`), fills in
   `hardware.mic_node_name` via a targeted line-scan text edit (NOT a RON
   parse+reserialize — that would destroy hand-written comments like
   `// active_mode: "raw"` in the example), and writes the result to
   `config_path()`.
3. The actual file creation uses `OpenOptions::create_new` — atomic
   "create iff absent" at the OS level — so when both binaries race to
   bootstrap at once (the normal case now that one systemd unit starts them
   together), the loser's `AlreadyExists` is treated as success rather than
   clobbering the winner's write. A full temp-file-plus-rename dance was
   judged unnecessary for a one-time, small, single `write_all` of a config
   file.

The same `crisp-config::set_mic_node_name_in_text` text-edit routine backs
`crisp-links mic <node-name>` / `mic --auto`, so there's exactly one place
that knows how to safely rewrite that one line.

## Supervision (single systemd unit)

`pipewire-crisp-vocals.service` runs one `ExecStart`:
`scripts/pipewire-crisp-vocals-wrapper.sh`, a small bash script that starts
both `pw-jack crisp-vocals` and `crisp-links` as background jobs (both
binaries resolved via `PATH`, since `PKGBUILD` installs them to
`/usr/bin`) and:

- On SIGTERM/SIGINT (a normal `systemctl stop`), forwards the signal to
  both children, waits for a clean exit, and exits 0 — `Restart=on-failure`
  correctly leaves the unit stopped.
- If either child exits on its own for any other reason (crash, or any
  exit at all — even a "clean" 0, since one half of the stack going away
  unprompted is never a healthy steady state here), the wrapper brings the
  other child down too and exits non-zero with the worse of the two exit
  codes, so `Restart=on-failure` restarts the *whole* unit rather than
  leaving one half running alone.

This replaces the old three separate units (`crisp-vocals.service`,
`crisp-links.service`, `crisp-vocals-setup.service`) with one.

## Configuration is one shared file

`crisp-vocals.ron` is read by both binaries: `crisp-vocals` owns
`preamp_db`/`active_mode`/`modes`, `crisp-links` owns `hardware`/`synth`/
`linking`. Neither validates the other's tables strictly — `ron::Value` /
`Option` fallbacks mean a bad or absent table on either side degrades to a
documented default rather than refusing to start. This keeps the machine's
whole audio-routing description in one human-edited file without coupling
the two binaries' schemas together at the type level.
