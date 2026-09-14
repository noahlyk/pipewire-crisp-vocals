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
install=$pkgname.install

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

    # First-run setup helper
    install -Dm755 "scripts/first-run-setup.sh" "$pkgdir/usr/lib/crisp-vocals/first-run-setup.sh"

    # systemd user units
    install -Dm644 "systemd/crisp-vocals.service" "$pkgdir/usr/lib/systemd/user/crisp-vocals.service"
    install -Dm644 "systemd/crisp-links.service" "$pkgdir/usr/lib/systemd/user/crisp-links.service"
    install -Dm644 "systemd/crisp-vocals-setup.service" "$pkgdir/usr/lib/systemd/user/crisp-vocals-setup.service"

    # PipeWire config drop-ins
    install -Dm644 "config/99-crisp-vocals.conf" "$pkgdir/usr/share/pipewire/pipewire.conf.d/99-crisp-vocals.conf"
    install -Dm644 "config/99-crisp-vocals-low-latency.conf" "$pkgdir/usr/share/pipewire/pipewire.conf.d/99-crisp-vocals-low-latency.conf"

    # Example config + docs
    install -Dm644 "config/crisp-vocals.ron.example" "$pkgdir/usr/share/doc/$pkgname/crisp-vocals.ron.example"
    install -Dm644 "README.md" "$pkgdir/usr/share/doc/$pkgname/README.md"
    install -Dm644 "ARCHITECTURE.md" "$pkgdir/usr/share/doc/$pkgname/ARCHITECTURE.md"
    install -Dm644 "LICENSE" "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
