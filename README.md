# pipewire-crisp-vocals

A hot-reloadable PipeWire mic processing chain (denoiser, expander,
compressor, gate, EQ) plus declarative auto-wiring, packaged as an
installable Arch Linux app. Feeds one virtual mic device (`virtual-mic`)
that apps like Discord/OBS select, and that you self-monitor through.

## What it does

- **`crisp-vocals`** — a JACK client that runs your mic through a
  configurable DSP chain (stereo→mono fold, RNNoise spectral denoise,
  expander, compressor, soft gate, parametric EQ) and outputs `out_L`/
  `out_R`. The chain is described in `~/.config/pipewire/crisp-vocals.ron`
  and hot-reloads within ~50ms of saving — no restart needed.
- **`crisp-links`** — a persistent PipeWire client that wires the graph:
  physical mic → `crisp-vocals` → `virtual-mic`, plus an optional
  MIDI-keyboard/synth fan-in (`vinput`) and a self-monitor tap back to your
  speakers. Purely event-driven (registry subscription, no polling).
- Two PipeWire config drop-ins (`99-crisp-vocals.conf`,
  `99-crisp-vocals-low-latency.conf`) that define the `vinput` and
  `virtual-mic` virtual devices and a low-latency clock quantum.

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

1. **Load the PipeWire config drop-ins.** The package installs them to
   `/usr/share/pipewire/pipewire.conf.d/`; symlink them into your PipeWire
   config path if it isn't already scanned there:

   ```bash
   mkdir -p ~/.config/pipewire/pipewire.conf.d
   ln -s /usr/share/pipewire/pipewire.conf.d/99-crisp-vocals.conf ~/.config/pipewire/pipewire.conf.d/
   ln -s /usr/share/pipewire/pipewire.conf.d/99-crisp-vocals-low-latency.conf ~/.config/pipewire/pipewire.conf.d/
   systemctl --user restart pipewire pipewire-pulse wireplumber
   ```

2. **Enable the user services:**

   ```bash
   systemctl --user enable --now crisp-vocals-setup.service
   systemctl --user enable --now crisp-vocals.service
   systemctl --user enable --now crisp-links.service
   ```

   `crisp-vocals-setup.service` runs once: it copies the shipped example
   config to `~/.config/pipewire/crisp-vocals.ron` (never overwriting an
   existing one) and fills in `hardware.mic_node_name` from your current
   default audio source (via `wpctl inspect @DEFAULT_AUDIO_SOURCE@`, with a
   `pactl` fallback).

   **Why a user unit and not a `post_install` hook:** `pacman`'s install
   hooks run as root with no reliable invoking-user context — no real
   `$HOME`, no user D-Bus/PipeWire session to query the default audio
   source from. `PKGBUILD`'s `post_install()` here only prints these
   instructions; the actual per-user file write happens in
   `crisp-vocals-setup.service`, a normal user unit with a real session.

3. **Select `virtual-mic`** as your microphone in Discord/OBS/etc.

## Configuration reference

Everything lives in `~/.config/pipewire/crisp-vocals.ron`, hot-reloaded by
`crisp-vocals` (the DSP chain) on every save; `crisp-links` reads its own
three tables (`hardware`, `synth`, `linking`) once at startup.

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
systemctl --user disable --now crisp-vocals.service crisp-links.service crisp-vocals-setup.service
yay -Rns pipewire-crisp-vocals
rm ~/.config/pipewire/crisp-vocals.ron   # if you want to drop your tuned config too
```

## See also

[ARCHITECTURE.md](ARCHITECTURE.md) for the internal design (signal flow
diagram, hot-reload mechanism, why one virtual-mic lane instead of two).

## License

MIT — see [LICENSE](LICENSE).
