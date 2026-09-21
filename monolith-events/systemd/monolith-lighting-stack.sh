#!/usr/bin/env bash
# One private-network namespace for the complete presentation path:
# QLC+ scene engine -> E1.31 -> Rust adapter -> OpenRGB SDK -> LEDs.
set -u -o pipefail

openrgb_appimage=/home/chronic_frolic/AppImages/openrgb-1.0.appimage
qlc_appimage=/home/chronic_frolic/AppImages/qlcplus-5.2.2-x86_64.AppImage
workspace=/home/chronic_frolic/server-config/qlcplus/monolith-lighting.qxw
adapter=/home/chronic_frolic/server-config/monolith-events/monolithd/target/release/monolithd

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
trap cleanup EXIT INT TERM

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

APPIMAGE_EXTRACT_AND_RUN=1 QT_QPA_PLATFORM=minimal "$qlc_appimage" \
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
exit "$status"
