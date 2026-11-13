#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 bezenti
#
# SPDX-License-Identifier: AGPL-3.0-only
"""Conformidad SCIM con una herramienta pública: `scim2-tester` (python-scim, Apache-2.0).

Levanta el servidor real (SQLite) con un dominio abierto a SCIM y pasa `scim2_tester.check_server` contra
`/scim/v2` con el token de portador del operador. La herramienta no viene en el repositorio: se le pasa un
directorio con `scim2-tester[httpx2]` instalado (`pip install --target DIR 'scim2-tester[httpx2]'`).

Una adaptación, la misma que hay que hacer en cualquier IdP: el servidor exige que `userName` y las direcciones
de `emails` sean direcciones de un dominio abierto a SCIM, así que los valores aleatorios de esos atributos se
generan en ese dominio (`…@conformidad.test`); `locale` y `timezone`, con valores válidos. Todo lo demás lo genera
y lo comprueba la herramienta.

Uso: scim_conformidad.py RUTA_AL_BINARIO_STALWART DIRECTORIO_CON_SCIM2_TESTER
Termina con código 0 si ninguna comprobación da ERROR (se aceptan DEVIATION, documentadas en la salida).
"""

import os
import shutil
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import multi_inquilino as mi  # noqa: E402
from multi_inquilino import ADMIN_PW, PW, Jmap, Servidor  # noqa: E402

# Comprobaciones que fallan por una decisión documentada (especificación scim.md), no por un error.
ESPERADAS = [
    # `active` siempre tiene valor (una cuenta puede o no entrar): «quitarlo» lo deja en true y se sigue devolviendo.
    ("check_remove_attribute", "attribute 'active'"),
    # La dirección principal sale de userName: no se quita por `emails` y siempre está en la lista, así que tras un
    # `add` la lista tiene la principal además de lo añadido (documentación pública).
    ("check_remove_attribute", "The primary address cannot be removed"),
    ("check_add_attribute", "unexpected value for 'emails'"),
    # `locale`/`preferredLanguage` siempre tienen valor: «quitarlos» los deja en en-US, que se sigue devolviendo.
    ("check_remove_attribute", "attribute 'preferredLanguage'"),
    ("check_remove_attribute", "attribute 'locale'"),
]


def main():
    if len(sys.argv) != 3:
        raise SystemExit(__doc__)
    sys.path.insert(0, os.path.abspath(sys.argv[2]))
    import httpx2  # noqa: E402
    import scim2_tester  # noqa: E402
    import scim2_tester.filling as relleno  # noqa: E402
    from scim2_client.engines.httpx2 import SyncSCIMClient  # noqa: E402

    original = relleno.get_random_example_value

    def ejemplo(context, path):
        ruta = str(path)
        binding = path.resolve()
        modelo = getattr(getattr(binding, "target_model", None), "__name__", "") or ""
        campo = getattr(binding, "target_field_name", None)
        es_email = campo == "value" and "email" in modelo.lower()
        if ruta.endswith("userName") or ruta.endswith("emails.value") or es_email:
            return f"t{context.random_uuid().hex[:16]}@conformidad.test"
        if ruta.endswith("locale") or ruta.endswith("preferredLanguage"):
            return "en-US"
        if ruta.endswith("timezone"):
            return "Europe/Lisbon"
        return original(context, path)

    relleno.get_random_example_value = ejemplo

    srv = Servidor(os.path.abspath(sys.argv[1]))
    errores = []
    try:
        mi.configurar(srv)
        base = f"http://127.0.0.1:{srv.p_http}"
        rec = Jmap(base, "admin", ADMIN_PW)
        dom = rec.crear("x:Domain", {"name": "conformidad.test", "allowScimProvisioning": True,
                                     "dkimManagement": {"@type": "Manual"}})
        rec.crear("x:Account", {"@type": "User", "name": "root", "domainId": dom, "roles": {"@type": "Admin"},
                                "credentials": {"0": {"@type": "Password", "secret": PW}}})
        root = Jmap(base, "root@conformidad.test", PW)
        _, r = root.set("x:ApiKey", create={"k": {"description": "conformidad", "permissions": {"@type": "Inherit"}}})
        token = r["created"]["k"]["secret"]

        cliente = SyncSCIMClient(httpx2.Client(base_url=f"{base}/scim/v2", headers={"Authorization": f"Bearer {token}"}))
        cliente.discover()
        resultados = scim2_tester.check_server(cliente, seed=7)
        cuenta = {}
        for res in resultados:
            cuenta[res.status.name] = cuenta.get(res.status.name, 0) + 1
            if res.status.name in ("ERROR", "DEVIATION"):
                linea = f"{res.status.name:9} [{res.resource_type or '-'}] {res.title}: {res.reason}"
                print(linea)
                esperada = any(res.title == t and r in (res.reason or "") for t, r in ESPERADAS)
                if res.status.name == "ERROR" and not esperada:
                    errores.append(linea)
                elif esperada:
                    print("          (esperado: decisión documentada en scim.md)")
        print("\nresumen:", ", ".join(f"{k} {v}" for k, v in sorted(cuenta.items())), f"(total {len(resultados)})")
    finally:
        srv.parar()
        if errores:
            destino = os.path.join(tempfile.gettempdir(), "bezenti-scim-conformidad.log")
            shutil.copy(srv.log, destino)
            print(f"registro del servidor en {destino}")
        shutil.rmtree(srv.dir, ignore_errors=True)
    if errores:
        print(f"\n{len(errores)} comprobaciones con ERROR")
        sys.exit(1)
    print("sin errores de conformidad")


if __name__ == "__main__":
    main()
