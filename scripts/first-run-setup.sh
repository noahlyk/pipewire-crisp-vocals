#!/bin/bash
# crisp-vocals-setup.service's ExecStart -- runs once per user, the first
# time crisp-vocals-setup.service is triggered on a machine with no
# ~/.config/pipewire/crisp-vocals.ron yet (see that unit's
# ConditionPathExists). Never overwrites an existing config.
set -euo pipefail

XDG_CONFIG_HOME="${XDG_CONFIG_HOME:-$HOME/.config}"
CONF_DIR="$XDG_CONFIG_HOME/pipewire"
CONF="$CONF_DIR/crisp-vocals.ron"
EXAMPLE="/usr/share/doc/pipewire-crisp-vocals/crisp-vocals.ron.example"

if [ -e "$CONF" ]; then
    echo "[crisp-vocals-setup] $CONF already exists; leaving it alone."
    exit 0
fi

if [ ! -e "$EXAMPLE" ]; then
    echo "[crisp-vocals-setup] example config not found at $EXAMPLE; nothing to install." >&2
    exit 1
fi

mkdir -p "$CONF_DIR"
cp "$EXAMPLE" "$CONF"
echo "[crisp-vocals-setup] installed default config to $CONF"

# Prefer wpctl (PipeWire/WirePlumber-native); fall back to pactl if it's
# unavailable or the default source can't be resolved.
mic_name=""
if command -v wpctl >/dev/null 2>&1; then
    mic_name="$(wpctl inspect @DEFAULT_AUDIO_SOURCE@ 2>/dev/null \
        | grep -m1 'node.name' \
        | sed -E 's/.*"([^"]+)".*/\1/')" || true
fi
if [ -z "$mic_name" ] && command -v pactl >/dev/null 2>&1; then
    default_source="$(pactl get-default-source 2>/dev/null || true)"
    if [ -n "$default_source" ]; then
        mic_name="$(pactl list sources 2>/dev/null \
            | awk -v src="$default_source" '
                /^Source #/ { name="" }
                /^\tName: / { name=$2 }
                name==src && /node\.description/ { print; exit }
              ' | sed -E 's/.*= "(.*)"/\1/')"
        # Fall back to the raw source name if the description lookup above
        # didn't find anything usable.
        mic_name="${mic_name:-$default_source}"
    fi
fi

if [ -n "$mic_name" ]; then
    sed -i "s/mic_node_name: \"\"/mic_node_name: \"${mic_name//\//\\/}\"/" "$CONF"
    echo "[crisp-vocals-setup] set hardware.mic_node_name = \"$mic_name\""
else
    echo "[crisp-vocals-setup] could not determine the default audio source;" \
         "edit hardware.mic_node_name in $CONF by hand." >&2
fi
