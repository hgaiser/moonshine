#!/bin/bash
# Shim that replaces systemd-run for container use.
# Moonshine calls: systemd-run --user --scope --collect --unit moonshine-session --property=... -- <program> <args>
# We strip all systemd-run flags and just exec the actual command.

while [[ $# -gt 0 ]]; do
    if [[ "$1" == "--" ]]; then
        shift
        break
    fi
    shift
done

exec "$@"
