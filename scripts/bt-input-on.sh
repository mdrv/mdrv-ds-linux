#!/bin/bash
# bt-input-on.sh — restore bluetoothd default (input plugin enabled).
set -euo pipefail

DROPIN="/etc/systemd/system/bluetooth.service.d/no-input.conf"
rm -f "$DROPIN"
systemctl daemon-reload
systemctl restart bluetooth.service
echo "bluetoothd restarted with default input plugin"
