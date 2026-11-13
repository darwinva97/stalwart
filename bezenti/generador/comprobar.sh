#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 bezenti
#
# SPDX-License-Identifier: AGPL-3.0-only
#
# Prueba de fidelidad del generador del registro.
#
#  1. Saca los seis archivos generados de la rama limpia (por defecto `limpio-upstream`, que es Stalwart sin
#     código SEL) y vuelve a extraer de ellos el perfil: debe ser idéntico a bezenti/generador/perfil.json.
#  2. Genera en modo fidelidad (esquema completo, filtrando lo de pago) y compara línea a línea con esos
#     archivos quitando solo las líneas de marca "[fragmento SEL eliminado: N líneas]".
#  3. Comprueba que los seis archivos de crates/registry/src/schema/ son la salida actual del generador en modo
#     bezenti (nadie los ha editado a mano ni se ha olvidado de regenerarlos).
#
# Uso: bezenti/generador/comprobar.sh        (REF=<rama o commit> para usar otra referencia)
set -euo pipefail
cd "$(dirname "$0")/../.."
G=bezenti/generador
REF="${REF:-limpio-upstream}"
command -v rustfmt >/dev/null || { echo "falta rustfmt (rustup component add rustfmt)"; exit 1; }

if ! git rev-parse -q --verify "$REF^{commit}" >/dev/null; then
  if git rev-parse -q --verify "origin/$REF^{commit}" >/dev/null; then
    REF="origin/$REF"
  else
    git fetch -q --depth=1 origin "$REF:refs/remotes/origin/$REF"
    REF="origin/$REF"
  fi
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/ref" "$TMP/fid" "$TMP/bez"
ARCHIVOS="enums.rs enums_impl.rs properties.rs properties_impl.rs structs.rs structs_impl.rs"
for f in $ARCHIVOS; do
  git show "$REF:crates/registry/src/schema/$f" > "$TMP/ref/$f"
done

echo "== 1. perfil extraído de $REF"
python3 -I $G/perfil.py "$TMP/ref" resources/schema/schema.json.gz "$TMP/perfil.json" >/dev/null
if cmp -s "$TMP/perfil.json" $G/perfil.json; then
  echo "perfil.json al día"
else
  echo "ERROR: perfil.json no coincide con el extraído de $REF (regenera con perfil.py)"; exit 1
fi

echo "== 2. fidelidad (modo fidelidad frente a $REF sin las marcas SEL)"
python3 -I $G/generar.py --modo fidelidad --salida "$TMP/fid" >/dev/null
total=0; distintas=0
for f in $ARCHIVOS; do
  python3 -I -c 'import re,sys; t=open(sys.argv[1],encoding="utf-8").read(); print("\n".join(l for l in t.split("\n") if not re.match(r"^\s*// \[(fragmento|archivo) SEL eliminado: \d+ líneas\]\s*$", l)), end="")' "$TMP/ref/$f" > "$TMP/ref/$f.limpio"
  n=$(wc -l < "$TMP/ref/$f.limpio")
  d=$(diff "$TMP/ref/$f.limpio" "$TMP/fid/$f" | grep -c '^[<>]' || true)
  marcas=$(grep -c 'SEL eliminado' "$TMP/ref/$f" || true)
  printf "%-20s %6d líneas, %4d marcas SEL, %d líneas distintas\n" "$f" "$n" "$marcas" "$d"
  total=$((total + n)); distintas=$((distintas + d))
done
echo "total: $total líneas comparadas, $distintas distintas"
[ "$distintas" -eq 0 ] || { echo "ERROR: la salida en modo fidelidad no coincide"; exit 1; }

echo "== 3. los archivos del crate son la salida del generador (modo bezenti)"
cp $G/asignaciones.json "$TMP/asig.json"
python3 -I $G/generar.py --modo bezenti --salida "$TMP/bez" --asignaciones "$TMP/asig.json" >/dev/null
cmp -s "$TMP/asig.json" $G/asignaciones.json || { echo "ERROR: hay identificadores de pago sin asignar (regenera)"; exit 1; }
for f in $ARCHIVOS; do
  cmp -s "$TMP/bez/$f" crates/registry/src/schema/$f || { echo "ERROR: crates/registry/src/schema/$f no es la salida del generador"; exit 1; }
done
echo "al día"
echo "fidelidad: OK"
