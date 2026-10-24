# Maintainer: noahlyk <noahlykins@gmail.com>
pkgname=pipewire-crisp-vocals
pkgver=0.2.0
pkgrel=1
pkgdesc="Hot-reloadable PipeWire mic DSP chain (denoise/gate/compressor/EQ) + declarative auto-wiring into a single virtual mic"
arch=('x86_64' 'aarch64')
url="https://github.com/noahlyk/pipewire-crisp-vocals"
license=('MIT')
depends=('pipewire' 'jack')
optdepends=('fluidsynth: optional MIDI-keyboard synth fan-in (synth.enabled in crisp-vocals.ron)')
makedepends=('rust' 'cargo')
options=('!debug')

# Local builds from a git checkout report the tag, commit count and hash, the same as `cargo build`:
#   0.2.0+3.gabc1234, or 0.2.0+3.gabc1234.dirty with uncommitted changes.
# Anywhere without git (or on the release tag) the version above is used.
pkgver() {
    cd "$startdir"
    git rev-parse --git-dir >/dev/null 2>&1 || { echo "$pkgver"; return; }

    local version meta=() described dirty
    if described=$(git describe --tags --long --match 'v[0-9]*' 2>/dev/null); then
        local rest="${described%-g*}"
        version="${rest%-*}"
        version="${version#v}"
        local count="${rest##*-}"
        (( count > 0 )) && meta+=("$count.g${described##*-g}")
    else
        version="0.0.0"
        meta+=("g$(git rev-parse --short HEAD)")
    fi
    [ -n "$(git status --porcelain --untracked-files=no)" ] && meta+=(dirty)

    if (( ${#meta[@]} )); then
        local IFS=.
        version+="+${meta[*]}"
    fi
    echo "$version"
}

source=()
sha256sums=()

build() {
    cd "$startdir"
    PIPEWIRE_CRISP_VOCALS_VERSION="$pkgver" cargo build --release --locked
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

    # Low-latency clock quantum: a genuine system-wide daemon setting, so
    # it stays a permanent conf.d drop-in.
    install -Dm644 "config/99-crisp-vocals-low-latency.conf" "$pkgdir/etc/pipewire/pipewire.conf.d/99-crisp-vocals-low-latency.conf"

    # virtual-input/virtual-mic node definitions: run as a standalone
    # `pipewire -c` client process by the wrapper script (not a conf.d
    # drop-in), so the nodes only exist while the service is running.
    install -Dm644 "config/virtual-devices.conf" "$pkgdir/usr/share/pipewire-crisp-vocals/virtual-devices.conf"

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
