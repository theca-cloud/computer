#!/usr/bin/env bash
# The tests' Postgres on GitHub's macOS and Windows runners, which have no
# Docker (Linux runs scripts/dev.sh): Postgres 18, initialized as the server
# requires (the builtin C.UTF-8 collation), on port 5433 (away from 5432,
# the default of the older Postgres the runners have installed), its URL
# written to $GITHUB_ENV as DATABASE_URL. Used by .github/workflows/test.yml.
set -euo pipefail

dir=$RUNNER_TEMP/postgres
case "$(uname -s)" in
Darwin)
    brew install -q postgresql@18
    bin=$(brew --prefix postgresql@18)/bin
    ;;
MINGW* | MSYS*)
    # EDB's binaries, unpacked: 30 s, where Chocolatey's installer takes 10 min.
    curl -fsSL -o "$RUNNER_TEMP/postgres.zip" \
        https://get.enterprisedb.com/postgresql/postgresql-18.6-5-windows-x64-binaries.zip
    unzip -q "$RUNNER_TEMP/postgres.zip" -d "$RUNNER_TEMP"
    bin=$RUNNER_TEMP/pgsql/bin
    ;;
*)
    echo "macOS and Windows only: Linux runs scripts/dev.sh" >&2
    exit 1
    ;;
esac

# -E UTF8: Windows' runner locale is WIN1252, which the builtin locale refuses.
"$bin/initdb" -D "$dir" -U theca -E UTF8 --auth=trust \
    --locale-provider=builtin --builtin-locale=C.UTF-8
"$bin/pg_ctl" -D "$dir" -o "-p 5433" -l "$RUNNER_TEMP/postgres.log" -w start
"$bin/createdb" -h localhost -p 5433 -U theca theca
echo "DATABASE_URL=postgres://theca@localhost:5433/theca" >>"$GITHUB_ENV"
