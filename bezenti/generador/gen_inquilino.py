#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 bezenti
#
# SPDX-License-Identifier: AGPL-3.0-only
"""Genera crates/registry/src/bezenti/inquilino_gen.rs (fase 2, multi-inquilino).

Accesores del campo `memberTenantId` sobre `ObjectInner`: leerlo y escribirlo sin saber de qué tipo es el
objeto. El esquema pone `memberTenantId` en ~60 structs (cuentas, dominios, roles, los 45 proveedores DNS…);
escribir esos `match` a mano sería frágil, así que salen del esquema igual que el resto del registro.

Uso: gen_inquilino.py [--salida RUTA]   (por defecto escribe en el crate; comprobar.sh verifica que está al día)
"""

import argparse
import os
import sys

AQUI = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, AQUI)

from comun import cargar_json, rustfmt  # noqa: E402
from modelo import Asignador, Modelo  # noqa: E402

RAIZ = os.path.abspath(os.path.join(AQUI, "..", ".."))

CABECERA = """/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

// Archivo generado por bezenti/generador/gen_inquilino.py a partir del esquema. No se edita a mano.
"""


def tiene_tenant(m, esquema):
    f = m.e["fields"].get(esquema)
    return bool(f) and "memberTenantId" in f["properties"]


def generar(m):
    lineas = ["use crate::schema::prelude::*;", "", "impl ObjectInner {"]
    for modo in ("get", "set"):
        if modo == "get":
            lineas += [
                "/// Inquilino al que pertenece el objeto (`memberTenantId`), o `None` si no tiene o no es de un inquilino.",
                "pub fn member_tenant_id(&self) -> Option<Id> {",
            ]
        else:
            lineas += [
                "/// Cambia el inquilino del objeto. Devuelve `false` si este tipo de objeto no tiene `memberTenantId`.",
                "pub fn set_member_tenant_id(&mut self, tenant_id: Option<Id>) -> bool {",
            ]
        lineas.append("match self {")
        for k, r, _, _ in m.objetos():
            esq = m.e["schemas"][k]
            if esq["type"] == "single":
                if not tiene_tenant(m, esq["schemaName"]):
                    continue
                if modo == "get":
                    lineas.append(f"ObjectInner::{r}(obj) => obj.member_tenant_id,")
                else:
                    lineas.append(f"ObjectInner::{r}(obj) => {{ obj.member_tenant_id = tenant_id; true }}")
            else:
                brazos, total = [], 0
                for v in esq["variants"]:
                    total += 1
                    if "schemaName" in v and tiene_tenant(m, v["schemaName"]):
                        if modo == "get":
                            brazos.append(f"{r}::{v['name']}(obj) => obj.member_tenant_id,")
                        else:
                            brazos.append(f"{r}::{v['name']}(obj) => {{ obj.member_tenant_id = tenant_id; true }}")
                if not brazos:
                    continue
                acceso = "as_ref" if modo == "get" else "as_mut"
                lineas.append(f"ObjectInner::{r}(inner) => match inner.{acceso}() {{")
                lineas += brazos
                if len(brazos) < total:
                    lineas.append("_ => None," if modo == "get" else "_ => false,")
                lineas.append("},")
        lineas.append("_ => None," if modo == "get" else "_ => false,")
        lineas += ["}", "}", ""]
    lineas.pop()
    lineas.append("}")
    return CABECERA + "\n" + rustfmt("\n".join(lineas) + "\n")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--salida", default=os.path.join(RAIZ, "crates/registry/src/bezenti/inquilino_gen.rs"))
    a = ap.parse_args()
    esquema = cargar_json(os.path.join(RAIZ, "resources/schema/schema.json.gz"))
    m = Modelo(esquema, cargar_json(os.path.join(AQUI, "perfil.json")), "bezenti",
               Asignador(os.path.join(AQUI, "asignaciones.json"), guardar=False),
               cargar_json(os.path.join(AQUI, "decisiones.json")))
    texto = generar(m)
    with open(a.salida, "w", encoding="utf-8") as f:
        f.write(texto)
    print(f"{os.path.relpath(a.salida, RAIZ)}: {texto.count(chr(10))} líneas")


if __name__ == "__main__":
    main()
