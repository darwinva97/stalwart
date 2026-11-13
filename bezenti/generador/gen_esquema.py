#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 bezenti
#
# SPDX-License-Identifier: AGPL-3.0-only
"""Genera el esquema de la API que sirve la edición bezenti (`resources/schema/schema_bezenti.json.gz`).

El servidor sirve el esquema en `GET /api/schema/<hash>` y la interfaz web lo usa para pintar listas y
formularios. La edición libre sirve `schema_oss.json.gz` (sin inquilinos ni `memberTenantId`), así que el panel no
mostraría lo que implementa esta edición. Este programa parte del esquema completo y:

  - quita `x:Enterprise` (licencias) y sus permisos `sysEnterprise*`, que esta edición no implementa;
  - añade las extensiones propias de `extensiones.json` (p. ej. `x:Tenant.bezentiParentTenantId`) y las pone en
    el formulario del objeto, para que el panel las muestre.

El archivo se escribe de forma reproducible (gzip sin fecha) y su hash va en `schema_bezenti.json.sha256` (base64url
del SHA-256 del .gz, igual que los otros esquemas). `comprobar.sh` verifica que está al día.

Uso: gen_esquema.py [--salida DIR]
"""

import argparse
import base64
import gzip
import hashlib
import io
import json
import os
import sys

AQUI = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, AQUI)

from comun import cargar_json  # noqa: E402
from generar import aplicar_extensiones  # noqa: E402
from modelo import OBJETOS_EXCLUIDOS, PREFIJOS_PERMISO_EXCLUIDOS  # noqa: E402

RAIZ = os.path.abspath(os.path.join(AQUI, "..", ".."))

# Dónde se muestran en los formularios los campos propios: objeto → [(sección, campo, etiqueta)].
FORMULARIOS = {
    "x:Tenant": [("Tenant", "bezentiParentTenantId", "Parent tenant (reseller)")],
}


def quitar_referencias(valor, excluidos):
    """Quita de listas y diccionarios cualquier elemento que apunte a un objeto excluido (enlaces del menú)."""
    if isinstance(valor, dict):
        return {k: quitar_referencias(v, excluidos) for k, v in valor.items()}
    if isinstance(valor, list):
        res = []
        for v in valor:
            if isinstance(v, dict) and any(e in json.dumps(v) for e in excluidos):
                # Contenedores: se conservan quitando solo el enlace; enlaces sueltos: fuera.
                if any(k in v for k in ("container", "items")):
                    res.append(quitar_referencias(v, excluidos))
                continue
            res.append(quitar_referencias(v, excluidos))
        return res
    return valor


def generar(esquema, extensiones):
    e = json.loads(json.dumps(esquema))
    for seccion in ("objects", "schemas", "fields", "forms", "lists"):
        for nombre in OBJETOS_EXCLUIDOS:
            e[seccion].pop(nombre, None)
    e["enums"]["Permission"] = [
        v for v in e["enums"]["Permission"] if not v["name"].startswith(PREFIJOS_PERMISO_EXCLUIDOS)
    ]
    e["layouts"] = quitar_referencias(e["layouts"], OBJETOS_EXCLUIDOS)
    e["dashboards"] = quitar_referencias(e["dashboards"], OBJETOS_EXCLUIDOS)
    aplicar_extensiones(e, extensiones)
    for objeto, campos in FORMULARIOS.items():
        secciones = e["forms"][objeto]["sections"]
        for titulo, campo, etiqueta in campos:
            seccion = next(s for s in secciones if s["title"] == titulo)
            if not any(f["name"] == campo for f in seccion["fields"]):
                seccion["fields"].append({"name": campo, "label": etiqueta})
    texto = json.dumps(e, ensure_ascii=False, sort_keys=False, separators=(",", ":"))
    for nombre in OBJETOS_EXCLUIDOS:
        assert nombre not in texto, f"queda una referencia a {nombre}"
    buf = io.BytesIO()
    with gzip.GzipFile(fileobj=buf, mode="wb", mtime=0, compresslevel=9) as f:
        f.write(texto.encode("utf-8"))
    datos = buf.getvalue()
    huella = base64.urlsafe_b64encode(hashlib.sha256(datos).digest()).decode().rstrip("=")
    return datos, huella


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--salida", default=os.path.join(RAIZ, "resources/schema"))
    a = ap.parse_args()
    datos, huella = generar(
        cargar_json(os.path.join(RAIZ, "resources/schema/schema.json.gz")),
        cargar_json(os.path.join(AQUI, "extensiones.json")),
    )
    with open(os.path.join(a.salida, "schema_bezenti.json.gz"), "wb") as f:
        f.write(datos)
    with open(os.path.join(a.salida, "schema_bezenti.json.sha256"), "w") as f:
        f.write(huella)
    print(f"schema_bezenti.json.gz: {len(datos)} bytes, hash {huella}")


if __name__ == "__main__":
    main()
