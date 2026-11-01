#!/bin/sh
# Runs after install and after removal so systemd and the system bus see the bootshiftd unit,
# bus policy and activation file come or go. dbus-broker (Fedora, newer Ubuntu) does not watch
# its directories the way dbus-daemon does, so ask the bus to reload. polkit watches on its own.
set -e

if [ -d /run/systemd/system ]; then
    systemctl daemon-reload || true
fi

dbus-send --system --type=method_call --dest=org.freedesktop.DBus \
    / org.freedesktop.DBus.ReloadConfig >/dev/null 2>&1 || true
