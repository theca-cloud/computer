#!/bin/sh
# Before theca-cli's files go (a .deb's prerm: `remove`, or `upgrade` and
# others, which keep the daemon running; an .rpm's %preun: 0 for a removal,
# 1 for an upgrade): the daemons stop, so none runs a deleted binary. Their
# `enable` links stay, as Debian's packages leave them: reinstalling picks
# them up again.
set -e
case "${1:-}" in
remove | 0)
    if [ -d /run/systemd/system ]; then
        systemctl stop 'theca@*.service' || true
    fi
    ;;
esac
exit 0
