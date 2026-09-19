# Monolith Event Controller runbook

## Current phase: observe-only suspend policy

`monolith_suspend_status.py` is deliberately read-only. It identifies the active
physical `seat0` session, reports its idle status and duration, lists systemd
inhibitors, and explains why automatic suspend is not yet eligible.

It must not call `systemctl suspend`, create an inhibitor, change RGB, or expose
a network endpoint in this phase.

## Intended suspend rule

Automatic suspension becomes eligible only when all conditions are true:

1. The active physical `seat0` session has been idle for the configured timeout.
2. No workload holds a blocking `sleep` inhibitor.
3. No manual remote suspend block is active.
4. No gaming or streaming safeguard is active.
5. The wake/start grace period has elapsed.

Do not use global `logind` `IdleAction` for this appliance: persistent SSH and
user-manager sessions make its all-sessions idle requirement unsuitable.

## Future controls

The manual remote block will be a named, systemd-managed `sleep` inhibitor with
an explicit reason and expiry. It will be added through the existing narrow
Monolith Remote allow-list only after this observer has been validated.
