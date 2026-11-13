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
    cargo build "${JOBS[@]}" --manifest-path crates/main/Cargo.toml --no-default-features --features "sqlite bezenti"
    BIN=target/debug/stalwart
  fi
fi
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

echo "### Fases 2 a 5: sin la feature bezenti, los crates tocados siguen compilando (comportamiento libre)"
cargo check "${JOBS[@]}" -p store -p common -p email -p imap -p dav -p services -p jmap -p http@1.1.0 -p trc -p spam-filter -p smtp
