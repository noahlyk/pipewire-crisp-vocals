# Maintainer: noahlyk <noahlykins@gmail.com>
pkgname=pipewire-crisp-vocals
pkgver=0.1.0
pkgrel=1
pkgdesc="Hot-reloadable PipeWire mic DSP chain (denoise/gate/compressor/EQ) + declarative auto-wiring into a single virtual mic"
arch=('x86_64' 'aarch64')
url="https://github.com/noahlyk/pipewire-crisp-vocals"
license=('MIT')
depends=('pipewire' 'jack2')
optdepends=('fluidsynth: optional MIDI-keyboard synth fan-in (synth.enabled in crisp-vocals.ron)')
makedepends=('rust' 'cargo')
options=('!debug')

source=()
sha256sums=()

build() {
    cd "$startdir"
    cargo build --release --locked
}

package() {
    cd "$startdir"

    # Binaries
    install -Dm755 "target/release/crisp-vocals" "$pkgdir/usr/bin/crisp-vocals"
    install -Dm755 "target/release/crisp-links" "$pkgdir/usr/bin/crisp-links"

    # Supervisor wrapper (single systemd unit starts both binaries)
    install -Dm755 "scripts/pipewire-crisp-vocals-wrapper.sh" "$pkgdir/usr/lib/crisp-vocals/pipewire-crisp-vocals-wrapper.sh"

    # systemd user unit (one unit, supervises both binaries)
    install -Dm644 "systemd/pipewire-crisp-vocals.service" "$pkgdir/usr/lib/systemd/user/pipewire-crisp-vocals.service"

    # PipeWire config drop-ins
    install -Dm644 "config/99-crisp-vocals.conf" "$pkgdir/etc/pipewire/pipewire.conf.d/99-crisp-vocals.conf"
    install -Dm644 "config/99-crisp-vocals-low-latency.conf" "$pkgdir/etc/pipewire/pipewire.conf.d/99-crisp-vocals-low-latency.conf"

    # Example config, packaged as a runtime asset (both binaries bootstrap
    # from this path on first run -- see crisp-config::EXAMPLE_CONF_PATH)
    install -Dm644 "config/crisp-vocals.ron.example" "$pkgdir/usr/share/pipewire-crisp-vocals/crisp-vocals.ron.example"

    # Docs
    install -Dm644 "README.md" "$pkgdir/usr/share/doc/$pkgname/README.md"
    install -Dm644 "ARCHITECTURE.md" "$pkgdir/usr/share/doc/$pkgname/ARCHITECTURE.md"
    install -Dm644 "LICENSE" "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}

post_install() {
    echo ""
    echo "==> pipewire-crisp-vocals installed. Enable it:"
    echo ""
    echo "    systemctl --user enable --now pipewire-crisp-vocals.service"
    echo ""
    echo "    That's it -- config is auto-created at ~/.config/pipewire/crisp-vocals.ron"
    echo "    with your mic auto-detected on first run. Edit that file to tune the DSP"
    echo "    chain (hot-reloads within ~50ms, no restart needed), or run"
    echo "    'crisp-links mic --list|--auto|<node-name>' to change the routed mic."
}

post_upgrade() {
    post_install
}
