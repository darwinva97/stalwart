#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 bezenti
#
# SPDX-License-Identifier: AGPL-3.0-only
#
# Levanta un MariaDB (o MySQL) primario y una réplica con GTID SIN root, con los binarios del sistema
# (`mariadb-install-db`/`mysqld`, `mariadbd`). Para las pruebas de la fase 6 (réplicas de lectura en MySQL/MariaDB).
#
#   mariadb_replica.sh arrancar DIR    crea y arranca los dos servidores en DIR; escribe DIR/entorno.sh
#   mariadb_replica.sh parar DIR       los para
#
# DIR/entorno.sh exporta BEZENTI_MY_PUERTO_PRIMARIO, BEZENTI_MY_PUERTO_REPLICA y BEZENTI_MY_DIR (con DIR/replica.sh
# para parar, arrancar, pausar y reanudar la réplica), que lee crates/store/src/backend/bezenti/pruebas_mysql.rs.
set -euo pipefail

orden="${1:-}"; dir="${2:-}"
[ -n "$orden" ] && [ -n "$dir" ] || { sed -n '6,13p' "$0"; exit 2; }

servidor=$(command -v mariadbd || ls /usr/sbin/mariadbd 2>/dev/null || command -v mysqld || true)
cliente=$(command -v mariadb || command -v mysql || true)
instalar=$(command -v mariadb-install-db || command -v mysql_install_db || true)
[ -n "$servidor" ] && [ -n "$cliente" ] && [ -n "$instalar" ] || { echo "no encuentro MariaDB"; exit 3; }

puerto_libre() {
  python3 -I -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'
}

arrancar_uno() { # nombre puerto id
  "$servidor" --no-defaults --datadir="$dir/$1" --port="$2" --bind-address=127.0.0.1 --socket="$dir/$1.sock" \
    --pid-file="$dir/$1.pid" --log-bin="$dir/$1/bin" --server-id="$3" --log-error="$dir/$1.err" \
    --skip-name-resolve --innodb-flush-log-at-trx-commit=2 >/dev/null 2>&1 &
  for _ in $(seq 1 100); do
    "$cliente" --no-defaults -S "$dir/$1.sock" -u root -e "SELECT 1" >/dev/null 2>&1 && return 0
    sleep 0.2
  done
  echo "MariaDB $1 no arrancó"; cat "$dir/$1.err"; exit 4
}

case "$orden" in
  arrancar)
    mkdir -p "$dir"; dir=$(cd "$dir" && pwd)
    p1=$(puerto_libre); p2=$(puerto_libre)
    for n in primario replica; do
      "$instalar" --no-defaults --datadir="$dir/$n" --auth-root-authentication-method=normal --skip-test-db \
        >"$dir/$n-install.log" 2>&1
    done
    arrancar_uno primario "$p1" 1
    "$cliente" --no-defaults -S "$dir/primario.sock" -u root -e "
      CREATE USER 'stalwart'@'%'; GRANT ALL ON *.* TO 'stalwart'@'%' WITH GRANT OPTION;
      CREATE DATABASE stalwart;"
    arrancar_uno replica "$p2" 2
    "$cliente" --no-defaults -S "$dir/replica.sock" -u root -e "
      CHANGE MASTER TO MASTER_HOST='127.0.0.1', MASTER_PORT=$p1, MASTER_USER='stalwart', MASTER_USE_GTID=slave_pos;
      START SLAVE; SET GLOBAL read_only = 1;"
    for _ in $(seq 1 50); do
      r=$("$cliente" --no-defaults -S "$dir/replica.sock" -u root -N -e "SHOW DATABASES LIKE 'stalwart'" 2>/dev/null || true)
      [ "$r" = "stalwart" ] && break; sleep 0.2
    done
    [ "$r" = "stalwart" ] || { echo "la réplica no replica"; exit 5; }
    cat >"$dir/replica.sh" <<EOF
#!/usr/bin/env bash
# parar | arrancar | pausar | reanudar la réplica
case "\$1" in
  parar) "$cliente" --no-defaults -S "$dir/replica.sock" -u root -e "SHUTDOWN" ;;
  arrancar) "$servidor" --no-defaults --datadir="$dir/replica" --port=$p2 --bind-address=127.0.0.1 \\
      --socket="$dir/replica.sock" --pid-file="$dir/replica.pid" --log-bin="$dir/replica/bin" --server-id=2 \\
      --log-error="$dir/replica.err" --skip-name-resolve >/dev/null 2>&1 &
    for _ in \$(seq 1 100); do
      "$cliente" --no-defaults -S "$dir/replica.sock" -u root -e "SELECT 1" >/dev/null 2>&1 && exit 0; sleep 0.2
    done; exit 1 ;;
  pausar) "$cliente" --no-defaults -S "$dir/replica.sock" -u root -e "STOP SLAVE SQL_THREAD" ;;
  reanudar) "$cliente" --no-defaults -S "$dir/replica.sock" -u root -e "START SLAVE SQL_THREAD" ;;
esac
EOF
    chmod +x "$dir/replica.sh"
    cat >"$dir/entorno.sh" <<EOF
export BEZENTI_MY_PUERTO_PRIMARIO=$p1
export BEZENTI_MY_PUERTO_REPLICA=$p2
export BEZENTI_MY_DIR=$dir
EOF
    echo "MariaDB primario en 127.0.0.1:$p1, réplica en 127.0.0.1:$p2 ($dir)"
    ;;
  parar)
    for n in replica primario; do
      [ -S "$dir/$n.sock" ] && "$cliente" --no-defaults -S "$dir/$n.sock" -u root -e "SHUTDOWN" >/dev/null 2>&1 || true
    done
    ;;
  *) echo "orden desconocida: $orden"; exit 2 ;;
esac
