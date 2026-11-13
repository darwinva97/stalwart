#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 bezenti
#
# SPDX-License-Identifier: AGPL-3.0-only
#
# Pruebas de la edición bezenti (las lanza la CI). Cada fase añade aquí las suyas.
#
# Variables: STALWART_BIN = binario ya compilado con las features `sqlite` y `bezenti` (la CI usa el de release);
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

echo "### Fase 2: pruebas unitarias del multi-inquilino (crate common, feature bezenti)"
cargo test "${JOBS[@]}" -p common --features bezenti --lib bezenti::

echo "### Fase 2: integración multi-inquilino contra el servidor real (SQLite, JMAP, SMTP, IMAP, HTTP)"
BIN="${STALWART_BIN:-}"
if [ -z "$BIN" ]; then
  if [ -x target/release/stalwart ]; then
    BIN=target/release/stalwart
  else
    cargo build "${JOBS[@]}" --manifest-path crates/main/Cargo.toml --no-default-features --features "sqlite bezenti"
    BIN=target/debug/stalwart
  fi
fi
python3 -I bezenti/pruebas/multi_inquilino.py "$BIN"

echo "### Fase 2: sin la feature bezenti, los crates tocados siguen compilando (comportamiento libre)"
cargo check "${JOBS[@]}" -p store -p common -p email -p imap -p dav -p jmap -p http@1.1.0
