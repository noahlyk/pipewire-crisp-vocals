#!/bin/bash
# ExecStart for the single pipewire-crisp-vocals.service unit: supervises
# both crisp-vocals (DSP, via pw-jack) and crisp-links (wiring) as one
# systemd unit.
#
#   - SIGTERM/SIGINT (systemctl stop, or a normal shutdown) are forwarded to
#     both children; this script then waits for them to exit and returns 0
#     -- a clean stop, not a failure, so Restart=on-failure leaves it down.
#   - If either child exits on its own (crash, or any non-signal exit) this
#     script brings the other one down too and exits non-zero, so
#     Restart=on-failure restarts the WHOLE unit rather than leaving one
#     half running alone.
#
# Binaries are resolved via PATH (systemd's default PATH includes
# /usr/bin, where PKGBUILD installs both) rather than hardcoded absolute
# paths, so this script doesn't need to know its own install prefix.
set -u

pw-jack crisp-vocals &
vocals_pid=$!
crisp-links &
links_pid=$!

forward_signal() {
    kill -s "$1" "$vocals_pid" 2>/dev/null
    kill -s "$1" "$links_pid" 2>/dev/null
}

clean_shutdown() {
    sig="$1"
    forward_signal "$sig"
    wait "$vocals_pid" 2>/dev/null
    wait "$links_pid" 2>/dev/null
    exit 0
}

trap 'clean_shutdown TERM' TERM
trap 'clean_shutdown INT' INT

# Block until EITHER child exits. During a normal `systemctl stop` this
# call is interrupted by the TERM trap above (which itself calls exit and
# never returns here). Reaching the lines below therefore means a child
# exited on its own -- always treated as a failure of the whole unit, even
# if that child's own exit code happened to be 0, since "one half of the
# stack quietly went away" is never a healthy steady state for this unit.
if wait -n "$vocals_pid" "$links_pid"; then
    first_status=0
else
    first_status=$?
fi

# Bring the other child down too, and propagate the worse of the two exit
# codes so systemd's journal shows what actually happened.
forward_signal TERM
wait "$vocals_pid" 2>/dev/null; vocals_status=$?
wait "$links_pid" 2>/dev/null; links_status=$?

status=$first_status
[ "$vocals_status" -gt "$status" ] && status=$vocals_status
[ "$links_status" -gt "$status" ] && status=$links_status
[ "$status" -eq 0 ] && status=1 # a child exiting at all is a failure here

exit "$status"
