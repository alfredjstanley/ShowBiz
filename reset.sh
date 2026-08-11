#!/bin/sh
# Wipes the databases. Both are recreated, migrated and re-seeded the next time you start.
#
# Run this when:
#   * you edited a migration you had already applied (applied migrations are checksummed, so
#     the app refuses to start otherwise),
#   * you want a clean slate,
#   * the seeded show schedule has gone stale — show times are generated relative to when the
#     migration ran, so a database left sitting for over a week has shows in the past.
#
# Nothing here is precious: it is seeded reference data plus whatever you booked while testing.

set -eu

cd "$(dirname "$0")"

if [ -d data ]; then
    rm -rf data
    printf 'removed data/ — it will be recreated and re-seeded on next start\n'
else
    printf 'data/ does not exist; nothing to do\n'
fi
