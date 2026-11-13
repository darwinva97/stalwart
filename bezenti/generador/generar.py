#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 bezenti
#
# SPDX-License-Identifier: AGPL-3.0-only
"""Generador del registro de la edición bezenti de Stalwart.

Lee el esquema de la API (`resources/schema/schema.json.gz`) y el perfil extraído del código AGPL
(`perfil.json`, ver perfil.py) y escribe los seis archivos de `crates/registry/src/schema/`.

Modos:
  bezenti    (por defecto) todo el esquema; lo que no existe en el código libre (lo «de pago») va tras
             `#[cfg(feature = "bezenti")]`. Se omiten `x:Enterprise` y lo de licencias.
  fidelidad  solo lo libre y con la cabecera original: debe coincidir línea a línea con los archivos de la
             rama limpia salvo las marcas de la sala limpia (lo comprueba comprobar.sh).

Uso:
  generar.py [--modo bezenti|fidelidad] [--esquema RUTA] [--perfil RUTA] [--asignaciones RUTA] [--salida DIR]
"""

import argparse
import os
import sys

AQUI = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, AQUI)

from comun import ARCHIVOS, cargar_json, rustfmt  # noqa: E402
from modelo import Asignador, Modelo  # noqa: E402
import gen_enums  # noqa: E402
import gen_propiedades  # noqa: E402
import gen_structs  # noqa: E402

RAIZ = os.path.abspath(os.path.join(AQUI, "..", ".."))

CABECERA_ORIGINAL = """/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

// This file is auto-generated. Do not edit directly.
"""

CABECERA_BEZENTI = """/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

// Archivo generado por bezenti/generador a partir del esquema resources/schema/schema.json.gz.
// No se edita a mano: para cambiarlo, cambia el generador o el esquema y vuelve a generarlo.
// Lo que va tras `#[cfg(feature = "bezenti")]` es el modelo de datos de las funciones Enterprise que la
// edición bezenti reimplementa como software libre (AGPL-3.0-only).
"""


ABRE = "// @@PAGO<"
CIERRA = "// @@PAGO>"


class Pago:
    def __init__(self, w, pago, attr, bloque=False):
        self.w, self.pago, self.attr, self.bloque = w, pago, attr, bloque

    def __enter__(self):
        if self.pago:
            if self.w.m.bezenti:
                if self.attr:
                    self.w.l(self.attr)
                if self.bloque:
                    self.w.l("{")
            else:
                self.w.l(ABRE)

    def __exit__(self, *_):
        if self.pago:
            if not self.w.m.bezenti:
                self.w.l(CIERRA)
            elif self.bloque:
                self.w.l("}")


class Escritor:
    """Acumula líneas de código. Lo de pago se escribe con `with w.pago(p): ...`:
    - en modo bezenti, precedido de `#[cfg(feature = "bezenti")]` (o del atributo indicado);
    - en modo fidelidad, entre centinelas que se quitan DESPUÉS de dar formato (así el formato del resto es el
      mismo que tenía el código original, que se formateó con lo de pago dentro)."""

    def __init__(self, m):
        self.m = m
        self.lineas = []

    def l(self, s):
        self.lineas.append(s)

    def pago(self, pago, attr=True, bloque=False):
        """`bloque=True`: varias sentencias; en modo bezenti van dentro de un bloque `{ }` con el cfg."""
        if attr is True:
            attr = self.m.cfg
        return Pago(self, pago, attr, bloque)

    def texto(self):
        """Sangra cada línea según el anidamiento de llaves, corchetes y paréntesis (rustfmt no reformatea
        las expresiones con líneas demasiado largas y las deja como vienen)."""
        salida, nivel = [], 0
        for linea in self.lineas:
            t = linea.strip()
            if not t:
                salida.append("")
                continue
            abre, cierra_ini = balance(t)
            nivel_linea = nivel - cierra_ini
            salida.append("    " * max(nivel_linea, 0) + t)
            nivel += abre
        return "\n".join(salida) + "\n"


def balance(t):
    """(saldo de aperturas de la línea, cierres al principio de la línea), sin contar cadenas ni comentarios."""
    if t.startswith("//"):
        return 0, 0
    saldo, ini, en_cadena, escape, principio = 0, 0, False, False, True
    for ch in t:
        if en_cadena:
            if escape:
                escape = False
            elif ch == "\\":
                escape = True
            elif ch == '"':
                en_cadena = False
            continue
        if ch == '"':
            en_cadena = True
            principio = False
        elif ch in "{[(":
            saldo += 1
            principio = False
        elif ch in "}])":
            saldo -= 1
            if principio:
                ini += 1
        elif ch not in " ,;.":
            principio = False
    return saldo, ini


def poner_huecos(texto, huecos):
    """Modo fidelidad: deja la línea en blanco que el código de pago desconocido dejó al final de algunos
    bloques `impl` del código limpio (ver perfil.extraer_huecos)."""
    lineas = texto.split("\n")
    salida, dentro, vistas = [], False, {}
    for linea in lineas:
        if linea.startswith("impl"):
            vistas[linea] = vistas.get(linea, 0) + 1
            if f"{vistas[linea]}:{linea}" in huecos:
                dentro = True
        elif dentro and linea == "}":
            salida.append("")
            dentro = False
        salida.append(linea)
    return "\n".join(salida)


def quitar_pago(texto):
    """Quita los bloques entre centinelas (modo fidelidad)."""
    salida, nivel = [], 0
    for linea in texto.split("\n"):
        t = linea.strip()
        if t == ABRE:
            nivel += 1
            continue
        if t == CIERRA:
            nivel -= 1
            continue
        if nivel == 0:
            salida.append(linea)
    assert nivel == 0, "centinelas de pago desequilibrados"
    return "\n".join(salida)


GENERADORES = {
    "enums.rs": gen_enums.enums_rs,
    "enums_impl.rs": gen_enums.enums_impl_rs,
    "properties.rs": gen_propiedades.properties_rs,
    "properties_impl.rs": gen_propiedades.properties_impl_rs,
    "structs.rs": gen_structs.structs_rs,
    "structs_impl.rs": gen_structs.structs_impl_rs,
}


def generar(m, nombre):
    w = Escritor(m)
    GENERADORES[nombre](m, w)
    cabecera = CABECERA_BEZENTI if m.bezenti else CABECERA_ORIGINAL
    cuerpo = rustfmt(w.texto())
    if not m.bezenti:
        cuerpo = quitar_pago(cuerpo)
        if nombre == "structs_impl.rs":
            cuerpo = poner_huecos(cuerpo, set(m.p["huecos"]))
    return cabecera + "\n" + cuerpo


def aplicar_extensiones(esquema, ext):
    """Añade al esquema los campos propios de la edición bezenti (extensiones.json), solo en modo bezenti.

    Son campos que el esquema oficial no tiene (p. ej. `x:Tenant.bezentiParentTenantId`, la jerarquía de
    revendedores). Van marcados como de pago, así que se generan tras `#[cfg(feature = "bezenti")]` con sus
    identificadores en asignaciones.json, como cualquier otro campo de pago."""
    for objeto, campos in ext.get("campos", {}).items():
        props = esquema["fields"][objeto]["properties"]
        for nombre, definicion in campos.items():
            if nombre in props:
                raise SystemExit(f"extensión {objeto}.{nombre}: el esquema oficial ya tiene ese campo")
            props[nombre] = definicion


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--modo", default="bezenti", choices=["bezenti", "fidelidad"])
    ap.add_argument("--esquema", default=os.path.join(RAIZ, "resources/schema/schema.json.gz"))
    ap.add_argument("--perfil", default=os.path.join(AQUI, "perfil.json"))
    ap.add_argument("--decisiones", default=os.path.join(AQUI, "decisiones.json"))
    ap.add_argument("--asignaciones", default=os.path.join(AQUI, "asignaciones.json"))
    ap.add_argument("--extensiones", default=os.path.join(AQUI, "extensiones.json"))
    ap.add_argument("--salida", default=os.path.join(RAIZ, "crates/registry/src/schema"))
    ap.add_argument("--solo", action="append", help="genera solo este archivo (se puede repetir)")
    a = ap.parse_args()
    esquema = cargar_json(a.esquema)
    if a.modo == "bezenti":
        aplicar_extensiones(esquema, cargar_json(a.extensiones))
    perfil = cargar_json(a.perfil)
    asig = Asignador(a.asignaciones, guardar=a.modo == "bezenti")
    decisiones = cargar_json(a.decisiones)
    m = Modelo(esquema, perfil, a.modo, asig, decisiones)
    os.makedirs(a.salida, exist_ok=True)
    for nombre in ARCHIVOS:
        if a.solo and nombre not in a.solo:
            continue
        if nombre not in GENERADORES:
            continue
        texto = generar(m, nombre)
        with open(os.path.join(a.salida, nombre), "w", encoding="utf-8") as f:
            f.write(texto)
        print(f"{nombre}: {texto.count(chr(10))} líneas")
    asig.guardar()


if __name__ == "__main__":
    main()
