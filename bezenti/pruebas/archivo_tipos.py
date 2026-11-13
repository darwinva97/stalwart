#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 bezenti
#
# SPDX-License-Identifier: AGPL-3.0-only
"""Pruebas de integración de la fase 8 (2/2): archivo de archivos, eventos, contactos y guiones Sieve.

Levanta el servidor real (binario `stalwart` con `sqlite` y `bezenti`) con una base SQLite nueva y lo prueba por
WebDAV/CalDAV/CardDAV, JMAP y ManageSieve-equivalente (JMAP SieveScript), solo con la biblioteca estándar de
Python. Reutiliza `multi_inquilino.py` y `archivo_enmascaradas.py`.

Escenario (bezenti/especificaciones/archivo.md, §3 y §4): ana@a.test borra un archivo, un evento, un contacto y
un guion Sieve; cada uno aparece en `x:ArchivedItem` con su tipo y sus datos, su `blobId` descarga el contenido
completo, y al restaurarlo vuelve íntegro (mismo contenido, en su carpeta, calendario o libreta, con su nombre).
Borrar una carpeta o un calendario con contenido archiva lo que contenían; mover no archiva nada. Aislamiento:
beto (otro inquilino) y su administrador no ven ni tocan nada de ana; el administrador de A sí (y descarga el
contenido). Purga al vencer también para lo que no es correo.

Uso: archivo_tipos.py RUTA_AL_BINARIO_STALWART
"""

import json
import os
import shutil
import sys
import tempfile
import time
import urllib.parse

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import archivo_enmascaradas as ae  # noqa: E402
import multi_inquilino as mi  # noqa: E402
from multi_inquilino import ADMIN_PW, PW, Jmap, Servidor, ok  # noqa: E402

USING_SIEVE = ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:sieve"]

EVENTO = """BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:-//bezenti//pruebas//ES\r
BEGIN:VEVENT\r
UID:evento-archivo-7d1@a.test\r
DTSTAMP:20261101T090000Z\r
DTSTART:20261201T100000Z\r
DTEND:20261201T110000Z\r
SUMMARY:Reunión de presupuesto\r
DESCRIPTION:Traer las cifras del trimestre\r
LOCATION:Sala 3\r
END:VEVENT\r
END:VCALENDAR\r
"""

CONTACTO = """BEGIN:VCARD\r
VERSION:4.0\r
UID:urn:uuid:4fbe8971-0bc3-424c-9c26-36c3e1eff6b1\r
FN:Marta Proveedora\r
N:Proveedora;Marta;;;\r
EMAIL;TYPE=work:marta@proveedor.example\r
TEL;TYPE=cell:+51 999 888 777\r
NOTE:Contacto de compras\r
END:VCARD\r
"""

GUION = 'require ["fileinto"];\r\nif header :contains "subject" "factura" {\r\n  fileinto "Facturas";\r\n}\r\n'


def dav(usr, metodo, ruta, cuerpo=None, cabeceras=None):
    """Petición WebDAV con el usuario de `usr`. Devuelve (estado, cuerpo)."""
    import urllib.request
    datos = cuerpo.encode() if isinstance(cuerpo, str) else cuerpo
    req = urllib.request.Request(usr.base + ruta, data=datos, method=metodo)
    req.add_header("Authorization", usr.auth)
    for k, v in (cabeceras or {}).items():
        req.add_header(k, v)
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


def ruta(tipo, cuenta, *partes):
    return f"/dav/{tipo}/{urllib.parse.quote(cuenta)}/" + "/".join(partes)


def archivados(usr, filtro=None):
    _, r = ae.archivados(usr, filtro)
    ids = list(r.get("ids") or [])
    if not ids:
        return []
    _, r = usr.get("x:ArchivedItem", ids)
    return r.get("list") or []


def de_tipo(lista, tipo, **campos):
    return next((o for o in lista if o.get("@type") == tipo
                 and all(o.get(k) == v for k, v in campos.items())), None)


def esperar_archivado(usr, tipo, **campos):
    return ae.esperar(lambda: de_tipo(archivados(usr), tipo, **campos), 20)


def restaurar(usr, item_id):
    _, r = usr.set("x:ArchivedItem", update={item_id: {"status": "requestRestore"}})
    return item_id in (r.get("updated") or {})


def lineas(texto, *claves):
    """Las líneas de un iCalendar/vCard que empiezan por alguna de las claves (para comparar sin el orden)."""
    if isinstance(texto, bytes):
        texto = texto.decode("utf-8", "replace")
    return sorted(linea.strip() for linea in texto.replace("\r\n ", "").splitlines()
                  if linea.split(":", 1)[0].split(";", 1)[0].upper() in claves)


def pruebas(srv):
    base = f"http://127.0.0.1:{srv.p_http}"
    rec = Jmap(base, "admin", ADMIN_PW)

    print("== el operador prepara inquilinos y cuentas")
    dom_op = rec.crear("x:Domain", {"name": "operador.test", "dkimManagement": {"@type": "Manual"}})
    rec.crear("x:Account", {"@type": "User", "name": "root", "domainId": dom_op, "roles": {"@type": "Admin"},
                            "credentials": {"0": {"@type": "Password", "secret": PW}}})
    root = Jmap(base, "root@operador.test", PW)
    t_a = root.crear("x:Tenant", {"name": "Empresa A"})
    t_b = root.crear("x:Tenant", {"name": "Empresa B"})
    dom_a = root.crear("x:Domain", {"name": "a.test", "memberTenantId": t_a, "dkimManagement": {"@type": "Manual"}})
    dom_b = root.crear("x:Domain", {"name": "b.test", "memberTenantId": t_b, "dkimManagement": {"@type": "Manual"}})

    def cuenta(nombre, dom, tenant, **extra):
        return root.crear("x:Account", {"@type": "User", "name": nombre, "domainId": dom, "memberTenantId": tenant,
                                        "credentials": {"0": {"@type": "Password", "secret": PW}}, **extra})
    cuenta("admin", dom_a, t_a, roles={"@type": "Admin"})
    cuenta("admin", dom_b, t_b, roles={"@type": "Admin"})
    ana_id = cuenta("ana", dom_a, t_a)
    cuenta("beto", dom_b, t_b)
    # Que el servidor añada sus cabeceras (Received, Return-Path…) también en este puerto de pruebas.
    _, r = root.set("x:MtaStageData", update={"singleton": {
        "addReceivedHeader": {"else": "true"}, "addReturnPathHeader": {"else": "true"},
        "addAuthResultsHeader": {"else": "true"}}})
    ok("singleton" in (r.get("updated") or {}), "el operador activa las cabeceras del servidor", json.dumps(r)[:300])
    ae.reiniciar(srv)
    ana = Jmap(base, "ana@a.test", PW)
    beto = Jmap(base, "beto@b.test", PW)
    adm_a = Jmap(base, "admin@a.test", PW)
    adm_b = Jmap(base, "admin@b.test", PW)

    # --------------------------------------------------------------------------------------------------------
    print("== correo: el blobId del elemento archivado descarga el mensaje íntegro (con las cabeceras del servidor)")
    ok(ae.enviar(srv, "ana@a.test", "Integro", "Cuerpo integro 51b.") is None, "llega un mensaje")
    ids = ae.esperar(lambda: ae.buscar_correo(ana, "Integro"))
    antes = ae.correo(ana, ids[0], ["blobId"])
    _, cuerpo_antes = ae.descargar(ana, ana.cuenta(), antes["blobId"])
    ana.una("Email/set", {"accountId": ana.cuenta(), "destroy": ids}, ae.USING_MAIL)
    item = esperar_archivado(ana, "Email", subject="Integro")
    ok(item is not None, "el mensaje se archiva")
    if item:
        estado, cuerpo = ae.descargar(ana, ana.cuenta(), item["blobId"])
        ok(estado == 200 and cuerpo == cuerpo_antes and b"Received:" in cuerpo,
           "su blobId descarga el mensaje completo, byte a byte, con Received y las demás cabeceras del servidor",
           f"{estado} {len(cuerpo)} vs {len(cuerpo_antes)}")
        estado, cuerpo = ae.descargar(adm_a, adm_a.cuenta(), item["blobId"])
        ok(estado == 200 and cuerpo == cuerpo_antes, "el administrador de A también lo descarga (cuenta de su ámbito)",
           str(estado))
        estado, _ = ae.descargar(adm_b, adm_b.cuenta(), item["blobId"])
        ok(estado != 200, "el de B no", str(estado))
        estado, _ = ae.descargar(beto, beto.cuenta(), item["blobId"])
        ok(estado != 200, "ni beto", str(estado))

    # --------------------------------------------------------------------------------------------------------
    print("== archivos (WebDAV): borrar → archivado → restaurar íntegro en su carpeta")
    contenido = ("Informe trimestral\n" + "línea de datos 0123456789\n" * 200).encode()
    estado, _ = dav(ana, "MKCOL", ruta("file", "ana@a.test", "informes") + "/")
    ok(estado in (201, 200), "MKCOL de la carpeta informes", str(estado))
    estado, _ = dav(ana, "PUT", ruta("file", "ana@a.test", "informes", "trimestre.txt"), contenido,
                    {"Content-Type": "text/plain"})
    ok(estado in (201, 204), "PUT del archivo", str(estado))
    estado, _ = dav(ana, "DELETE", ruta("file", "ana@a.test", "informes", "trimestre.txt"))
    ok(estado in (200, 204), "DELETE del archivo", str(estado))
    item = esperar_archivado(ana, "FileNode", name="trimestre.txt")
    ok(item is not None and item.get("status") == "archived", "aparece como FileNode con su nombre",
       json.dumps(archivados(ana))[:400])
    if item:
        estado, cuerpo = ae.descargar(ana, ana.cuenta(), item["blobId"])
        ok(estado == 200 and cuerpo == contenido, "su blobId descarga el archivo", str(estado))
        ok(restaurar(ana, item["id"]), "pedir la restauración")
        ok(ae.esperar(lambda: dav(ana, "GET", ruta("file", "ana@a.test", "informes", "trimestre.txt"))[1] == contenido,
                      30), "vuelve a su carpeta, con su nombre y su contenido byte a byte")
        ok(ae.esperar(lambda: not de_tipo(archivados(ana), "FileNode", name="trimestre.txt"), 20),
           "y sale del archivo")

    print("== archivos: borrar una carpeta archiva lo que contiene; mover no archiva nada")
    dav(ana, "MKCOL", ruta("file", "ana@a.test", "viejos") + "/")
    dav(ana, "PUT", ruta("file", "ana@a.test", "viejos", "a.txt"), b"contenido a", {"Content-Type": "text/plain"})
    dav(ana, "PUT", ruta("file", "ana@a.test", "viejos", "b.txt"), b"contenido b", {"Content-Type": "text/plain"})
    dav(ana, "PUT", ruta("file", "ana@a.test", "movible.txt"), b"me muevo", {"Content-Type": "text/plain"})
    estado, _ = dav(ana, "MOVE", ruta("file", "ana@a.test", "movible.txt"),
                    cabeceras={"Destination": base + ruta("file", "ana@a.test", "informes", "movido.txt")})
    ok(estado in (201, 204), "MOVE de un archivo", str(estado))
    estado, _ = dav(ana, "DELETE", ruta("file", "ana@a.test", "viejos") + "/")
    ok(estado in (200, 204), "DELETE de la carpeta", str(estado))
    ok(ae.esperar(lambda: de_tipo(archivados(ana), "FileNode", name="a.txt")
                  and de_tipo(archivados(ana), "FileNode", name="b.txt"), 20),
       "sus dos archivos aparecen en el archivo")
    ok(not any(o.get("name") in ("movible.txt", "movido.txt") for o in archivados(ana)),
       "el archivo movido no se archiva")
    a_txt = de_tipo(archivados(ana), "FileNode", name="a.txt")
    if a_txt:
        restaurar(ana, a_txt["id"])
        ok(ae.esperar(lambda: dav(ana, "GET", ruta("file", "ana@a.test", "a.txt"))[1] == b"contenido a", 30),
           "si la carpeta ya no existe, vuelve a la raíz")

    # --------------------------------------------------------------------------------------------------------
    print("== eventos (CalDAV): borrar → archivado → restaurar íntegro en su calendario")
    ruta_ev = ruta("cal", "ana@a.test", "default", "presupuesto.ics")
    estado, _ = dav(ana, "PUT", ruta_ev, EVENTO, {"Content-Type": "text/calendar; charset=utf-8"})
    ok(estado in (201, 204), "PUT del evento", str(estado))
    _, original = dav(ana, "GET", ruta_ev)
    estado, _ = dav(ana, "DELETE", ruta_ev)
    ok(estado in (200, 204), "DELETE del evento", str(estado))
    item = esperar_archivado(ana, "CalendarEvent", title="Reunión de presupuesto")
    ok(item is not None and (item.get("startTime") or "").startswith("2026-12-01T10:00:00"),
       "aparece como CalendarEvent con su título y su inicio", json.dumps(archivados(ana))[:400])
    claves = ("UID", "SUMMARY", "DESCRIPTION", "LOCATION", "DTSTART", "DTEND")
    if item:
        estado, cuerpo = ae.descargar(ana, ana.cuenta(), item["blobId"])
        ok(estado == 200 and lineas(cuerpo, *claves) == lineas(original, *claves),
           "su blobId descarga el iCalendar completo", f"{estado} {cuerpo[:300]!r}")
        _, r = beto.get("x:ArchivedItem", [item["id"]])
        ok(item["id"] in (r.get("notFound") or []), "beto no lo ve")
        _, r = adm_b.set("x:ArchivedItem", update={item["id"]: {"status": "requestRestore"}})
        ok((r.get("notUpdated") or {}).get(item["id"], {}).get("type") == "notFound",
           "el administrador de B no lo restaura")
        ok(restaurar(adm_a, item["id"]), "el administrador de A pide la restauración")
        ok(ae.esperar(lambda: lineas(dav(ana, "GET", ruta_ev)[1], *claves) == lineas(original, *claves), 30),
           "vuelve a su calendario, con su nombre y los mismos datos (UID, título, fechas, lugar)")

    print("== eventos: borrar un calendario con eventos los archiva")
    dav(ana, "MKCALENDAR", ruta("cal", "ana@a.test", "proyectos") + "/")
    ev2 = EVENTO.replace("evento-archivo-7d1", "evento-proyecto-2").replace("Reunión de presupuesto", "Hito del proyecto")
    estado, _ = dav(ana, "PUT", ruta("cal", "ana@a.test", "proyectos", "hito.ics"), ev2,
                    {"Content-Type": "text/calendar"})
    ok(estado in (201, 204), "PUT de un evento en otro calendario", str(estado))
    estado, _ = dav(ana, "DELETE", ruta("cal", "ana@a.test", "proyectos") + "/")
    ok(estado in (200, 204), "DELETE del calendario", str(estado))
    hito = esperar_archivado(ana, "CalendarEvent", title="Hito del proyecto")
    ok(hito is not None, "su evento aparece archivado")
    if hito:
        restaurar(ana, hito["id"])
        ok(ae.esperar(lambda: b"evento-proyecto-2" in dav(ana, "GET", ruta("cal", "ana@a.test", "default", "hito.ics"))[1],
                      30), "sin su calendario, vuelve al calendario por defecto")

    # --------------------------------------------------------------------------------------------------------
    print("== contactos (CardDAV): borrar → archivado → restaurar íntegro en su libreta")
    ruta_ct = ruta("card", "ana@a.test", "default", "marta.vcf")
    estado, _ = dav(ana, "PUT", ruta_ct, CONTACTO, {"Content-Type": "text/vcard; charset=utf-8"})
    ok(estado in (201, 204), "PUT del contacto", str(estado))
    _, original = dav(ana, "GET", ruta_ct)
    estado, _ = dav(ana, "DELETE", ruta_ct)
    ok(estado in (200, 204), "DELETE del contacto", str(estado))
    item = esperar_archivado(ana, "ContactCard", name="Marta Proveedora")
    ok(item is not None, "aparece como ContactCard con su nombre", json.dumps(archivados(ana))[:400])
    claves = ("UID", "FN", "N", "EMAIL", "TEL", "NOTE")
    if item:
        estado, cuerpo = ae.descargar(ana, ana.cuenta(), item["blobId"])
        ok(estado == 200 and lineas(cuerpo, *claves) == lineas(original, *claves),
           "su blobId descarga la vCard completa", f"{estado} {cuerpo[:300]!r}")
        ok(restaurar(ana, item["id"]), "pedir la restauración")
        ok(ae.esperar(lambda: lineas(dav(ana, "GET", ruta_ct)[1], *claves) == lineas(original, *claves), 30),
           "vuelve a su libreta, con su nombre y los mismos datos")
        ok(ae.esperar(lambda: not de_tipo(archivados(ana), "ContactCard", name="Marta Proveedora"), 20),
           "y sale del archivo")
        # Restaurar otra vez el mismo contacto: el UID ya está en la libreta → falla y sigue archivado.
        dav(ana, "PUT", ruta("card", "ana@a.test", "default", "otra.vcf"),
            CONTACTO.replace("FN:Marta Proveedora", "FN:Marta Repetida"), {"Content-Type": "text/vcard"})
        dav(ana, "DELETE", ruta("card", "ana@a.test", "default", "otra.vcf"))
        repetida = esperar_archivado(ana, "ContactCard", name="Marta Repetida")
        if repetida:
            restaurar(ana, repetida["id"])
            vuelta = ae.esperar(lambda: (lambda o: o if o and o.get("status") == "archived" else None)(
                de_tipo(archivados(ana), "ContactCard", name="Marta Repetida")), 30)
            ok(vuelta is not None, "con el mismo UID ya en la libreta, la restauración falla y sigue archivado")

    # --------------------------------------------------------------------------------------------------------
    print("== guiones Sieve (JMAP): borrar → archivado (con su texto) → restaurar íntegro")
    import urllib.request
    req = urllib.request.Request(base + f"/jmap/upload/{ana.cuenta()}/", data=GUION.encode(), method="POST")
    req.add_header("Authorization", ana.auth)
    req.add_header("Content-Type", "application/sieve")
    with urllib.request.urlopen(req, timeout=30) as resp:
        subido = json.loads(resp.read())
    _, r = ana.una("SieveScript/set", {"accountId": ana.cuenta(USING_SIEVE[1]),
                                       "create": {"g": {"name": "facturas", "blobId": subido["blobId"]}}}, USING_SIEVE)
    guion_id = (r.get("created") or {}).get("g", {}).get("id")
    ok(guion_id, "se crea el guion", json.dumps(r)[:300])
    _, r = ana.una("SieveScript/set", {"accountId": ana.cuenta(USING_SIEVE[1]), "destroy": [guion_id]}, USING_SIEVE)
    ok(guion_id in (r.get("destroyed") or []), "se borra", json.dumps(r)[:300])
    item = esperar_archivado(ana, "SieveScript", name="facturas")
    ok(item is not None and item.get("contents") == GUION, "aparece como SieveScript con su nombre y su texto",
       json.dumps(item)[:300])
    if item:
        ok(restaurar(ana, item["id"]), "pedir la restauración")

        def guion_restaurado():
            _, r = ana.una("SieveScript/query", {"accountId": ana.cuenta(USING_SIEVE[1])}, USING_SIEVE)
            _, g = ana.una("SieveScript/get", {"accountId": ana.cuenta(USING_SIEVE[1]), "ids": r.get("ids") or []},
                           USING_SIEVE)
            return next((s for s in g.get("list") or [] if s.get("name") == "facturas"), None)
        restaurado = ae.esperar(guion_restaurado, 30)
        ok(restaurado is not None and not restaurado.get("isActive"), "vuelve con su nombre (inactivo)")
        if restaurado:
            estado, cuerpo = ae.descargar(ana, ana.cuenta(), restaurado["blobId"])
            ok(cuerpo.decode() == GUION, "y con el mismo texto")

    # --------------------------------------------------------------------------------------------------------
    print("== aislamiento y vista del administrador de inquilino")
    lista = archivados(adm_a, {"accountId": ana_id})
    ok(any(o.get("@type") == "FileNode" for o in lista), "el administrador de A ve el archivo de ana (todos los tipos)")
    ok(not archivados(adm_b, {"accountId": ana_id}), "el de B no ve nada de ana")
    ok(not archivados(beto), "beto no tiene nada en su archivo")

    # --------------------------------------------------------------------------------------------------------
    print("== purga al vencer (también lo que no es correo)")
    _, r = root.set("x:DataRetention", update={"singleton": {"archiveDeletedItemsFor": 2000}})
    ok("singleton" in (r.get("updated") or {}), "retención a 2 s")
    ae.reiniciar(srv)
    ana = Jmap(base, "ana@a.test", PW)
    dav(ana, "PUT", ruta("card", "ana@a.test", "default", "fugaz.vcf"),
        CONTACTO.replace("4fbe8971", "11111111").replace("FN:Marta Proveedora", "FN:Fugaz"),
        {"Content-Type": "text/vcard"})
    dav(ana, "DELETE", ruta("card", "ana@a.test", "default", "fugaz.vcf"))
    fugaz = esperar_archivado(ana, "ContactCard", name="Fugaz")
    ok(fugaz is not None, "se archiva con la retención corta")
    time.sleep(3)
    ahora = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    _, r = root.set("x:Task", create={"p": {"@type": "StoreMaintenance", "maintenanceType": "purgeBlob",
                                            "status": {"@type": "Pending", "due": ahora}}})
    ok("p" in (r.get("created") or {}), "tarea de limpieza de blobs")
    ok(ae.esperar(lambda: not de_tipo(archivados(ana), "ContactCard", name="Fugaz"), 90),
       "al vencer, se purga del archivo")
    if fugaz:
        estado, _ = ae.descargar(ana, ana.cuenta(), fugaz["blobId"])
        ok(estado == 404, "y su contenido ya no se puede descargar", str(estado))


def main():
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    srv = Servidor(os.path.abspath(sys.argv[1]))
    error = None
    try:
        ae.configurar(srv)
        pruebas(srv)
    except Exception as e:  # noqa: BLE001 — se informa y se guarda el registro
        error = e
        mi.FALLOS.append(f"excepción: {e!r}")
    finally:
        srv.parar()
        if mi.FALLOS:
            destino = os.path.join(tempfile.gettempdir(), "bezenti-archivo-tipos.log")
            shutil.copy(srv.log, destino)
            print(f"\nregistro del servidor en {destino}")
        shutil.rmtree(srv.dir, ignore_errors=True)
    if error is not None:
        import traceback
        traceback.print_exception(error)
    print(f"\n{len(mi.PROBADOS) - len(mi.FALLOS)}/{len(mi.PROBADOS)} comprobaciones correctas")
    if mi.FALLOS:
        print("FALLOS:\n  " + "\n  ".join(mi.FALLOS))
        sys.exit(1)


if __name__ == "__main__":
    main()
