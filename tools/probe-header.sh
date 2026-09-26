#!/usr/bin/env bash
# Diagnostic for finding where one device on the ARGB header ends and the next begins
# (for example a GPU bracket daisy-chained ahead of the strip).
#
#   probe-header.sh sweep [FROM TO [DWELL_MS]]   light header LED FROM..TO one at a time
#   probe-header.sh at N [SECONDS]               hold header LED N lit
#
# RAM and the eye display the LED number being lit: RAM fills 8 LEDs per stick, left to
# right, in green; each full 32 lights one of the eye's LEDs in blue.
#
# The lighting stack owns OpenRGB inside a private network namespace that a host process
# cannot reach, so this pauses the stack, runs the standalone OpenRGB SDK server (its
# normal unit, which conflicts with the stack), and always restores the stack on exit.
set -u
binary="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)/target/release/monolithd"

restore() {
    systemctl --user stop openrgb-sdk.service
    systemctl --user start monolith-lighting-stack.service
    echo "lighting stack restored"
}
trap restore EXIT

systemctl --user stop monolith-lighting-stack.service
systemctl --user start openrgb-sdk.service
for _ in $(seq 1 100); do
    if (exec 3<>/dev/tcp/127.0.0.1/6742) 2>/dev/null; then break; fi
    sleep 0.2
done
"$binary" probe-header "$@"
