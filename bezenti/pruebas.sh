#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 bezenti
#
# SPDX-License-Identifier: AGPL-3.0-only
#
# Pruebas de la edición bezenti (las lanza la CI). Cada fase añade aquí las suyas.
#
# Variables: STALWART_BIN = binario ya compilado con las features `sqlite`, `postgres` y `bezenti` (la CI usa el de release);
# si no se da y no existe target/release/stalwart, se compila uno de depuración. CARGO_JOBS = trabajos de cargo.
set -euo pipefail
cd "$(dirname "$0")/.."
JOBS=(); [ -n "${CARGO_JOBS:-}" ] && JOBS=(-j "$CARGO_JOBS")

echo "### Fase 1: prueba de fidelidad del generador del registro (y archivos generados de la fase 2)"
bash bezenti/generador/comprobar.sh

echo "### Fase 1: pruebas del modelo de datos de pago (crate registry, feature bezenti)"
cargo test "${JOBS[@]}" -p registry --features bezenti --lib bezenti::

echo "### Fase 1: el crate registry sigue compilando y pasando sus pruebas sin la feature"
cargo test "${JOBS[@]}" -p registry --lib

echo "### Fases 2, 3 y 5: pruebas unitarias (multi-inquilino, enmascaradas, archivo y telemetría; feature bezenti)"
cargo test "${JOBS[@]}" -p common --features bezenti --lib bezenti::
cargo test "${JOBS[@]}" -p jmap --features bezenti --lib registry::telemetria
cargo test "${JOBS[@]}" -p services --features bezenti --lib bezenti::telemetria

echo "### Fase 2: integración multi-inquilino contra el servidor real (SQLite, JMAP, SMTP, IMAP, HTTP)"
BIN="${STALWART_BIN:-}"
if [ -z "$BIN" ]; then
  if [ -x target/release/stalwart ]; then
    BIN=target/release/stalwart
  else
    cargo build "${JOBS[@]}" --manifest-path crates/main/Cargo.toml --no-default-features --features "sqlite postgres bezenti"
    BIN=target/debug/stalwart
  fi
fi
# En un binario de depuración los futuros son mucho más grandes: con PostgreSQL y Redis compilados, algunas
# peticiones no caben en la pila de 2 MiB de los hilos de tokio (en release sí). Solo para depuración.
case "$BIN" in */debug/*) export RUST_MIN_STACK="${RUST_MIN_STACK:-16777216}" ;; esac
python3 -I bezenti/pruebas/multi_inquilino.py "$BIN"

echo "### Fase 3: integración de direcciones enmascaradas y archivo de borrados (SQLite, JMAP, SMTP, IMAP)"
python3 -I bezenti/pruebas/archivo_enmascaradas.py "$BIN"

echo "### Fase 4: pruebas unitarias de la IA (eventos/métricas/claves en trc, filtro antispam con IA)"
cargo test "${JOBS[@]}" -p trc --features bezenti --lib ia::
cargo test "${JOBS[@]}" -p spam-filter --features bezenti --lib bezenti::

echo "### Fase 4: integración IA contra el servidor real (proveedores falsos: OpenAI, Anthropic, Gemini, Ollama, TypeSafe)"
python3 -I bezenti/pruebas/ia.py "$BIN"

echo "### Fase 5: integración de la telemetría con historial y alertas (SQLite, JMAP, SMTP, HTTP/SSE, webhook)"
python3 -I bezenti/pruebas/telemetria.py "$BIN"

echo "### Fase 6: PostgreSQL primario + réplica física reales (sin root) para las pruebas de réplicas"
PGDIR=""
if bash bezenti/pruebas/postgres_replica.sh arrancar "$(mktemp -d)/pg" > /tmp/bezenti-pg.txt 2>&1; then
  cat /tmp/bezenti-pg.txt
  PGDIR=$(sed -n 's/.*(\(.*\))$/\1/p' /tmp/bezenti-pg.txt)
  trap 'bash bezenti/pruebas/postgres_replica.sh parar "$PGDIR"' EXIT
elif [ -n "${CI:-}" ]; then
  cat /tmp/bezenti-pg.txt; echo "en la CI hacen falta los binarios de PostgreSQL"; exit 1
else
  cat /tmp/bezenti-pg.txt; echo "sin PostgreSQL local: se omiten las pruebas con réplicas reales"
fi

echo "### Fase 6: pruebas unitarias y de integración del almacén (réplicas, reparto de blobs y de memoria)"
(
  [ -n "$PGDIR" ] && . "$PGDIR/entorno.sh"
  cargo test "${JOBS[@]}" -p store --features "postgres sqlite bezenti" --lib backend::bezenti -- --nocapture
)
echo "### Fase 6: el código de réplicas de MySQL/MariaDB compila"
cargo check "${JOBS[@]}" -p store --features "mysql bezenti"

echo "### Fase 6: integración con el servidor real (blobs repartidos; datos en PostgreSQL con réplica)"
if [ -n "$PGDIR" ]; then
  python3 -I bezenti/pruebas/replicas_reparto.py "$BIN" --pg "$PGDIR"
else
  python3 -I bezenti/pruebas/replicas_reparto.py "$BIN"
fi

echo "### Fases 2 a 6: sin la feature bezenti, los crates tocados siguen compilando (comportamiento libre)"
cargo check "${JOBS[@]}" -p store -p common -p email -p imap -p dav -p services -p jmap -p http@1.1.0 -p trc -p spam-filter -p smtp -p coordinator -p directory
cargo check "${JOBS[@]}" -p store --features "postgres mysql redis s3"
