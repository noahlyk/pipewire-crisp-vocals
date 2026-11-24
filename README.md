# pipewire-crisp-vocals

A hot-reloadable PipeWire mic processing chain (denoiser, expander,
compressor, gate, EQ) plus declarative auto-wiring, packaged as an
installable Arch Linux app. Feeds one virtual mic device (`virtual-mic`)
that apps like Discord/OBS select, and that you self-monitor through.

## What it does

One installed binary, `crisp-vocals`, run by the service as two processes:

- **`crisp-vocals dsp`** — a JACK client that runs your mic through a
  configurable DSP chain (stereo→mono fold, RNNoise spectral denoise,
  expander, compressor, soft gate, parametric EQ) and outputs `out_L`/
  `out_R`. The chain is described in `~/.config/pipewire/crisp-vocals.ron`
  and hot-reloads within ~50ms of saving — no restart needed.
- **`crisp-vocals links`** — a persistent PipeWire client that wires the graph:
  physical mic → `crisp-vocals` (the DSP) → `virtual-mic`, plus an optional
  MIDI-keyboard/synth fan-in (`virtual-input`) and a self-monitor tap back to your
  speakers. Purely event-driven (registry subscription, no polling).
  `crisp-vocals links --dry-run` logs what it would change without touching
  the graph (safe next to the running service).
- `virtual-input`/`virtual-mic` are defined in `config/virtual-devices.conf`
  and run as their own `pipewire -c` client process, supervised by the
  service alongside the `dsp` and `links` processes -- so those nodes only
  exist while the service is running, not for as long as the whole audio
  daemon is up. A separate conf.d drop-in (`99-crisp-vocals-low-latency.conf`)
  sets a system-wide low-latency clock quantum.

## Install

```bash
yay -S pipewire-crisp-vocals
```

or build from source with the root `PKGBUILD`:

```bash
git clone https://github.com/noahlyk/pipewire-crisp-vocals.git
cd pipewire-crisp-vocals
makepkg -si
```

## Setup

```bash
systemctl --user enable --now pipewire-crisp-vocals.service
```

That's it. The one unit supervises `virtual-devices` (the
`virtual-input`/`virtual-mic` nodes), `crisp-vocals dsp` and `crisp-vocals links` —
nothing to symlink, and the virtual devices disappear from the PipeWire
graph the moment the service is stopped. On first run it creates
`~/.config/pipewire/crisp-vocals.ron` from the shipped example and
auto-detects your mic (`hardware.mic_node_name`) from the current default
audio source (`wpctl inspect @DEFAULT_AUDIO_SOURCE@`, with a `pactl`
fallback).

Then select **`virtual-mic`** as your microphone in Discord/OBS/etc.

Changed mics, or the auto-detected one was wrong? Use the `mic` subcommand
instead of hand-editing the config:

```bash
crisp-vocals mic --list                 # see current PipeWire audio sources
crisp-vocals mic "USB PnP Audio Device"  # set it explicitly
crisp-vocals mic --auto                  # re-run auto-detection
```

## Upgrading

An upgrade replaces the binary, but the running service keeps the old
code until it is restarted. `crisp-vocals --version` (or `-V`) tells you
when that is the case, naming which of the two processes is stale:

```console
$ crisp-vocals --version
crisp-vocals 0.4.0+4.gabc1234 (dsp, links out-of-date, running 0.3.0+1.gdef5678)
```

The note in brackets appears only when a running process differs from the
binary you ran; nothing is added when they match or the service is not
running. Processes that differ from each other are listed separately
(`(dsp out-of-date, running 0.3.0; links out-of-date, running …)`), and one
started by a build too old to report its version shows `running an older
build` (this is what the first upgrade from 0.3.0, which still had a
separate `crisp-links` binary, reports). Either way, restart it (the mic
drops out for a moment while the chain comes back up):

```bash
systemctl --user restart pipewire-crisp-vocals.service
```

Versions are `0.3.0` for a release and `0.3.0+4.gabc1234` for a build from a
checkout (4 commits past the tag, at that commit; `.dirty` is appended for
uncommitted changes). The running processes record theirs in
`$XDG_RUNTIME_DIR/crisp-vocals/dsp.version` and `links.version` (see
[ARCHITECTURE.md](ARCHITECTURE.md)).

## Configuration reference

Everything lives in `~/.config/pipewire/crisp-vocals.ron`, hot-reloaded by
`crisp-vocals dsp` (the DSP chain) on every save; `crisp-vocals links`
re-reads its own three tables (`hardware`, `synth`, `linking`) within about a
second of a change.

```ron
(
    preamp_db: 12.0,
    active_mode: "vocals",   // which [modes.*] table is live

    hardware: (
        mic_node_name: "Komplete",   // physical mic's PipeWire node name (substring match)
    ),

    synth: (                          // optional; omit or set enabled: false to skip
        enabled: false,
        soundfont_path: "/usr/share/soundfonts/FluidR3_GM.sf2",
        midi_keyboard_name: "",
    ),

    linking: (
        enabled: true,
        only_edit_links_on_node_init: true,
        monitor_through_default_output: false,   // true to hear yourself through your speakers
    ),

    modes: {
        "vocals": ( stages: [ /* ... */ ] ),
        "raw": ( stages: [ /* ... */ ] ),
        "instrumental": ( stages: [ /* ... */ ] ),
    },
)
```

### Stage types (usable in any mode's `stages` list, in order)

- `stereo2mono` — input fold (`mode`: `"peak"` | `"average"` | `"left"` |
  `"right"`), stage 0 of any mode that wants mono processing.
- `rnnoise` — RNNoise spectral denoiser (fixed ~10ms delay when enabled).
- `expander` — downward expansion below `threshold_db`, floored at
  `range_db` (gentle noise reduction, never a hard mute).
- `compressor` — attenuates above `threshold_db` at `ratio`, with optional
  `makeup_db`.
- `gate` — hysteresis (`threshold_db`/`hysteresis_db`) + `hold_ms` +
  floored downward expansion; keys on an earlier stage via `detector`
  (default `"expander"`).
- `eq` — parametric biquads (`band: [(type: "LS"|"PK"|"HS", freq, gain_db,
  q)]`) plus an output `preamp_db` trim.

Every stage supports `enabled: false` to bypass it in place without
removing it — an empty or all-disabled `stages` list is a complete bypass,
same mechanism as every other mode (see the `"raw"` mode in the example).

Switching modes is just editing `active_mode` and saving.

## Uninstall

```bash
systemctl --user disable --now pipewire-crisp-vocals.service
yay -Rns pipewire-crisp-vocals
rm ~/.config/pipewire/crisp-vocals.ron   # if you want to drop your tuned config too
```

## See also

[ARCHITECTURE.md](ARCHITECTURE.md) for the internal design (signal flow
diagram, hot-reload mechanism, why one virtual-mic lane instead of two).

## License

MIT — see [LICENSE](LICENSE).
