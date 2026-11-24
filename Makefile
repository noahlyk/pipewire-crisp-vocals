# Local builds. The version comes from git (see crisp-config/build.rs):
#   tag v0.3.0 checked out      -> 0.3.0
#   4 commits past v0.3.0       -> 0.3.0+4.gabc1234
#   uncommitted changes         -> ...+dirty
# Release builds set PIPEWIRE_CRISP_VOCALS_VERSION explicitly, so they keep the plain version.
#
# `make install` installs the binary and its license only. The service (systemd user unit,
# wrapper script, PipeWire configs, example config) expects /usr paths, so it comes from the
# package: `make pkg`, then install that with pacman.

PREFIX ?= /usr/local
BIN := target/release/crisp-vocals
NAME := pipewire-crisp-vocals

.PHONY: all build install uninstall version clean check pkg

all: build

build:
	cargo build --release --locked

install: build
	install -Dm755 $(BIN) $(DESTDIR)$(PREFIX)/bin/crisp-vocals
	install -Dm644 LICENSE $(DESTDIR)$(PREFIX)/share/licenses/$(NAME)/LICENSE
	@echo "installed crisp-vocals $$($(DESTDIR)$(PREFIX)/bin/crisp-vocals --version | cut -d' ' -f2) to $(DESTDIR)$(PREFIX)/bin/crisp-vocals"

uninstall:
	rm -f $(DESTDIR)$(PREFIX)/bin/crisp-vocals
	rm -rf $(DESTDIR)$(PREFIX)/share/licenses/$(NAME)

version: build
	@$(BIN) --version

# What CI runs (.github/workflows/check.yml): no clippy finding, every test green, scripts parse.
check:
	cargo clippy --locked --all-targets -- -D warnings
	cargo test --locked
	bash -n scripts/pipewire-crisp-vocals-wrapper.sh
	bash -n $(NAME).install

# Build the Arch package from this checkout (git-derived pkgver, see PKGBUILD).
pkg:
	makepkg -f

clean:
	cargo clean
