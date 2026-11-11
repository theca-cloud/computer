#!/usr/bin/env bash
# A throwaway Theca server on this Mac, for drive-check.sh: CE's own
# docker-compose.yml, at http://localhost:8080 (browsers allow the web app
# on localhost without HTTPS; THECA_PORT for another port).
#   ./server.sh        start it (the first time: .env and secrets made), print the setup link
#   ./server.sh down   stop it and delete its data (./data, the secrets)
set -eu
cd "$(dirname "$0")"
url=http://localhost:${THECA_PORT:-8080}
if [ "${1:-}" = down ]; then
    docker compose down
    rm -rf secrets .env data
    exit 0
fi
# Its data beside this file: the server runs as uid 65532, Postgres as its
# own user, so the folders are theirs to write.
mkdir -p data/db data/theca
chmod 777 data/theca
if [ ! -e .env ]; then
    echo "PUBLIC_URL=$url" >.env
    mkdir -p secrets
    openssl rand -hex 24 >secrets/postgres_password
    echo "postgres://theca:$(cat secrets/postgres_password)@postgres:5432/theca" >secrets/database_url
    openssl rand -hex 32 >secrets/server.key
    openssl rand -hex 32 >secrets/data.key
    chmod 644 secrets/*
fi
docker compose up -d
for _ in $(seq 60); do
    link=$(docker compose logs theca 2>/dev/null | grep -o "$url/setup[^ ]*" | tail -1)
    [ -n "$link" ] && break
    sleep 2
done
if [ -n "${link:-}" ]; then
    echo "Open the setup link and create your account: $link"
else
    echo "Up at $url (no setup link in the logs: the account exists already, or see docker compose logs theca)"
fi
