#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 bezenti
#
# SPDX-License-Identifier: AGPL-3.0-only
#
# Pruebas de la edición bezenti (las lanza la CI). Cada fase añade aquí las suyas.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "### Fase 1: prueba de fidelidad del generador del registro"
bash bezenti/generador/comprobar.sh

echo "### Fase 1: pruebas del modelo de datos de pago (crate registry, feature bezenti)"
cargo test -p registry --features bezenti --lib bezenti::

echo "### Fase 1: el crate registry sigue compilando y pasando sus pruebas sin la feature"
cargo test -p registry --lib
