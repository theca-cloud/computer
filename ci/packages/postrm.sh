#!/bin/sh
# After theca-cli's files are gone: systemd forgets the unit that went with them.
set -e
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload || true
fi
exit 0
