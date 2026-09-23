#!/usr/bin/env bash
# Temporary, non-namespaced OpenRGB + E1.31 receiver so a normal GUI QLC+ session
# editing an intake workspace can drive the real hardware live while authoring.
#
# The production stack (monolith-lighting-stack.service) keeps its own OpenRGB and
# receiver inside a private network namespace -- QLC+ opened normally for editing
# can't reach into that namespace, so it has no way to preview on real hardware by
# default. This stands up the same output path (same OpenRGB flags, same receiver
# binary) un-namespaced instead, so loopback E1.31 from a normally-launched QLC+
# reaches it. Only one thing may own the hardware at a time, so this stops the
# production stack first and the caller restarts it with `stop` when done authoring.
#
# Runs both as transient systemd --user units, not raw backgrounded shell jobs: a
# first version tracked raw PIDs and both (a) lost the child process an
# --extract-and-run AppImage forks under (a different PID than `$!` captures, left
# running and holding the SDK port after "stop") and (b) lost the processes entirely
# once this script's own SSH session exited (kernel session hangup on the
# controlling terminal closing, which plain `disown` does not prevent). systemd's
# own cgroup-based supervision doesn't have either problem, and it's the pattern the
# production stack itself already uses -- reused, not reinvented, once the raw
# version broke live.
#
# Usage:
#   monolith-author-mode.sh start [WORKSPACE_PATH]   # defaults to the intake template
#   monolith-author-mode.sh stop
#   monolith-author-mode.sh status
set -u -o pipefail

openrgb_appimage=/home/chronic_frolic/AppImages/openrgb-1.0.appimage
qlc_appimage=/home/chronic_frolic/AppImages/qlcplus-5.2.2-x86_64.AppImage
adapter=/home/chronic_frolic/server-config/monolith-events/monolithd/target/release/monolithd
default_workspace="/home/chronic_frolic/server-config/qlcplus/intake/Monolithd Intake Template.qxw"
openrgb_unit=monolith-author-openrgb
receiver_unit=monolith-author-receiver

usage() {
    echo "usage: monolith-author-mode.sh <start [WORKSPACE_PATH] | stop | status>" >&2
    exit 2
}

cmd_status() {
    echo "openrgb:          $(systemctl --user is-active "$openrgb_unit" 2>/dev/null || echo inactive)"
    echo "e131 receiver:    $(systemctl --user is-active "$receiver_unit" 2>/dev/null || echo inactive)"
    echo "production stack: $(systemctl --user is-active monolith-lighting-stack.service 2>/dev/null || echo inactive)"
}

cmd_start() {
    local workspace="${1:-$default_workspace}"
    if systemctl --user is-active --quiet "$openrgb_unit" 2>/dev/null; then
        echo "author mode already running (see: $0 status). Run 'stop' first." >&2
        exit 1
    fi
    if [ ! -f "$workspace" ]; then
        echo "workspace not found: $workspace" >&2
        exit 1
    fi

    echo "Stopping the production stack (single owner of the hardware)..."
    systemctl --user stop monolith-lighting-stack.service 2>/dev/null || true

    echo "Starting OpenRGB (un-namespaced)..."
    systemctl --user reset-failed "$openrgb_unit" 2>/dev/null || true
    systemd-run --user --unit="$openrgb_unit" --description="author mode: un-namespaced OpenRGB" \
        --setenv=APPIMAGE_EXTRACT_AND_RUN=1 \
        -- "$openrgb_appimage" --server --server-host 127.0.0.1 --server-port 6742 \
        > /dev/null

    for _ in $(seq 1 100); do
        if (exec 3<>/dev/tcp/127.0.0.1/6742) 2>/dev/null; then
            exec 3>&- 3<&-
            break
        fi
        if ! systemctl --user is-active --quiet "$openrgb_unit"; then
            echo "OpenRGB exited before it started listening -- check: journalctl --user -u $openrgb_unit" >&2
            exit 1
        fi
        sleep 0.1
    done
    if ! (exec 3<>/dev/tcp/127.0.0.1/6742) 2>/dev/null; then
        echo "OpenRGB SDK did not listen on 127.0.0.1:6742 within 10s" >&2
        systemctl --user stop "$openrgb_unit" 2>/dev/null || true
        exit 1
    fi
    exec 3>&- 3<&-

    echo "Starting the E1.31 receiver (un-namespaced)..."
    systemctl --user reset-failed "$receiver_unit" 2>/dev/null || true
    systemd-run --user --unit="$receiver_unit" --description="author mode: un-namespaced E1.31 receiver" \
        -- "$adapter" e131-receiver \
        > /dev/null
    sleep 0.5
    if ! systemctl --user is-active --quiet "$receiver_unit"; then
        echo "the E1.31 receiver exited immediately -- check: journalctl --user -u $receiver_unit" >&2
        systemctl --user stop "$openrgb_unit" 2>/dev/null || true
        exit 1
    fi

    cat <<EOF

Author mode is up. Open QLC+ yourself, normally, pointed at the workspace you want
to edit -- edits will drive the real hardware live as you make them:

    APPIMAGE_EXTRACT_AND_RUN=1 "$qlc_appimage" --open "$workspace" &

When you're done, run:  $0 stop
(this restarts the normal production stack)
EOF
}

cmd_stop() {
    echo "Stopping author mode..."
    systemctl --user stop "$receiver_unit" "$openrgb_unit" 2>/dev/null || true
    systemctl --user reset-failed "$receiver_unit" "$openrgb_unit" 2>/dev/null || true
    echo "Restarting the production stack..."
    systemctl --user start monolith-lighting-stack.service
}

case "${1:-}" in
    start) shift; cmd_start "$@" ;;
    stop) cmd_stop ;;
    status) cmd_status ;;
    *) usage ;;
esac
