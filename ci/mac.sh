#!/bin/sh
# Theca's CI agents on the developer's Mac (ci/README.md, step 6), for as long
# as this runs (in `screen`, say; Ctrl-C stops all of it): the `macos`
# workflow's (native, the local backend) and the `linux-arm64` workflow's (a
# container in OrbStack). Both reach the CI server's gRPC through an SSH tunnel
# to its loopback port. Workflows wait while it doesn't run.
#   ci/mac.sh <ssh host>
set -eu

version=3.20.1 # the Woodpecker server's
port=19000     # the tunnel's local end
dir="$HOME/Library/Application Support/theca-ci"
agent="$dir/woodpecker-agent-$version"
container=theca-ci-agent
host=${1:?usage: ci/mac.sh <ssh host>}

umask 077
mkdir -p "$dir"
if [ ! -x "$agent" ]; then
    curl -fsSL "https://github.com/woodpecker-ci/woodpecker/releases/download/v$version/woodpecker-agent_darwin_arm64.tar.gz" |
        tar -xzO woodpecker-agent >"$agent.part"
    chmod 700 "$agent.part"
    mv "$agent.part" "$agent"
fi
# The server's shared agent secret: system agents, as its own.
ssh "$host" cat /data/docker/secrets/ci/WP_SHARED >"$dir/secret"

tunnel=
stop() {
    [ -z "$tunnel" ] || kill "$tunnel" 2>/dev/null || true
    docker rm -f $container >/dev/null 2>&1 || true
}
trap stop EXIT
trap 'exit 130' INT TERM

# The local backend gives each workflow a HOME of its own: the tools' homes are
# pinned to yours, or rustup installs the toolchain again every run and Volta
# finds no Node.
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}" CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" \
    VOLTA_HOME="${VOLTA_HOME:-$HOME/.volta}" DOCKER_CONFIG="${DOCKER_CONFIG:-$HOME/.docker}" \
    npm_config_cache="${npm_config_cache:-$HOME/.npm}"

# The agents give up when the server is gone a while (Sablier stops the CI
# when idle; the Mac changed networks): all of it starts again, until Ctrl-C.
while :; do
    ssh -N -o ExitOnForwardFailure=yes -o ServerAliveInterval=30 -o ServerAliveCountMax=3 \
        -L 127.0.0.1:$port:127.0.0.1:9000 "$host" &
    tunnel=$!
    docker rm -f $container >/dev/null 2>&1 || true
    docker run -d --rm --name $container \
        -v /var/run/docker.sock:/var/run/docker.sock \
        -v "$dir/secret:/run/secrets/agent:ro" \
        -v $container:/etc/woodpecker \
        -e WOODPECKER_SERVER=host.docker.internal:$port \
        -e WOODPECKER_AGENT_SECRET_FILE=/run/secrets/agent \
        -e WOODPECKER_BACKEND=docker \
        -e WOODPECKER_MAX_WORKFLOWS=1 \
        woodpeckerci/woodpecker-agent:v$version >/dev/null
    WOODPECKER_SERVER=127.0.0.1:$port WOODPECKER_AGENT_SECRET_FILE="$dir/secret" \
        WOODPECKER_BACKEND=local WOODPECKER_MAX_WORKFLOWS=1 \
        WOODPECKER_AGENT_CONFIG_FILE="$dir/agent.conf" "$agent" || true
    kill "$tunnel" 2>/dev/null || true
    echo "the agents stopped: again in 10 s (Ctrl-C stops)" >&2
    sleep 10
done
