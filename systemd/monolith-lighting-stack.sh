#!/usr/bin/env bash
# One private-network namespace for the complete presentation path:
# QLC+ scene engine -> E1.31 -> Rust adapter -> OpenRGB SDK -> LEDs.
set -u -o pipefail

# Paths follow this checkout; MONOLITHD_OPENRGB and MONOLITHD_QLCPLUS override the AppImages.
repo=$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)
openrgb_appimage=${MONOLITHD_OPENRGB:-$HOME/AppImages/openrgb-1.0.appimage}
qlc_appimage=${MONOLITHD_QLCPLUS:-$HOME/AppImages/qlcplus-5.2.2-x86_64.AppImage}
workspace=$repo/qlcplus/monolith-lighting.qxw
adapter=$repo/target/release/monolithd

children=()

cleanup() {
    local child
    trap - EXIT INT TERM
    for child in "${children[@]}"; do
        kill -TERM "$child" 2>/dev/null || true
    done
    for child in "${children[@]}"; do
        wait "$child" 2>/dev/null || true
    done
}
trap cleanup EXIT
# systemd stops the stack by signalling this script alone (KillMode=mixed in the unit).
# That is a deliberate stop, so exit 0. A supervised child that exits on its own, cleanly
# or by a signal, is not: that is a failure below, so systemd restarts the stack.
trap 'exit 0' INT TERM

APPIMAGE_EXTRACT_AND_RUN=1 "$openrgb_appimage" \
    --server --server-host 127.0.0.1 --server-port 6742 &
openrgb_pid=$!
children+=("$openrgb_pid")

# OpenRGB must own the SDK socket before the Rust output adapter connects.
for _ in $(seq 1 100); do
    if (exec 3<>/dev/tcp/127.0.0.1/6742) 2>/dev/null; then
        exec 3>&-
        exec 3<&-
        break
    fi
    if ! kill -0 "$openrgb_pid" 2>/dev/null; then
        wait "$openrgb_pid"
        exit $?
    fi
    sleep 0.1
done
if ! (exec 3<>/dev/tcp/127.0.0.1/6742) 2>/dev/null; then
    echo "Monolith lighting stack: OpenRGB SDK did not listen on 127.0.0.1:6742" >&2
    exit 1
fi
exec 3>&-
exec 3<&-

# QLC+ prints every channel blend at debug level (about 12,000 lines a minute), which
# rotates the journal in minutes and erased the evidence of a crash. Keep warnings
# and errors, drop debug. To measure chaser phase, set QLC_LOGGING_RULES to a rule that
# leaves the default output alone (for example qt.network.ssl.warning=false) with
# `systemctl --user set-environment` and restart the stack. Not *.debug=true: that is
# every category, about 18,000 lines a second, and journald suppresses the lines needed.
APPIMAGE_EXTRACT_AND_RUN=1 QT_QPA_PLATFORM=minimal QT_LOGGING_RULES="${QLC_LOGGING_RULES:-*.debug=false;qt.network.ssl.warning=false}" "$qlc_appimage" \
    --open "$workspace" --web --web-port 9999 &
qlc_pid=$!
children+=("$qlc_pid")

"$adapter" e131-receiver &
adapter_pid=$!
children+=("$adapter_pid")

set +e
wait -n "$openrgb_pid" "$qlc_pid" "$adapter_pid"
status=$?
set -e
# A supervised child that exits, even cleanly, leaves the stack incomplete. Make that a
# failure: systemd Restart=on-failure restarts a failed stack but leaves a clean exit
# (status 0) stopped, which would leave the machine dark for good.
if [ "$status" -eq 0 ]; then
    echo "Monolith lighting stack: a child process exited cleanly; failing so the stack restarts" >&2
    status=1
fi
exit "$status"
