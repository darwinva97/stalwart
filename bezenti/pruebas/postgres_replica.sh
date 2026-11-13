#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 bezenti
#
# SPDX-License-Identifier: AGPL-3.0-only
#
# Levanta un PostgreSQL primario y una réplica física (streaming) SIN root, con los binarios del sistema
# (`/usr/lib/postgresql/*/bin`) o los de PG_BIN. Para las pruebas de la fase 6 (réplicas de lectura).
#
#   postgres_replica.sh arrancar DIR    crea y arranca los dos servidores en DIR; escribe DIR/entorno.sh
#   postgres_replica.sh parar DIR       los para (y deja DIR para inspeccionarlo)
#
# DIR/entorno.sh exporta BEZENTI_PG_PUERTO_PRIMARIO, BEZENTI_PG_PUERTO_REPLICA, BEZENTI_PG_CTL, BEZENTI_PG_PSQL,
# BEZENTI_PG_DIR_REPLICA (las que lee crates/store/src/backend/bezenti/pruebas_postgres.rs).
set -euo pipefail

orden="${1:-}"; dir="${2:-}"
[ -n "$orden" ] && [ -n "$dir" ] || { sed -n '6,13p' "$0"; exit 2; }

bin="${PG_BIN:-}"
if [ -z "$bin" ]; then
  bin=$(ls -d /usr/lib/postgresql/*/bin 2>/dev/null | sort -V | tail -1 || true)
fi
[ -x "$bin/initdb" ] || { echo "no encuentro initdb (pon PG_BIN)"; exit 3; }

puerto_libre() {
  python3 -I -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'
}

case "$orden" in
  arrancar)
    mkdir -p "$dir"; dir=$(cd "$dir" && pwd)
    p1=$(puerto_libre); p2=$(puerto_libre)
    "$bin/initdb" -D "$dir/primario" -U stalwart -A trust -E UTF8 --no-instructions >"$dir/initdb.log"
    cat >>"$dir/primario/postgresql.conf" <<EOF
port = $p1
listen_addresses = '127.0.0.1'
unix_socket_directories = '$dir'
wal_level = replica
max_wal_senders = 5
hot_standby = on
fsync = off
synchronous_commit = on
EOF
    "$bin/pg_ctl" -D "$dir/primario" -w -l "$dir/primario.log" start >/dev/null
    "$bin/createdb" -h 127.0.0.1 -p "$p1" -U stalwart stalwart
    "$bin/pg_basebackup" -h 127.0.0.1 -p "$p1" -U stalwart -D "$dir/replica" -R -X stream >/dev/null
    sed -i "s/^port = $p1/port = $p2/" "$dir/replica/postgresql.conf"
    "$bin/pg_ctl" -D "$dir/replica" -w -l "$dir/replica.log" start >/dev/null
    # Esperar a que la réplica esté en recuperación y recibiendo.
    for _ in $(seq 1 50); do
      r=$("$bin/psql" -h 127.0.0.1 -p "$p2" -U stalwart -d stalwart -qAt -c "SELECT pg_is_in_recovery()" 2>/dev/null || true)
      [ "$r" = "t" ] && break; sleep 0.2
    done
    [ "$r" = "t" ] || { echo "la réplica no arrancó"; cat "$dir/replica.log"; exit 4; }
    cat >"$dir/entorno.sh" <<EOF
export BEZENTI_PG_PUERTO_PRIMARIO=$p1
export BEZENTI_PG_PUERTO_REPLICA=$p2
export BEZENTI_PG_CTL=$bin/pg_ctl
export BEZENTI_PG_PSQL=$bin/psql
export BEZENTI_PG_DIR_REPLICA=$dir/replica
EOF
    echo "primario en 127.0.0.1:$p1, réplica en 127.0.0.1:$p2 ($dir)"
    ;;
  parar)
    for d in replica primario; do
      [ -f "$dir/$d/postmaster.pid" ] && "$bin/pg_ctl" -D "$dir/$d" -m fast stop >/dev/null 2>&1 || true
    done
    ;;
  *) echo "orden desconocida: $orden"; exit 2 ;;
esac
