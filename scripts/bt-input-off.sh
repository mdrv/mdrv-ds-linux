#!/bin/bash
# bt-input-off.sh — disable bluetoothd's input plugin so mdrv-ds can own
# the L2CAP session directly.  Run as root; restarts bluetoothd.
set -euo pipefail

DROPIN="/etc/systemd/system/bluetooth.service.d/no-input.conf"
mkdir -p "$(dirname "$DROPIN")"
cat > "$DROPIN" <<'EOF'
[Service]
ExecStart=
ExecStart=/usr/lib/bluetooth/bluetoothd --noplugin=input
EOF
systemctl daemon-reload
systemctl restart bluetooth.service
echo "bluetoothd restarted with --noplugin=input"
