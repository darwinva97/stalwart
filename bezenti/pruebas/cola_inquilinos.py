#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 bezenti
#
# SPDX-License-Identifier: AGPL-3.0-only
"""Pruebas de integración de la fase 8 (1/2): cola SMTP, informes externos y cuentas borradas por inquilino.

Levanta el servidor real (binario `stalwart` con `sqlite` y `bezenti`) con una base SQLite nueva y lo prueba por
SMTP y JMAP, solo con la biblioteca estándar de Python. Los dominios remotos se entregan a un servidor SMTP falso
(este script) que puede tardar en aceptar o rechazar temporalmente. Reutiliza `multi_inquilino.py`.

Escenario (bezenti/especificaciones/multi-inquilino.md, §5 y §4):

- Reparto justo: la cola `remote` tiene 2 hilos; ana@a.test manda 40 mensajes y después beto@b.test uno; el de
  beto sale enseguida, no detrás de los 40.
- Vista y gestión de la cola: cada administrador de inquilino ve solo los mensajes de su inquilino (query, get,
  filtros, descarga del mensaje), los reintenta y los cancela; lo ajeno no existe.
- Límites: mensajes por intervalo, destinatarios por intervalo y mensajes a la vez en la cola, también
  jerárquicos (revendedor).
- Informes externos DMARC y TLS: cada uno con el inquilino del dominio al que se refiere; cada administrador ve y
  borra solo los suyos.
- Cuentas borradas: el administrador de un inquilino ve las de su ámbito (tareas `DestroyAccount`) y las
  recupera; las ajenas no existen; la recuperación respeta la cuota de cuentas del inquilino.

Uso: cola_inquilinos.py RUTA_AL_BINARIO_STALWART
"""

import json
import os
import shutil
import smtplib
import socketserver
import sys
import tempfile
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import multi_inquilino as mi  # noqa: E402
from multi_inquilino import ADMIN_PW, PW, Jmap, Servidor, ok  # noqa: E402

HORA_MS = 3_600_000


class SmtpFalso:
    """Servidor SMTP remoto falso: apunta lo que recibe; puede tardar (`retraso`) o rechazar (`modo = temporal`)."""

    def __init__(self):
        self.modo = "aceptar"
        self.retraso = 0.0
        self.recibidos = []  # (instante, remitente, destinatarios, asunto)
        self.lock = threading.Lock()
        padre = self

        class Manejador(socketserver.StreamRequestHandler):
            def responder(self, linea):
                self.wfile.write(linea.encode() + b"\r\n")
                self.wfile.flush()

            def handle(self):
                self.responder("220 remoto.example ESMTP falso")
                remitente, destinatarios = "", []
                while True:
                    linea = self.rfile.readline()
                    if not linea:
                        return
                    orden = linea.decode("utf-8", "replace").strip()
                    mayus = orden.upper()
                    if mayus.startswith(("EHLO", "HELO")):
                        self.responder("250-remoto.example")
                        self.responder("250 8BITMIME")
                    elif mayus.startswith("MAIL FROM"):
                        remitente, destinatarios = orden[10:].strip(), []
                        self.responder("250 2.1.0 ok")
                    elif mayus.startswith("RCPT TO"):
                        if padre.modo == "temporal":
                            self.responder("451 4.3.0 Prueba: vuelva más tarde")
                        else:
                            destinatarios.append(orden[8:].strip())
                            self.responder("250 2.1.5 ok")
                    elif mayus == "DATA":
                        self.responder("354 adelante")
                        datos = []
                        while True:
                            l2 = self.rfile.readline()
                            if not l2 or l2 in (b".\r\n", b".\n"):
                                break
                            datos.append(l2)
                        asunto = ""
                        for l2 in datos:
                            if l2.lower().startswith(b"subject:"):
                                asunto = l2[8:].decode("utf-8", "replace").strip()
                                break
                        if padre.retraso:
                            time.sleep(padre.retraso)
                        with padre.lock:
                            padre.recibidos.append((time.time(), remitente, list(destinatarios), asunto))
                        self.responder("250 2.0.0 recibido")
                    elif mayus == "QUIT":
                        self.responder("221 adiós")
                        return
                    else:
                        self.responder("250 ok")

        class Servidor(socketserver.ThreadingTCPServer):
            daemon_threads = True
            allow_reuse_address = True

        self.srv = Servidor(("127.0.0.1", 0), Manejador)
        self.puerto = self.srv.server_address[1]
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()

    def asuntos(self):
        with self.lock:
            return [r[3] for r in self.recibidos]

    def lista(self):
        with self.lock:
            return list(self.recibidos)


def configurar(srv, falso):
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
        ("x:MtaStageAuth", {"require": {"else": "false"}, "saslMechanisms": {"else": "[plain, login]"}}),
        ("x:DataRetention", {"archiveDeletedAccountsFor": HORA_MS}),
        # Todo lo que no es local va al SMTP falso.
        ("x:MtaOutboundStrategy", {"route": {"else": "'falso'", "match": {
            "0": {"if": "is_local_domain(rcpt_domain)", "then": "'local'"}}}}),
    ):
        n, r = rec.set(objeto, update={"singleton": cambios})
        assert "singleton" in (r.get("updated") or {}), f"{objeto}: {r}"
    rec.crear("x:MtaRoute", {"@type": "Relay", "name": "falso", "host": "127.0.0.1", "port": falso.puerto,
                             "protocol": "smtp", "implicitTls": False, "allowInvalidCerts": True})
    # La cola de salida (sin colas configuradas todo usa la cola «default») con dos hilos: así se nota quién va
    # primero.
    rec.crear("x:MtaVirtualQueue", {"name": "default", "threadsPerNode": 2})
    _, apps = rec.query("x:Application")
    if apps.get("ids"):
        rec.set("x:Application", update={i: {"enable": False} for i in apps["ids"]})
    srv.parar()
    srv.arrancar(recuperacion=False)
    mi.esperar_puerto(srv.p_smtp)
    mi.esperar_puerto(srv.p_imap)


_ULTIMA_CONEXION = [0.0]


def conectar(srv):
    """Abre una conexión SMTP sin pasar del límite de serie de 5 conexiones por segundo y por IP."""
    espera = _ULTIMA_CONEXION[0] + 0.25 - time.time()
    if espera > 0:
        time.sleep(espera)
    _ULTIMA_CONEXION[0] = time.time()
    return smtplib.SMTP("127.0.0.1", srv.p_smtp, timeout=30)


def enviar(srv, usuario, para, asunto, cuerpo="Hola.", conexion=None):
    """Envía autenticado por SMTP. Devuelve None si se aceptó o «código texto» del rechazo."""
    destinatarios = para if isinstance(para, list) else [para]
    msg = (f"From: <{usuario}>\r\nTo: {', '.join('<' + d + '>' for d in destinatarios)}\r\nSubject: {asunto}\r\n"
           f"Message-ID: <{time.time_ns()}@prueba>\r\n\r\n{cuerpo}\r\n")
    try:
        if conexion is not None:
            conexion.sendmail(usuario, destinatarios, msg)
            return None
        with conectar(srv) as s:
            s.ehlo("cliente.prueba")
            s.login(usuario, PW)
            s.sendmail(usuario, destinatarios, msg)
        return None
    except smtplib.SMTPRecipientsRefused as e:
        return " ".join(f"{c} {t.decode(errors='replace') if isinstance(t, bytes) else t}"
                        for c, t in e.recipients.values())
    except smtplib.SMTPResponseException as e:
        t = e.smtp_error.decode(errors="replace") if isinstance(e.smtp_error, bytes) else str(e.smtp_error)
        return f"{e.smtp_code} {t}"
    except (smtplib.SMTPException, OSError) as e:
        return repr(e)


def enviar_sin_auth(srv, de, para, asunto, cuerpo, cabeceras=""):
    msg = (f"From: <{de}>\r\nTo: <{para}>\r\nSubject: {asunto}\r\nMessage-ID: <{time.time_ns()}@informes>\r\n"
           f"{cabeceras}\r\n{cuerpo}")
    with conectar(srv) as s:
        s.ehlo("informes.example")
        s.sendmail(de, [para], msg)


def esperar(cond, segundos=60, paso=0.3):
    fin = time.time() + segundos
    while time.time() < fin:
        r = cond()
        if r:
            return r
        time.sleep(paso)
    return cond()


def en_cola(usr, filtro=None):
    n, r = usr.query("x:QueuedMessage", filtro or {})
    if n == "error":
        return None
    ids = list(r.get("ids") or [])
    if not ids:
        return {}
    _, g = usr.get("x:QueuedMessage", ids)
    return {m["id"]: m for m in g.get("list") or []}


def asunto_de_cola(usr, m):
    estado, cuerpo = usr.http("GET", f"/jmap/download/{usr.cuenta()}/{m['blobId']}/m.eml?accept=message/rfc822")
    return estado, cuerpo


def informe_dmarc(dominio, informe_id):
    return f"""<?xml version="1.0" encoding="UTF-8"?>
<feedback>
  <report_metadata><org_name>informes.example</org_name><email>noreply@informes.example</email>
    <report_id>{informe_id}</report_id><date_range><begin>1700000000</begin><end>1700086399</end></date_range>
  </report_metadata>
  <policy_published><domain>{dominio}</domain><adkim>r</adkim><aspf>r</aspf><p>none</p><sp>none</sp><pct>100</pct>
  </policy_published>
  <record><row><source_ip>192.0.2.1</source_ip><count>3</count>
    <policy_evaluated><disposition>none</disposition><dkim>pass</dkim><spf>pass</spf></policy_evaluated></row>
    <identifiers><header_from>{dominio}</header_from></identifiers>
    <auth_results><dkim><domain>{dominio}</domain><result>pass</result></dkim>
      <spf><domain>{dominio}</domain><result>pass</result></spf></auth_results></record>
</feedback>
"""


def informe_tls(dominio, informe_id):
    return json.dumps({
        "organization-name": "informes.example",
        "date-range": {"start-datetime": "2026-11-01T00:00:00Z", "end-datetime": "2026-11-01T23:59:59Z"},
        "contact-info": "tls@informes.example", "report-id": informe_id,
        "policies": [{"policy": {"policy-type": "sts", "policy-string": ["version: STSv1", "mode: testing"],
                                 "policy-domain": dominio, "mx-host": [f"mx.{dominio}"]},
                      "summary": {"total-successful-session-count": 10, "total-failure-session-count": 1},
                      "failure-details": []}],
    })


def mandar_informe(srv, para, asunto, adjunto, tipo, nombre):
    limite = "LIMITE-PRUEBA"
    cuerpo = (f"--{limite}\r\nContent-Type: text/plain\r\n\r\nInforme.\r\n"
              f"--{limite}\r\nContent-Type: {tipo}\r\nContent-Disposition: attachment; filename=\"{nombre}\"\r\n\r\n"
              f"{adjunto}\r\n--{limite}--\r\n")
    enviar_sin_auth(srv, "noreply@informes.example", para, asunto, cuerpo,
                    f"MIME-Version: 1.0\r\nContent-Type: multipart/report; report-type=x; boundary=\"{limite}\"\r\n")


def pruebas(srv, falso):
    base = f"http://127.0.0.1:{srv.p_http}"
    rec = Jmap(base, "admin", ADMIN_PW)

    print("== el operador prepara inquilinos, un revendedor, dominios y cuentas")
    dom_op = rec.crear("x:Domain", {"name": "operador.test", "dkimManagement": {"@type": "Manual"}})
    rec.crear("x:Account", {"@type": "User", "name": "root", "domainId": dom_op, "roles": {"@type": "Admin"},
                            "credentials": {"0": {"@type": "Password", "secret": PW}}})
    rec.crear("x:Account", {"@type": "User", "name": "postmaster", "domainId": dom_op,
                            "credentials": {"0": {"@type": "Password", "secret": PW}}})
    root = Jmap(base, "root@operador.test", PW)
    reventa = {"@type": "Merge", "enabledPermissions": {p: True for p in (
        "sysTenantGet", "sysTenantCreate", "sysTenantUpdate", "sysTenantDestroy", "sysTenantQuery")},
               "disabledPermissions": {}}
    t_a = root.crear("x:Tenant", {"name": "Empresa A"})
    t_b = root.crear("x:Tenant", {"name": "Empresa B"})
    t_r = root.crear("x:Tenant", {"name": "Revendedor R", "permissions": reventa})
    t_c = root.crear("x:Tenant", {"name": "Cliente C", "bezentiParentTenantId": t_r})
    doms = {}
    for nombre, t in (("a.test", t_a), ("b.test", t_b), ("c.test", t_c)):
        doms[nombre] = root.crear("x:Domain", {"name": nombre, "memberTenantId": t,
                                               "dkimManagement": {"@type": "Manual"}})

    def cuenta(nombre, dom, tenant, **extra):
        return root.crear("x:Account", {"@type": "User", "name": nombre, "domainId": doms[dom],
                                        "memberTenantId": tenant,
                                        "credentials": {"0": {"@type": "Password", "secret": PW}}, **extra})
    cuenta("admin", "a.test", t_a, roles={"@type": "Admin"})
    cuenta("admin", "b.test", t_b, roles={"@type": "Admin"})
    for nombre, dom, t in (("ana", "a.test", t_a), ("beto", "b.test", t_b), ("carlos", "c.test", t_c),
                           ("postmaster", "a.test", t_a), ("postmaster", "b.test", t_b)):
        cuenta(nombre, dom, t)
    adm_a = Jmap(base, "admin@a.test", PW)
    adm_b = Jmap(base, "admin@b.test", PW)
    ana = Jmap(base, "ana@a.test", PW)

    # --------------------------------------------------------------------------------------------------------
    print("== reparto justo: un envío masivo de un inquilino no retrasa el correo de otro")
    falso.modo, falso.retraso = "aceptar", 0.6
    errores = []
    for lote in range(8):
        with conectar(srv) as s:
            s.ehlo("cliente.prueba")
            s.login("ana@a.test", PW)
            for i in range(lote * 5, lote * 5 + 5):
                errores.append(enviar(srv, "ana@a.test", f"dest{i}@remoto.example", f"masivo-{i}", conexion=s))
    ok(not any(errores), "ana encola 40 mensajes", json.dumps([e for e in errores if e][:3]))
    t_beto = time.time()
    ok(enviar(srv, "beto@b.test", "alguien@remoto.example", "urgente-de-beto") is None, "beto envía un mensaje")
    esperar(lambda: "urgente-de-beto" in falso.asuntos(), 60)
    recibidos = falso.lista()
    pos = next((i for i, r in enumerate(recibidos) if r[3] == "urgente-de-beto"), None)
    ok(pos is not None, "el mensaje de beto se entrega")
    if pos is not None:
        delante = [r for r in recibidos[:pos] if r[0] >= t_beto and r[3].startswith("masivo-")]
        print(f"    (beto entregado en la posición {pos}, {len(delante)} de ana enviados después de él van delante)")
        ok(len(delante) <= 4,
           "el de beto sale enseguida: como mucho los de ana que ya estaban en marcha van delante",
           f"{len(delante)} de ana delante")
    esperar(lambda: sum(1 for a in falso.asuntos() if a.startswith("masivo-")) >= 40, 120)
    recibidos = falso.lista()
    pos = next((i for i, r in enumerate(recibidos) if r[3] == "urgente-de-beto"), 0)
    detras = sum(1 for r in recibidos[pos:] if r[3].startswith("masivo-"))
    print(f"    ({detras} de ana detrás del de beto)")
    ok(detras >= 10, "y la mayoría del envío masivo de ana sale después (la prueba tenía cola)",
       f"{detras} de ana detrás")
    ok(sum(1 for a in falso.asuntos() if a.startswith("masivo-")) == 40, "los 40 de ana se entregan todos")

    # --------------------------------------------------------------------------------------------------------
    print("== vista y gestión de la cola por el administrador de cada inquilino")
    falso.modo, falso.retraso = "temporal", 0.0
    for i in (1, 2, 3):
        ok(enviar(srv, "ana@a.test", f"x{i}@remoto.example", f"cola-a-{i}") is None, f"ana encola cola-a-{i}")
    ok(enviar(srv, "beto@b.test", "y@remoto.example", "cola-b-1") is None, "beto encola cola-b-1")
    time.sleep(2)
    cola_a = en_cola(adm_a)
    cola_b = en_cola(adm_b)
    cola_op = en_cola(root)
    ok(cola_a is not None and len(cola_a) == 3 and all(m.get("returnPath") == "ana@a.test" for m in cola_a.values()),
       "el administrador de A ve solo los 3 mensajes de A", json.dumps(cola_a)[:400])
    ok(cola_b is not None and len(cola_b) == 1 and all(m.get("returnPath") == "beto@b.test" for m in cola_b.values()),
       "el administrador de B ve solo el suyo", json.dumps(cola_b)[:400])
    ok(cola_op is not None and set(cola_a or {}) | set(cola_b or {}) <= set(cola_op),
       "el operador ve todos")
    ids_a = list((cola_a or {}).keys())
    n, r = adm_b.get("x:QueuedMessage", ids_a)
    ok(mi.no_encontrado(n, r, ids_a), "B pide por id los de A → notFound", json.dumps(r)[:300])
    n, r = adm_b.query("x:QueuedMessage", {"text": "ana"})
    ok(n != "error" and not r.get("ids"), "B filtra por texto del remitente de A → vacío", json.dumps(r)[:300])
    n, r = adm_b.query("x:QueuedMessage", {}, calculateTotal=True)
    ok(n != "error" and r.get("total") == 1, "B cuenta su cola: 1 (no cuenta los de A)", json.dumps(r)[:300])
    if ids_a:
        n, r = adm_b.set("x:QueuedMessage", destroy=[ids_a[0]])
        ok(ids_a[0] in (r.get("notDestroyed") or {}) and
           r["notDestroyed"][ids_a[0]].get("type") == "notFound", "B cancela uno de A → notFound", json.dumps(r)[:300])
        n, r = adm_b.set("x:QueuedMessage", update={ids_a[0]: {"nextRetry": "2000-01-01T00:00:00Z"}})
        ok(ids_a[0] in (r.get("notUpdated") or {}), "B reintenta uno de A → notFound", json.dumps(r)[:300])
        m0 = cola_a[ids_a[0]]
        estado, cuerpo = adm_a.http("GET", f"/jmap/download/{adm_a.cuenta()}/{m0['blobId']}/m.eml")
        ok(estado == 200 and b"cola-a-" in cuerpo, "A descarga el mensaje de su cola", f"{estado}")
        estado, _ = adm_b.http("GET", f"/jmap/download/{adm_b.cuenta()}/{m0['blobId']}/m.eml")
        ok(estado != 200, "B no puede descargar el de A", f"{estado}")
    n, r = ana.query("x:QueuedMessage")
    ok(n == "error" and r.get("type") == "forbidden", "un usuario sin permisos no ve la cola", json.dumps(r)[:200])
    # Cancelar uno y reintentar los otros dos.
    por_asunto = {}
    for i, m in (cola_a or {}).items():
        estado, cuerpo = adm_a.http("GET", f"/jmap/download/{adm_a.cuenta()}/{m['blobId']}/m.eml")
        for k in (1, 2, 3):
            if f"cola-a-{k}".encode() in cuerpo:
                por_asunto[k] = i
    if len(por_asunto) == 3:
        n, r = adm_a.set("x:QueuedMessage", destroy=[por_asunto[3]])
        ok(por_asunto[3] in (r.get("destroyed") or []), "A cancela cola-a-3", json.dumps(r)[:300])
        falso.modo = "aceptar"
        n, r = adm_a.set("x:QueuedMessage", update={por_asunto[k]: {"nextRetry": "2000-01-01T00:00:00Z"}
                                                    for k in (1, 2)})
        ok(set(r.get("updated") or {}) == {por_asunto[1], por_asunto[2]}, "A reintenta ya cola-a-1 y cola-a-2",
           json.dumps(r)[:300])
        ok(esperar(lambda: {"cola-a-1", "cola-a-2"} <= set(falso.asuntos()), 30),
           "los reintentados se entregan al momento")
        time.sleep(2)
        ok("cola-a-3" not in falso.asuntos(), "el cancelado no sale")
    else:
        ok(False, "A identifica sus tres mensajes por su contenido", json.dumps(por_asunto))
    falso.modo = "aceptar"
    for i in (en_cola(root) or {}):
        root.set("x:QueuedMessage", update={i: {"nextRetry": "2000-01-01T00:00:00Z"}})
    esperar(lambda: "cola-b-1" in falso.asuntos(), 30)

    # --------------------------------------------------------------------------------------------------------
    print("== límites de envío por inquilino")
    _, r = root.set("x:Tenant", update={t_a: {"bezentiMessageRate": {"count": 3, "period": HORA_MS}}})
    ok(t_a in (r.get("updated") or {}), "el operador pone a A un ritmo de 3 mensajes por hora", json.dumps(r)[:300])
    _, r = root.get("x:Tenant", [t_a], ["bezentiMessageRate", "bezentiMaxQueuedMessages"])
    ok(((r.get("list") or [{}])[0].get("bezentiMessageRate") or {}).get("count") == 3, "y se lee por la API")
    errores = [enviar(srv, "ana@a.test", f"r{i}@remoto.example", f"ritmo-{i}") for i in range(4)]
    ok(errores[:3] == [None, None, None], "los 3 primeros se aceptan", json.dumps(errores))
    ok(errores[3] is not None and errores[3].startswith("451") and "4.7.1" in errores[3],
       "el 4.º se aplaza con 451 4.7.1", str(errores[3]))
    ok(enviar(srv, "beto@b.test", "z@remoto.example", "b-sin-limite") is None, "a B no le afecta el ritmo de A")
    _, r = root.set("x:Tenant", update={t_a: {"bezentiMessageRate": None},
                                        t_b: {"bezentiRecipientRate": {"count": 3, "period": HORA_MS}}})
    ok(enviar(srv, "ana@a.test", "r9@remoto.example", "ritmo-quitado") is None, "sin ritmo, A vuelve a enviar")
    e1 = enviar(srv, "beto@b.test", ["d1@remoto.example", "d2@remoto.example"], "dest-1")
    e2 = enviar(srv, "beto@b.test", ["d3@remoto.example", "d4@remoto.example"], "dest-2")
    e3 = enviar(srv, "beto@b.test", ["d5@remoto.example"], "dest-3")
    ok(e1 is None and e2 is not None and "4.7.1" in e2 and e3 is None,
       "B con 3 destinatarios por hora: 2 sí, 2 más no, 1 más sí", json.dumps([e1, e2, e3]))
    root.set("x:Tenant", update={t_b: {"bezentiRecipientRate": None}})

    print("== límites: jerárquicos (el ritmo del revendedor frena a su cliente)")
    root.set("x:Tenant", update={t_r: {"bezentiMessageRate": {"count": 1, "period": HORA_MS}}})
    c1 = enviar(srv, "carlos@c.test", "c1@remoto.example", "cliente-1")
    c2 = enviar(srv, "carlos@c.test", "c2@remoto.example", "cliente-2")
    ok(c1 is None and c2 is not None and "4.7.1" in c2, "el cliente C del revendedor R gasta el ritmo de R",
       json.dumps([c1, c2]))
    root.set("x:Tenant", update={t_r: {"bezentiMessageRate": None}})

    print("== límites: mensajes a la vez en la cola")
    esperar(lambda: not en_cola(root), 30)
    falso.modo = "temporal"
    root.set("x:Tenant", update={t_a: {"bezentiMaxQueuedMessages": 2}})
    q = [enviar(srv, "ana@a.test", f"q{i}@remoto.example", f"en-cola-{i}") for i in range(3)]
    ok(q[0] is None and q[1] is None and q[2] is not None and q[2].startswith("452"),
       "con 2 como máximo, el 3.º se aplaza con 452", json.dumps(q))
    cola_a = en_cola(adm_a) or {}
    if cola_a:
        adm_a.set("x:QueuedMessage", destroy=[next(iter(cola_a))])
    ok(enviar(srv, "ana@a.test", "q9@remoto.example", "en-cola-9") is None,
       "al cancelar uno queda sitio otra vez (el contador se libera)")
    ok(enviar(srv, "beto@b.test", "q@remoto.example", "b-con-cola") is None, "a B no le afecta la cola llena de A")
    root.set("x:Tenant", update={t_a: {"bezentiMaxQueuedMessages": None}})
    falso.modo = "aceptar"
    for i in (en_cola(root) or {}):
        root.set("x:QueuedMessage", update={i: {"nextRetry": "2000-01-01T00:00:00Z"}})

    # --------------------------------------------------------------------------------------------------------
    print("== informes externos por inquilino")
    mandar_informe(srv, "postmaster@a.test", "Report Domain: a.test", informe_dmarc("a.test", "dmarc-a-1"),
                   "text/xml", "informe-a.xml")
    mandar_informe(srv, "postmaster@operador.test", "Report Domain: otro.example",
                   informe_dmarc("otro.example", "dmarc-otro-1"), "text/xml", "informe-otro.xml")
    mandar_informe(srv, "postmaster@b.test", "Report Domain: b.test", informe_tls("b.test", "tls-b-1"),
                   "application/tlsrpt+json", "informe-b.json")
    todos = esperar(lambda: (lambda d, t: (d, t) if len(d) >= 2 and len(t) >= 1 else None)(
        root.query("x:DmarcExternalReport")[1].get("ids") or [],
        root.query("x:TlsExternalReport")[1].get("ids") or []), 30)
    ok(todos, "el operador ve los tres informes")
    n, r = adm_a.query("x:DmarcExternalReport")
    ids_dmarc_a = list(r.get("ids") or [])
    ok(n != "error" and len(ids_dmarc_a) == 1, "A ve un informe DMARC (el de a.test)", json.dumps(r)[:300])
    if ids_dmarc_a:
        _, g = adm_a.get("x:DmarcExternalReport", ids_dmarc_a)
        inf = (g.get("list") or [{}])[0]
        ok(inf.get("memberTenantId") == t_a and "a.test" in json.dumps(inf),
           "es el de a.test y lleva el inquilino de A", json.dumps(inf)[:300])
        n, r = adm_b.get("x:DmarcExternalReport", ids_dmarc_a)
        ok(mi.no_encontrado(n, r, ids_dmarc_a), "B pide el de A por id → notFound")
        n, r = adm_b.set("x:DmarcExternalReport", destroy=ids_dmarc_a)
        ok(ids_dmarc_a[0] in (r.get("notDestroyed") or {}), "B borra el de A → notFound", json.dumps(r)[:300])
    n, r = adm_b.query("x:DmarcExternalReport")
    ok(n != "error" and not r.get("ids"), "B no ve informes DMARC (ni el de A ni el del operador)", json.dumps(r)[:300])
    n, r = adm_b.query("x:TlsExternalReport")
    ids_tls_b = list(r.get("ids") or [])
    ok(len(ids_tls_b) == 1, "B ve su informe TLS", json.dumps(r)[:300])
    n, r = adm_a.query("x:TlsExternalReport")
    ok(n != "error" and not r.get("ids"), "A no ve el informe TLS de B")
    n, r = adm_a.query("x:DmarcExternalReport", {"text": "otro.example"})
    ok(n != "error" and not r.get("ids"), "A no encuentra el del operador buscando por texto")
    n, r = adm_a.query("x:DmarcInternalReport")
    ok(n == "error" and r.get("type") == "forbidden", "los informes salientes siguen siendo del operador")
    if ids_dmarc_a:
        n, r = adm_a.set("x:DmarcExternalReport", destroy=ids_dmarc_a)
        ok(ids_dmarc_a[0] in (r.get("destroyed") or []), "A borra su informe", json.dumps(r)[:300])

    # --------------------------------------------------------------------------------------------------------
    print("== cuentas borradas: recuperación desde el administrador del inquilino")
    temporal = adm_a.crear("x:Account", {"@type": "User", "name": "temporal", "domainId": doms["a.test"],
                                         "credentials": {"0": {"@type": "Password", "secret": PW}}})
    n, r = adm_a.set("x:Account", destroy=[temporal])
    ok(temporal in (r.get("destroyed") or []), "A borra la cuenta temporal@a.test", json.dumps(r)[:300])
    n, r = adm_a.query("x:Task")
    tareas_a = list(r.get("ids") or [])
    ok(n != "error" and len(tareas_a) == 1, "A ve una cuenta borrada pendiente (tarea DestroyAccount)",
       json.dumps(r)[:300])
    if tareas_a:
        _, g = adm_a.get("x:Task", tareas_a)
        t = (g.get("list") or [{}])[0]
        ok(t.get("@type") == "DestroyAccount" and t.get("accountName") == "temporal", "con su nombre",
           json.dumps(t)[:300])
        n, r = adm_b.get("x:Task", tareas_a)
        ok(mi.no_encontrado(n, r, tareas_a), "B pide esa tarea → notFound", json.dumps(r)[:300])
        n, r = adm_b.set("x:Task", destroy=tareas_a)
        ok(tareas_a[0] in (r.get("notDestroyed") or {}), "B no puede recuperar la cuenta de A", json.dumps(r)[:300])
    n, r = adm_b.query("x:Task")
    ok(n != "error" and not r.get("ids"), "B no ve tareas (ni las de A ni las del servidor)", json.dumps(r)[:300])
    n, r = ana.query("x:Task")
    ok(n == "error", "un usuario sin permisos de cuentas no ve las tareas")
    n, r = adm_a.set("x:Task", create={"t": {"@type": "StoreMaintenance", "maintenanceType": "purgeBlob"}})
    ok(n == "error" or "t" in (r.get("notCreated") or {}), "A no puede crear tareas", json.dumps(r)[:300])
    if tareas_a:
        n, r = adm_a.set("x:Task", destroy=tareas_a)
        ok(tareas_a[0] in (r.get("destroyed") or []), "A recupera la cuenta borrando su tarea", json.dumps(r)[:300])
        _, g = adm_a.get("x:Account", [temporal], ["name", "memberTenantId"])
        cta = (g.get("list") or [{}])[0]
        ok(cta.get("name") == "temporal" and cta.get("memberTenantId") == t_a,
           "la cuenta vuelve con su id, su nombre y su inquilino", json.dumps(g)[:300])

    print("== cuentas borradas: la recuperación respeta la cuota de cuentas del inquilino")
    adm_a.set("x:Account", destroy=[temporal])
    _, r = adm_a.query("x:Account")
    usadas = len(r.get("ids") or [])
    root.set("x:Tenant", update={t_a: {"quotas": {"maxAccounts": usadas + 1}}})
    adm_a.crear("x:Account", {"@type": "User", "name": "nueva", "domainId": doms["a.test"],
                              "credentials": {"0": {"@type": "Password", "secret": PW}}})
    _, r = adm_a.query("x:Task")
    tareas_a = list(r.get("ids") or [])
    if tareas_a:
        n, r = adm_a.set("x:Task", destroy=tareas_a)
        err = (r.get("notDestroyed") or {}).get(tareas_a[0], {})
        ok(err.get("type") == "overQuota", "con la cuota llena, la recuperación responde overQuota", json.dumps(r)[:300])
    else:
        ok(False, "la segunda cuenta borrada aparece como tarea")
    root.set("x:Tenant", update={t_a: {"quotas": {}}})


def main():
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    srv = Servidor(os.path.abspath(sys.argv[1]))
    falso = SmtpFalso()
    error = None
    try:
        configurar(srv, falso)
        pruebas(srv, falso)
    except Exception as e:  # noqa: BLE001 — se informa y se guarda el registro
        error = e
        mi.FALLOS.append(f"excepción: {e!r}")
    finally:
        srv.parar()
        if mi.FALLOS:
            destino = os.path.join(tempfile.gettempdir(), "bezenti-cola-inquilinos.log")
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
