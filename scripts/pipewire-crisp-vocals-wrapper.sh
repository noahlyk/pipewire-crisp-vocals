#!/bin/bash
# ExecStart for the single pipewire-crisp-vocals.service unit: supervises
# virtual-devices (the virtual-input/virtual-mic PipeWire nodes, run as a
# plain client process rather than a conf.d drop-in so they only exist
# while this unit is up), crisp-vocals (DSP, via pw-jack) and crisp-links
# (wiring) as one systemd unit.
#
#   - SIGTERM/SIGINT (systemctl stop, or a normal shutdown) are forwarded to
#     all three children; this script then waits for them to exit and
#     returns 0 -- a clean stop, not a failure, so Restart=on-failure
#     leaves it down. Killing virtual-devices' `pipewire -c` process is
#     also what makes virtual-input/virtual-mic disappear from the graph
#     on stop.
#   - If any child exits on its own (crash, or any non-signal exit) this
#     script brings the other two down too and exits non-zero, so
#     Restart=on-failure restarts the WHOLE unit rather than leaving part
#     of it running alone.
#
# Binaries are resolved via PATH (systemd's default PATH includes
# /usr/bin, where PKGBUILD installs both) rather than hardcoded absolute
# paths, so this script doesn't need to know its own install prefix.
set -u

pipewire -c /usr/share/pipewire-crisp-vocals/virtual-devices.conf &
devices_pid=$!
pw-jack crisp-vocals &
vocals_pid=$!
crisp-links &
links_pid=$!

forward_signal() {
    kill -s "$1" "$devices_pid" 2>/dev/null
    kill -s "$1" "$vocals_pid" 2>/dev/null
    kill -s "$1" "$links_pid" 2>/dev/null
}

clean_shutdown() {
    sig="$1"
    forward_signal "$sig"
    wait "$devices_pid" 2>/dev/null
    wait "$vocals_pid" 2>/dev/null
    wait "$links_pid" 2>/dev/null
    exit 0
}

trap 'clean_shutdown TERM' TERM
trap 'clean_shutdown INT' INT

# Block until ANY child exits. During a normal `systemctl stop` this call
# is interrupted by the TERM trap above (which itself calls exit and never
# returns here). Reaching the lines below therefore means a child exited
# on its own -- always treated as a failure of the whole unit, even if
# that child's own exit code happened to be 0, since "part of the stack
# quietly went away" is never a healthy steady state for this unit.
if wait -n "$devices_pid" "$vocals_pid" "$links_pid"; then
    first_status=0
else
    first_status=$?
fi

# Bring the other children down too, and propagate the worst of the exit
# codes so systemd's journal shows what actually happened.
forward_signal TERM
wait "$devices_pid" 2>/dev/null; devices_status=$?
wait "$vocals_pid" 2>/dev/null; vocals_status=$?
wait "$links_pid" 2>/dev/null; links_status=$?

status=$first_status
[ "$devices_status" -gt "$status" ] && status=$devices_status
[ "$vocals_status" -gt "$status" ] && status=$vocals_status
[ "$links_status" -gt "$status" ] && status=$links_status
[ "$status" -eq 0 ] && status=1 # a child exiting at all is a failure here

exit "$status"
