#!/usr/bin/env bash
# What the tests start ends with them, however they end.
#
#   child.sh <silence> <grace> <folder or -> <command...>
#
# Runs the command until this script's input closes (the test process ended,
# or stopped its child) or stays silent for <silence> seconds: a test process
# killed while one of its threads waits on a FUSE drive keeps every file it
# had open, this pipe included, so only its heartbeat's silence says it is
# gone (theca/computer#1). Then SIGTERM, and SIGKILL <grace> seconds later
# (0: SIGKILL at once). Last, every mount at or below <folder> (a device's
# own: its drives) is detached, since a daemon killed leaves its mounts dead,
# and the script exits as the command did.
#
# The tests give it a process group of its own: no Ctrl-C and no `timeout`
# reaches it, so it always gets to see the test end. Never kill the script
# itself: its command would be left running.
silence=$1 grace=$2 folder=$3
shift 3
# Kept as 3: bash gives a background command /dev/null as its input.
exec 3<&0
"$@" </dev/null 3<&- &
child=$!
(
    while read -r -t "$silence" _; do :; done
    if [ "$grace" -gt 0 ]; then
        kill -TERM "$child" 2>/dev/null
        for _ in $(seq "$((grace * 10))"); do
            kill -0 "$child" 2>/dev/null || exit 0
            sleep 0.1
        done
    fi
    kill -KILL "$child" 2>/dev/null
) <&3 &
watcher=$!
exec 3<&-
# Quietly: bash reports a killed job on its own.
{ wait "$child"; } 2>/dev/null
status=$?
kill "$watcher" 2>/dev/null
if [ "$folder" != - ] && [ -r /proc/self/mounts ]; then
    # The mount table spells a space and the like in octal (`\040`).
    while read -r _ at _; do
        at=$(printf '%b' "$at")
        case "$at/" in "$folder"/*) fusermount3 -u -q -z "$at" ;; esac
    done </proc/self/mounts
fi
exit "$status"
