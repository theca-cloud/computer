#!/usr/bin/env sh
# Starts the test server's database and writes .env. Idempotent. Start over:
# rm -rf data/ .env
set -eu
cd "$(dirname "$0")/.."

# A host port: the one .env has, else `default` or the next free one (another
# project, core's dev Postgres, may hold it), kept in .env from then on.
port() {
    value=$(sed -n "s/^$1=//p" .env 2>/dev/null || true)
    if [ -z "$value" ]; then
        value=$2
        while nc -z 127.0.0.1 "$value" 2>/dev/null; do
            value=$((value + 1))
        done
    fi
    echo "$value"
}
postgres_port=$(port POSTGRES_PORT 5432)

if [ -f .env ]; then
    password=$(sed -n 's/^POSTGRES_PASSWORD=//p' .env)
elif [ -d data/postgres ]; then
    echo "data/postgres exists but .env is missing: its password is lost. Run: rm -rf data/" >&2
    exit 1
else
    password=$(openssl rand -hex 24)
fi
cat > .env <<ENV
# Written by scripts/dev.sh. The port: edit it here, then run it again.
POSTGRES_PORT=$postgres_port
POSTGRES_PASSWORD=$password
# The end-to-end tests create a database per test on this server.
DATABASE_URL=postgres://theca:$password@localhost:$postgres_port/theca
ENV
docker compose up -d --wait
echo "Postgres is up on $postgres_port; .env written"
