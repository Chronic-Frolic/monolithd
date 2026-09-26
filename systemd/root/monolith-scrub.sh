#!/usr/bin/env bash
# Root side of monolithd's storage reporter: scrub one btrfs mount and mirror its
# `btrfs scrub status` into a world-readable file. btrfs keeps scrub state in a root-only
# file (/var/lib/btrfs/scrub.status.<fsid>, mode 0600, measured 2026-09-26), so without
# this the unprivileged reporter cannot see progress or errors. The reporter reads
# /var/lib/monolith-scrub/<fsid>.status; the mirror is rewritten every 5 s while the
# scrub runs and once when it ends, and survives reboots.
#
# Installed as a root-owned copy and run through bash, never from the checkout: root must
# not execute a file its user can edit.
#   sudo install -D -m 0755 systemd/root/monolith-scrub.sh /usr/local/libexec/monolith-scrub
#   sudo install -m 0644 systemd/root/monolith-scrub@.service systemd/root/monolith-scrub@.timer /etc/systemd/system/
#   sudo systemctl daemon-reload
#   sudo systemctl enable --now "monolith-scrub@$(systemd-escape --path /storage/protected).timer"
# Scrub now:  sudo systemctl start "monolith-scrub@$(systemd-escape --path /storage/protected).service"
set -u -o pipefail

mount=${1:?usage: monolith-scrub MOUNTPOINT}
fsid=$(findmnt -n -o UUID --mountpoint "$mount")
if [ -z "$fsid" ]; then
    echo "monolith-scrub: $mount is not a mounted filesystem" >&2
    exit 1
fi
dir=/var/lib/monolith-scrub
install -d -m 0755 "$dir"

# Replace the mirror atomically, and only with a status btrfs actually produced.
mirror() {
    local temporary="$dir/.$fsid.status.tmp"
    if btrfs scrub status "$mount" > "$temporary" 2>/dev/null; then
        chmod 0644 "$temporary" && mv -f "$temporary" "$dir/$fsid.status"
    else
        rm -f "$temporary"
    fi
}

btrfs scrub start -B "$mount" &
scrub=$!
# systemd stops the unit by signalling this script alone (KillMode=mixed): cancel the
# scrub, record where it stopped, and exit.
trap 'btrfs scrub cancel "$mount" >/dev/null 2>&1; wait "$scrub"; mirror; exit 143' TERM INT
while kill -0 "$scrub" 2>/dev/null; do
    mirror
    sleep 5 &
    wait $!
done
wait "$scrub"
status=$?
mirror
exit "$status"
