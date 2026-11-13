#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 bezenti
#
# SPDX-License-Identifier: AGPL-3.0-only
"""Pruebas de integración de la fase 3: direcciones enmascaradas y archivo de borrados (edición bezenti).

Levanta el servidor real (binario `stalwart` con `sqlite` y `bezenti`) con una base SQLite nueva y lo prueba por
la API real (JMAP, SMTP e IMAP), solo con la biblioteca estándar de Python. Reutiliza las utilidades de
`multi_inquilino.py` (mismo directorio).

Escenario: el operador crea los inquilinos A y B con sus dominios, administradores y usuarios (ana@a.test,
beto@b.test). Después:

- Enmascaradas (bezenti/especificaciones/enmascaradas.md): ana crea una dirección, le llega correo por SMTP, lo
  lee por JMAP e IMAP, la desactiva (rechazo en RCPT), la reactiva, la borra (rechazo); caducidad; límites
  (cuota de la cuenta y valor por defecto 5); dominio y prefijo no válidos; aislamiento entre inquilinos.
- Archivo (bezenti/especificaciones/archivo.md): un mensaje con buzón y etiquetas propios se borra por JMAP,
  aparece archivado, se descarga, se restaura y vuelve íntegro (mismo contenido, buzón, etiquetas y fecha);
  `EXPUNGE` de IMAP también archiva; aislamiento entre inquilinos y vista del administrador de inquilino;
  borrar del archivo; recuperar una cuenta borrada (`archiveDeletedAccountsFor`); purga al vencer.

Uso: archivo_enmascaradas.py RUTA_AL_BINARIO_STALWART
"""

import imaplib
import json
import os
import re
import shutil
import smtplib
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import multi_inquilino as mi  # noqa: E402
from multi_inquilino import ADMIN_PW, PW, USING, USING_MAIL, Jmap, Servidor, ids_de, ok  # noqa: E402

DIA_MS = 86_400_000


def configurar(srv):
    """Como `multi_inquilino.configurar`, y además activa el archivo (1 día) y el aplazamiento de cuentas (1 h)."""
    print("== configuración inicial (modo recuperación)")
    srv.arrancar(recuperacion=True)
    rec = Jmap(f"http://127.0.0.1:{srv.p_rec}", "admin", ADMIN_PW)
    for nombre, proto, puerto in (("http", "http", srv.p_http), ("smtp", "smtp", srv.p_smtp), ("imap", "imap", srv.p_imap)):
        rec.crear("x:NetworkListener", {
            "name": nombre, "protocol": proto, "bind": {f"127.0.0.1:{puerto}": True},
            "useTls": False, "implicitTls": False,
        })
    rec.crear("x:Tracer", {"@type": "Stdout", "level": "info", "enable": True})
    rec.crear("x:Asn", {"@type": "Disabled"})
    for objeto, cambios in (
        ("x:SpamSettings", {"enable": False, "spamFilterRulesUrl": None}),
        ("x:Imap", {"allowPlainTextAuth": True}),
        ("x:MtaStageAuth", {"require": {"else": "false"}}),
        ("x:DataRetention", {"archiveDeletedItemsFor": DIA_MS, "archiveDeletedAccountsFor": 3_600_000}),
    ):
        _, r = rec.set(objeto, update={"singleton": cambios})
        assert "singleton" in (r.get("updated") or {}), f"{objeto}: {r}"
    _, apps = rec.query("x:Application")
    if apps.get("ids"):
        rec.set("x:Application", update={i: {"enable": False} for i in apps["ids"]})
    srv.parar()
    reiniciar(srv)


def reiniciar(srv):
    srv.parar()
    srv.arrancar(recuperacion=False)
    mi.esperar_puerto(srv.p_smtp)
    mi.esperar_puerto(srv.p_imap)


def enviar(srv, para, asunto, cuerpo="Hola.", remitente="remitente@externo.example"):
    """Envía por SMTP. Devuelve None si se aceptó o el texto del rechazo."""
    msg = (f"From: Remitente Externo <{remitente}>\r\nTo: {para}\r\nSubject: {asunto}\r\n"
           f"Message-ID: <{time.time_ns()}@externo.example>\r\n\r\n{cuerpo}\r\n")
    try:
        with smtplib.SMTP("127.0.0.1", srv.p_smtp, timeout=30) as s:
            s.ehlo("externo.example")
            s.sendmail(remitente, [para], msg)
        return None
    except smtplib.SMTPRecipientsRefused as e:
        return str(e.recipients)
    except smtplib.SMTPResponseException as e:
        return f"{e.smtp_code} {e.smtp_error!r}"


def esperar(cond, segundos=60, paso=0.5):
    fin = time.time() + segundos
    while time.time() < fin:
        r = cond()
        if r:
            return r
        time.sleep(paso)
    return cond()


def buscar_correo(usr, asunto):
    _, r = usr.una("Email/query", {"accountId": usr.cuenta(), "filter": {"subject": asunto}}, USING_MAIL)
    return list(r.get("ids") or [])


def correo(usr, email_id, props):
    _, r = usr.una("Email/get", {"accountId": usr.cuenta(), "ids": [email_id], "properties": props}, USING_MAIL)
    return (r.get("list") or [None])[0]


def descargar(usr, account_id, blob_id):
    estado, cuerpo = usr.http("GET", f"/jmap/download/{account_id}/{blob_id}/m.eml?accept=message/rfc822")
    return estado, cuerpo


def archivados(usr, filtro=None):
    args = {}
    if filtro is not None:
        args["filter"] = filtro
    n, r = usr.una("x:ArchivedItem/query", args)
    return n, r


def pruebas(srv):
    base = f"http://127.0.0.1:{srv.p_http}"
    rec = Jmap(base, "admin", ADMIN_PW)

    print("== el operador prepara inquilinos, dominios y cuentas")
    dom_op = rec.crear("x:Domain", {"name": "operador.test", "dkimManagement": {"@type": "Manual"}})
    rec.crear("x:Account", {"@type": "User", "name": "root", "domainId": dom_op, "roles": {"@type": "Admin"},
                            "credentials": {"0": {"@type": "Password", "secret": PW}}})
    root = Jmap(base, "root@operador.test", PW)
    t_a = root.crear("x:Tenant", {"name": "Empresa A"})
    t_b = root.crear("x:Tenant", {"name": "Empresa B"})
    dom_a = root.crear("x:Domain", {"name": "a.test", "memberTenantId": t_a, "dkimManagement": {"@type": "Manual"}})
    dom_a2 = root.crear("x:Domain", {"name": "a2.test", "memberTenantId": t_a, "dkimManagement": {"@type": "Manual"}})
    dom_b = root.crear("x:Domain", {"name": "b.test", "memberTenantId": t_b, "dkimManagement": {"@type": "Manual"}})
    cuenta = lambda nombre, dom, tenant, **extra: root.crear("x:Account", {  # noqa: E731
        "@type": "User", "name": nombre, "domainId": dom, "memberTenantId": tenant,
        "credentials": {"0": {"@type": "Password", "secret": PW}}, **extra})
    cuenta("admin", dom_a, t_a, roles={"@type": "Admin"})
    cuenta("admin", dom_b, t_b, roles={"@type": "Admin"})
    ana_id = cuenta("ana", dom_a, t_a, quotas={"maxMaskedAddresses": 4})
    beto_id = cuenta("beto", dom_b, t_b)
    ana = Jmap(base, "ana@a.test", PW)
    beto = Jmap(base, "beto@b.test", PW)
    adm_a = Jmap(base, "admin@a.test", PW)
    adm_b = Jmap(base, "admin@b.test", PW)

    # ------------------------------------------------------------------------------------------------------
    print("== direcciones enmascaradas: alta")
    n, r = ana.set("x:MaskedEmail", create={"m": {"description": "Boletín", "forDomain": "https://ejemplo.com",
                                                  "emailPrefix": "Boletin"}})
    creada = (r.get("created") or {}).get("m") or {}
    m1 = creada.get("id")
    dir1 = creada.get("email", "")
    ok(m1 and re.fullmatch(r"boletin\.[a-km-np-z2-9]{10}@a\.test", dir1),
       "crear: la dirección es <prefijo en minúsculas>.<código>@<dominio de la cuenta>", json.dumps(r)[:400])
    ok(creada.get("createdAt"), "crear: el servidor pone createdAt")
    _, r = ana.get("x:MaskedEmail", [m1])
    obj = (r.get("list") or [{}])[0]
    ok(obj.get("enable") is True and obj.get("email") == dir1 and obj.get("forDomain") == "https://ejemplo.com",
       "get: activada por defecto y con sus datos", json.dumps(r)[:400])
    n, r = ana.set("x:MaskedEmail", update={m1: {"email": "robada@a.test"}})
    ok(m1 in (r.get("notUpdated") or {}), "la dirección no se puede cambiar (la pone el servidor)")
    n, r = ana.set("x:MaskedEmail", create={"x": {"emailPrefix": "_malo"}})
    err = (r.get("notCreated") or {}).get("x", {})
    ok(err.get("type") == "invalidProperties", "prefijo con _ inicial → invalidProperties", json.dumps(r)[:300])
    n, r = ana.set("x:MaskedEmail", create={"x": {"emailPrefix": "con.punto"}})
    ok("x" in (r.get("notCreated") or {}), "prefijo con punto → rechazado")
    n, r = ana.set("x:MaskedEmail", create={"x": {"emailDomain": "b.test"}})
    err = (r.get("notCreated") or {}).get("x", {})
    ok(err.get("type") == "forbidden" and "not valid for this account" in (err.get("description") or ""),
       "emailDomain de otro inquilino → forbidden", json.dumps(r)[:300])
    n, r = ana.set("x:MaskedEmail", create={"x": {"emailDomain": "a2.test"}})
    ok("x" in (r.get("notCreated") or {}), "emailDomain en el que la cuenta no tiene dirección → rechazado")

    print("== direcciones enmascaradas: recepción, desactivación y borrado")
    ok(enviar(srv, dir1, "Para la enmascarada") is None, "SMTP a la dirección enmascarada → aceptado")
    ids = esperar(lambda: buscar_correo(ana, "Para la enmascarada"))
    ok(ids, "el correo llega a la cuenta dueña (JMAP)")
    if ids:
        c = correo(ana, ids[0], ["to"])
        ok(c and any(t.get("email") == dir1 for t in c.get("to") or []),
           "la cabecera To conserva la dirección enmascarada", json.dumps(c))
    try:
        imap = imaplib.IMAP4("127.0.0.1", srv.p_imap)
        imap.login("ana@a.test", PW)
        imap.select("INBOX")
        typ, data = imap.search(None, '(SUBJECT "Para la enmascarada")')
        ok(typ == "OK" and data and data[0].split(), "y se lee por IMAP")
        imap.logout()
    except imaplib.IMAP4.error as e:
        ok(False, "y se lee por IMAP", str(e))
    rechazo = enviar(srv, dir1.replace("@a.test", "@b.test"), "Dominio cambiado")
    ok(rechazo is not None, "la misma parte local en otro dominio no es la enmascarada → rechazo", str(rechazo))

    _, r = ana.set("x:MaskedEmail", update={m1: {"enable": False}})
    ok(m1 in (r.get("updated") or {}), "desactivar → actualizado")
    rechazo = enviar(srv, dir1, "Desactivada")
    ok(rechazo is not None and "550" in rechazo, "SMTP a una enmascarada desactivada → 550 (destinatario desconocido)", str(rechazo))
    _, r = ana.set("x:MaskedEmail", update={m1: {"enable": True}})
    ok(enviar(srv, dir1, "Reactivada") is None, "reactivada → vuelve a aceptar")
    _, r = ana.set("x:MaskedEmail", destroy=[m1])
    ok(m1 in (r.get("destroyed") or []), "borrar → destruida")
    rechazo = enviar(srv, dir1, "Borrada")
    ok(rechazo is not None and "550" in rechazo, "SMTP a una enmascarada borrada → 550", str(rechazo))

    print("== direcciones enmascaradas: caducidad")
    expira = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(time.time() + 4))
    n, r = ana.set("x:MaskedEmail", create={"e": {"description": "Caduca", "expiresAt": expira}})
    creada = (r.get("created") or {}).get("e") or {}
    ok(enviar(srv, creada.get("email", "x@a.test"), "Antes de caducar") is None, "antes de caducar → aceptado", json.dumps(r)[:300])
    time.sleep(5)
    rechazo = enviar(srv, creada.get("email", "x@a.test"), "Caducada")
    ok(rechazo is not None and "550" in rechazo, "caducada → 550", str(rechazo))
    pasada = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(time.time() - 60))
    n, r = ana.set("x:MaskedEmail", create={"p": {"expiresAt": pasada}})
    ok("p" in (r.get("notCreated") or {}), "caducidad en el pasado → rechazado")

    print("== direcciones enmascaradas: límites")
    # ana tiene `maxMaskedAddresses: 4` en su cuenta y ya tiene 1 (la caducada).
    creadas = []
    for i in range(3):
        _, r = ana.set("x:MaskedEmail", create={"n": {"description": f"n{i}"}})
        creadas += [o["id"] for o in (r.get("created") or {}).values()]
    _, r = ana.set("x:MaskedEmail", create={"n": {"description": "una de más"}})
    err = (r.get("notCreated") or {}).get("n", {})
    ok(len(creadas) == 3 and err.get("type") == "overQuota" and "quota of 4 masked addresses" in (err.get("description") or ""),
       "cuota de la cuenta (4) → overQuota con el texto de la documentación", json.dumps(r)[:300])
    _, r = ana.set("x:MaskedEmail", update={creadas[0]: {"enable": False}})
    _, r = ana.set("x:MaskedEmail", create={"n": {}})
    ok("n" in (r.get("notCreated") or {}), "las desactivadas siguen contando para el límite")
    _, r = ana.set("x:MaskedEmail", destroy=[creadas[0]])
    _, r = ana.set("x:MaskedEmail", create={"n": {}})
    ok("n" in (r.get("created") or {}), "borrar una libera su plaza")
    beto_m = []
    for i in range(6):
        _, r = beto.set("x:MaskedEmail", create={"n": {}})
        beto_m += [o["id"] for o in (r.get("created") or {}).values()]
        ultimo = r
    err = (ultimo.get("notCreated") or {}).get("n", {})
    ok(len(beto_m) == 5 and err.get("type") == "overQuota", "límite por defecto del servidor (5)", json.dumps(ultimo)[:300])

    print("== direcciones enmascaradas: aislamiento")
    _, r = ana.query("x:MaskedEmail")
    ana_m = ids_de(r)
    _, r = beto.get("x:MaskedEmail", list(ana_m))
    ok(not r.get("list") and set(r.get("notFound") or []) >= ana_m, "beto (inquilino B) no ve las de ana por id")
    _, r = beto.query("x:MaskedEmail", {"accountId": ana_id})
    ok(not (ids_de(r) & ana_m), "ni filtrando por la cuenta de ana")
    _, r = beto.set("x:MaskedEmail", update={i: {"enable": False} for i in ana_m})
    _, r2 = beto.set("x:MaskedEmail", destroy=list(ana_m))
    ok(all((r.get("notUpdated") or {}).get(i, {}).get("type") == "notFound" for i in ana_m)
       and all((r2.get("notDestroyed") or {}).get(i, {}).get("type") == "notFound" for i in ana_m),
       "ni modificarlas ni borrarlas (notFound)", json.dumps(r)[:300] + json.dumps(r2)[:300])
    for quien, usr in (("admin de B", adm_b), ("admin de A", adm_a)):
        _, r = usr.get("x:MaskedEmail", list(ana_m))
        ok(not r.get("list"), f"el {quien} no ve las enmascaradas de ana (son de la cuenta)")
    _, r = root.query("x:MaskedEmail", {"accountId": ana_id})
    ok(ana_m <= ids_de(r), "el operador (impersonate) las ve filtrando por cuenta", json.dumps(r)[:300])

    # ------------------------------------------------------------------------------------------------------
    print("== archivo: borrar → archivado → restaurar → íntegro")
    _, r = ana.una("Mailbox/set", {"accountId": ana.cuenta(), "create": {"f": {"name": "Facturas"}}}, USING_MAIL)
    facturas = r["created"]["f"]["id"]
    ok(enviar(srv, "ana@a.test", "Archivo uno", "Contenido unico 9f3a.") is None, "llega el mensaje a archivar")
    ids = esperar(lambda: buscar_correo(ana, "Archivo uno"))
    original = ids[0]
    etiquetas = {"$seen": True, "$flagged": True, "proyecto": True}
    _, r = ana.una("Email/set", {"accountId": ana.cuenta(), "update": {
        original: {"keywords": etiquetas, "mailboxIds": {facturas: True}}}}, USING_MAIL)
    antes = correo(ana, original, ["blobId", "receivedAt", "keywords", "mailboxIds", "size"])
    _, cuerpo_antes = descargar(ana, ana.cuenta(), antes["blobId"])
    _, r = ana.una("Email/set", {"accountId": ana.cuenta(), "destroy": [original]}, USING_MAIL)
    ok(original in (r.get("destroyed") or []), "Email/set destroy")

    def un_archivado():
        _, r = archivados(ana)
        return list(r.get("ids") or [])
    arch = esperar(un_archivado)
    ok(len(arch) == 1, "el mensaje borrado aparece en x:ArchivedItem/query", str(arch))
    _, r = ana.get("x:ArchivedItem", arch)
    item = (r.get("list") or [{}])[0]
    ok(item.get("@type") == "Email" and item.get("subject") == "Archivo uno"
       and "remitente@externo.example" in item.get("from", "") and item.get("size", 0) > 0
       and item.get("status") == "archived" and "accountId" not in item,
       "get: tipo, asunto, remitente, tamaño y estado (sin accountId para el usuario)", json.dumps(item))
    hasta = time.mktime(time.strptime(item.get("archivedUntil", "1970-01-01T00:00:00Z")[:19], "%Y-%m-%dT%H:%M:%S")) - time.timezone
    ok(abs(hasta - (time.time() + 86400)) < 600, "archivedUntil = ahora + archiveDeletedItemsFor", item.get("archivedUntil"))
    estado, cuerpo = descargar(ana, ana.cuenta(), item.get("blobId", "x"))
    # Fase 8: el blob se descarga íntegro, con las cabeceras que añadió el servidor al recibirlo (Received…).
    ok(estado == 200 and cuerpo == cuerpo_antes,
       "el blob archivado se descarga íntegro (con las cabeceras del servidor)", f"{estado} {cuerpo[:200]!r}")
    _, r = ana.set("x:ArchivedItem", update={arch[0]: {"subject": "falsificado"}})
    ok(arch[0] in (r.get("notUpdated") or {}), "solo se puede cambiar el estado (no el asunto)")
    n, r = ana.set("x:ArchivedItem", create={"x": {"@type": "Email"}})
    ok(n == "error" or "x" in (r.get("notCreated") or {}), "no se pueden crear elementos archivados", json.dumps(r)[:300])

    _, r = ana.set("x:ArchivedItem", update={arch[0]: {"status": "requestRestore"}})
    ok(arch[0] in (r.get("updated") or {}), "pedir la restauración (status: requestRestore)", json.dumps(r)[:300])
    ids = esperar(lambda: buscar_correo(ana, "Archivo uno"))
    ok(ids, "el mensaje vuelve al buzón")
    if ids:
        despues = correo(ana, ids[0], ["blobId", "receivedAt", "keywords", "mailboxIds", "size"])
        _, cuerpo_despues = descargar(ana, ana.cuenta(), despues["blobId"])
        ok(cuerpo_despues == cuerpo_antes and despues["size"] == antes["size"], "mismo contenido, byte a byte")
        ok(despues["mailboxIds"] == {facturas: True}, "en su buzón original (Facturas)", json.dumps(despues["mailboxIds"]))
        # `$hasattachment`/`$hasnoattachment` los calcula el servidor al ingresar el mensaje.
        propias = {k: v for k, v in despues["keywords"].items() if k not in ("$hasattachment", "$hasnoattachment")}
        ok(propias == etiquetas, "con las mismas etiquetas", json.dumps(despues["keywords"]))
        ok(despues["receivedAt"] == antes["receivedAt"], "con la misma fecha de recepción")
    ok(esperar(lambda: not un_archivado(), 30), "y ya no está en el archivo")

    print("== archivo: EXPUNGE de IMAP también archiva (sin \\Deleted al restaurar)")
    enviar(srv, "ana@a.test", "Archivo imap")
    esperar(lambda: buscar_correo(ana, "Archivo imap"))
    imap = imaplib.IMAP4("127.0.0.1", srv.p_imap)
    imap.login("ana@a.test", PW)
    imap.select("INBOX")
    _, data = imap.search(None, '(SUBJECT "Archivo imap")')
    for num in data[0].split():
        imap.store(num, "+FLAGS", "(\\Deleted \\Answered)")
    imap.expunge()
    imap.logout()
    arch = esperar(un_archivado)
    _, r = ana.get("x:ArchivedItem", arch)
    item_imap = next((o for o in r.get("list") or [] if o.get("subject") == "Archivo imap"), None)
    ok(item_imap is not None, "el mensaje expurgado por IMAP aparece archivado", json.dumps(r)[:300])

    print("== archivo: aislamiento entre inquilinos")
    item_id = item_imap["id"] if item_imap else "zz"
    _, r = beto.get("x:ArchivedItem", [item_id])
    ok(not r.get("list") and item_id in (r.get("notFound") or []), "beto no ve el archivo de ana por id")
    _, r = archivados(beto, {"accountId": ana_id})
    ok(not ids_de(r), "ni filtrando por la cuenta de ana (el filtro se ignora para usuarios)")
    _, r = beto.set("x:ArchivedItem", update={item_id: {"status": "requestRestore"}})
    _, r2 = beto.set("x:ArchivedItem", destroy=[item_id])
    ok((r.get("notUpdated") or {}).get(item_id, {}).get("type") == "notFound"
       and (r2.get("notDestroyed") or {}).get(item_id, {}).get("type") == "notFound",
       "ni pedir su restauración ni borrarlo", json.dumps(r)[:200] + json.dumps(r2)[:200])
    _, r = archivados(adm_b, {"accountId": ana_id})
    ok(not ids_de(r), "el admin de B, filtrando por ana (otro inquilino) → vacío", json.dumps(r)[:300])
    _, r = adm_b.get("x:ArchivedItem", [item_id])
    ok(not r.get("list"), "el admin de B no lo obtiene por id")
    _, r = adm_b.set("x:ArchivedItem", destroy=[item_id])
    ok((r.get("notDestroyed") or {}).get(item_id, {}).get("type") == "notFound", "ni lo borra")
    _, r = archivados(adm_a, {"accountId": ana_id})
    ok(item_id in ids_de(r), "el admin de A ve el archivo de las cuentas de su inquilino", json.dumps(r)[:300])
    _, r = adm_a.get("x:ArchivedItem", [item_id])
    ok(r.get("list") and r["list"][0].get("accountId") == ana_id, "y lo obtiene (con su accountId)")
    _, r = archivados(root, {"accountId": ana_id})
    ok(item_id in ids_de(r), "el operador lo ve filtrando por cuenta")
    _, r = adm_a.set("x:ArchivedItem", update={item_id: {"status": "requestRestore"}})
    ok(item_id in (r.get("updated") or {}), "el admin de A restaura un mensaje de ana")
    ids = esperar(lambda: buscar_correo(ana, "Archivo imap"))
    if ids:
        c = correo(ana, ids[0], ["keywords", "mailboxIds"])
        ok(c["keywords"].get("$answered") and not c["keywords"].get("$deleted"),
           "vuelve con \\Answered y sin \\Deleted", json.dumps(c))
    else:
        ok(False, "el mensaje expurgado se restaura")

    print("== archivo: borrar del archivo")
    enviar(srv, "ana@a.test", "Archivo borrar")
    ids = esperar(lambda: buscar_correo(ana, "Archivo borrar"))
    ana.una("Email/set", {"accountId": ana.cuenta(), "destroy": ids}, USING_MAIL)
    arch = esperar(un_archivado)
    _, r = ana.set("x:ArchivedItem", destroy=arch)
    ok(set(arch) <= set(r.get("destroyed") or []), "el usuario borra un elemento de su archivo")
    ok(not un_archivado(), "y desaparece")

    print("== recuperar una cuenta borrada (archiveDeletedAccountsFor)")
    tmp_id = cuenta("temporal", dom_a, t_a)
    tmp = Jmap(base, "temporal@a.test", PW)
    enviar(srv, "temporal@a.test", "Antes de borrar la cuenta")
    ok(esperar(lambda: buscar_correo(tmp, "Antes de borrar la cuenta")), "la cuenta temporal recibe correo")
    _, r = root.set("x:Account", destroy=[tmp_id])
    ok(tmp_id in (r.get("destroyed") or []), "el operador borra la cuenta", json.dumps(r)[:300])
    _, r = root.query("x:Task")
    _, r = root.get("x:Task", list(r.get("ids") or []))
    tarea = next((t for t in r.get("list") or [] if t.get("@type") == "DestroyAccount" and t.get("accountName") == "temporal"), None)
    ok(tarea is not None and tarea.get("status", {}).get("due", "") > time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(time.time() + 3000)),
       "la tarea DestroyAccount queda aplazada archiveDeletedAccountsFor", json.dumps(tarea)[:400])
    if tarea:
        _, r = root.set("x:Task", destroy=[tarea["id"]])
        ok(tarea["id"] in (r.get("destroyed") or []), "borrar la tarea recupera la cuenta", json.dumps(r)[:300])
        _, r = root.get("x:Account", [tmp_id], ["name", "memberTenantId"])
        rec_obj = (r.get("list") or [{}])[0]
        ok(rec_obj.get("name") == "temporal" and rec_obj.get("memberTenantId") == t_a,
           "la cuenta vuelve con su id, nombre e inquilino", json.dumps(r)[:300])
        root.set("x:Account", update={tmp_id: {"credentials": {"0": {"@type": "Password", "secret": PW}}}})
        tmp = Jmap(base, "temporal@a.test", PW)
        ok(esperar(lambda: buscar_correo(tmp, "Antes de borrar la cuenta"), 20), "y con su correo")

    print("== purga al vencer (retención de 2 s)")
    _, r = root.set("x:DataRetention", update={"singleton": {"archiveDeletedItemsFor": 2000}})
    ok("singleton" in (r.get("updated") or {}), "retención a 2 s", json.dumps(r)[:200])
    reiniciar(srv)
    ana = Jmap(base, "ana@a.test", PW)
    enviar(srv, "ana@a.test", "Archivo purga", "Contenido para purgar 77c1.")
    ids = esperar(lambda: buscar_correo(ana, "Archivo purga"))
    blob = correo(ana, ids[0], ["blobId"])["blobId"]
    ana.una("Email/set", {"accountId": ana.cuenta(), "destroy": ids}, USING_MAIL)
    arch = esperar(un_archivado)
    _, r = ana.get("x:ArchivedItem", arch)
    blob_arch = (r.get("list") or [{}])[0].get("blobId", "")
    ok(len(arch) == 1, "se archiva con la retención corta")
    time.sleep(3)
    ahora = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    _, r = root.set("x:Task", create={"p": {"@type": "StoreMaintenance", "maintenanceType": "purgeBlob",
                                            "status": {"@type": "Pending", "due": ahora}}})
    ok("p" in (r.get("created") or {}), "tarea de limpieza de blobs (purgeBlob)", json.dumps(r)[:300])
    ok(esperar(lambda: not un_archivado(), 90), "al vencer, la limpieza programada lo purga del archivo")
    estado, _ = descargar(ana, ana.cuenta(), blob_arch)
    ok(estado == 404, "y su contenido ya no se puede descargar", str(estado))
    del blob


def main():
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    srv = Servidor(os.path.abspath(sys.argv[1]))
    error = None
    try:
        configurar(srv)
        pruebas(srv)
    except Exception as e:  # noqa: BLE001 — se informa y se guarda el registro
        error = e
        mi.FALLOS.append(f"excepción: {e!r}")
    finally:
        srv.parar()
        if mi.FALLOS:
            destino = os.path.join(tempfile.gettempdir(), "bezenti-archivo-enmascaradas.log")
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
