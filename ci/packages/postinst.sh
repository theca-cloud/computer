#!/bin/sh
# After theca-cli's files are in place (a .deb's postinst: `configure [<old
# version>]`; an .rpm's %post: 1 for an install, 2 for an upgrade): systemd
# reads the new unit, and the daemons already running (`theca@<user>`) start
# again on the new binary, on an upgrade only. Nothing where systemd doesn't
# run (a container being built, a chroot).
set -e
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload || true
    upgrade=false
    case "${1:-}" in
    configure) [ -n "${2:-}" ] && upgrade=true ;;
    *[!0-9]* | '') ;;
    *) [ "$1" -ge 2 ] && upgrade=true ;;
    esac
    if $upgrade; then
        systemctl try-restart 'theca@*.service' || true
    fi
fi
exit 0
