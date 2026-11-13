#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 bezenti
#
# SPDX-License-Identifier: AGPL-3.0-only
"""Pruebas de integración de la fase 5: telemetría con historial y alertas (edición bezenti).

Levanta el servidor real (binario `stalwart` con `sqlite` y `bezenti`) con una base SQLite nueva y lo prueba por
la API real (JMAP, SMTP, HTTP/SSE), solo con la biblioteca estándar de Python. Reutiliza las utilidades de
`multi_inquilino.py` (mismo directorio). Especificación: `bezenti/especificaciones/telemetria.md`.

Escenario: el operador crea los inquilinos A y B (con permisos de historial y de telemetría en vivo), sus
dominios, administradores y usuarios (ana@a.test, beto@b.test). Intervalos acortados por variables de entorno
(métricas cada 2 s, alertas cada 2 s, volcado de trazas cada 1 s). Después:

- Historial de entregas: varios correos de entrada y uno de salida a un dominio inexistente aparecen en
  `x:Trace/InboundDelivery` y `x:Trace/OutboundDelivery` con remitente, destinatarios, tamaño, cola y estado;
  filtros por texto, fecha, cola y tipo.
- Historial de métricas: `x:Metric/query`/`get` con los contadores agregados por intervalo.
- Aislamiento: cada administrador de inquilino ve solo las trazas y los contadores de su inquilino; un usuario sin
  permisos no ve nada; las alertas y los almacenes son solo del operador.
- Telemetría en vivo: token de 60 s, flujo de eventos con filtro y seguimiento de sesión, flujo de métricas; un
  administrador de inquilino solo recibe sus sesiones y no puede abrir el de métricas.
- Alertas: `metric('smtp.active-connections') > 2` se dispara al abrir 3 conexiones (webhook falso que falla la
  primera vez, para probar los reintentos, y correo a un buzón local), no se repite mientras sigue y avisa de que
  se resuelve al cerrarlas.
- Retención: con `holdTracesFor`/`holdMetricsFor` de 2 s, la purga (`purgeData`) borra lo viejo.

Uso: telemetria.py RUTA_AL_BINARIO_STALWART
"""

import base64
import http.client
import http.server
import json
import os
import shutil
import smtplib
import socket
import sqlite3
import sys
import tempfile
import threading
import time
import urllib.parse

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import multi_inquilino as mi  # noqa: E402
from multi_inquilino import ADMIN_PW, PW, USING_MAIL, Jmap, Servidor, ok  # noqa: E402

PERMISOS_TELEMETRIA = {
    "sysTraceGet": True, "sysTraceQuery": True, "sysMetricGet": True, "sysMetricQuery": True,
    "liveTracing": True, "liveMetrics": True,
}


# ------------------------------------------------------------------------------------------------------------
# Webhook falso: falla la primera petición (500) y acepta las demás.
# ------------------------------------------------------------------------------------------------------------
class Webhook:
    def __init__(self):
        self.peticiones = []
        self.lock = threading.Lock()
        padre = self

        class Manejador(http.server.BaseHTTPRequestHandler):
            def do_POST(self):  # noqa: N802
                cuerpo = self.rfile.read(int(self.headers.get("Content-Length") or 0))
                with padre.lock:
                    padre.peticiones.append(cuerpo.decode("utf-8", "replace"))
                    primera = len(padre.peticiones) == 1
                self.send_response(500 if primera else 200)
                self.send_header("Content-Length", "0")
                self.end_headers()

            def log_message(self, *a):
                pass

        self.srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Manejador)
        self.puerto = self.srv.server_address[1]
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()

    def eventos(self, tipo=None):
        """Eventos `telemetry.alert-event` recibidos con éxito (sin contar la petición que falló)."""
        with self.lock:
            peticiones = list(self.peticiones[1:])
        vistos = []
        for p in peticiones:
            try:
                for e in json.loads(p).get("events", []):
                    if e.get("type") == "telemetry.alert-event":
                        datos = json.dumps(e)
                        if tipo is None or tipo in datos:
                            vistos.append(e)
            except ValueError:
                pass
        return vistos


def configurar(srv, webhook):
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
        ("x:MtaStageAuth", {"require": {"else": "false"},
                            "saslMechanisms": {"else": "[plain, login]"}}),
        ("x:TracingStore", {"@type": "Default"}),
        ("x:MetricsStore", {"@type": "Default"}),
    ):
        n, r = rec.set(objeto, update={"singleton": cambios})
        assert "singleton" in (r.get("updated") or {}), f"{objeto}: {r}"
    _, apps = rec.query("x:Application")
    if apps.get("ids"):
        rec.set("x:Application", update={i: {"enable": False} for i in apps["ids"]})
    # Webhook para los eventos de alerta (con reintentos: el falso falla la primera vez).
    rec.crear("x:WebHook", {
        "url": f"http://127.0.0.1:{webhook.puerto}/alertas", "enable": True, "level": "info",
        "events": {"telemetry.alert-event": True}, "eventsPolicy": "include",
        "throttle": 500, "discardAfter": 600000, "timeout": 5000,
    })
    # La alerta: más de 2 conexiones SMTP abiertas.
    rec.crear("x:Alert", {
        "enable": True,
        "condition": {"else": "metric('smtp.active-connections') > 2"},
        "emailAlert": {"@type": "Enabled", "fromAddress": "alertas@operador.test", "fromName": "Vigilante",
                       "to": {"root@operador.test": True},
                       "subject": "Conexiones SMTP: %{smtp.active-connections}%",
                       "body": "Hay %{smtp.active-connections}% conexiones SMTP abiertas. %{no.existe}%"},
        "eventAlert": {"@type": "Enabled", "eventMessage": "conexiones=%{smtp.active-connections}%"},
    })
    # Una condición con una métrica que no existe se rechaza.
    n, r = rec.set("x:Alert", create={"m": {"condition": {"else": "metric('no.existe') > 1"},
                                            "eventAlert": {"@type": "Enabled", "eventMessage": "x"}}})
    ok("m" in (r.get("notCreated") or {}) and "metric" in json.dumps(r).lower(),
       "x:Alert con una métrica inexistente → rechazada", json.dumps(r)[:300])
    srv.parar()
    reiniciar(srv)


def reiniciar(srv):
    srv.parar()
    srv.arrancar(recuperacion=False)
    mi.esperar_puerto(srv.p_smtp)
    mi.esperar_puerto(srv.p_imap)


def enviar(srv, para, asunto, remitente="remitente@externo.example", cuerpo="Hola.", auth=None):
    msg = (f"From: <{remitente}>\r\nTo: <{para}>\r\nSubject: {asunto}\r\n"
           f"Message-ID: <{time.time_ns()}@externo.example>\r\n\r\n{cuerpo}\r\n")
    try:
        with smtplib.SMTP("127.0.0.1", srv.p_smtp, timeout=30) as s:
            s.ehlo("externo.example")
            if auth:
                s.login(*auth)
            s.sendmail(remitente, [para], msg)
        return None
    except (smtplib.SMTPException, OSError) as e:
        return repr(e) or "error"


def esperar(cond, segundos=60, paso=0.5):
    fin = time.time() + segundos
    while time.time() < fin:
        r = cond()
        if r:
            return r
        time.sleep(paso)
    return cond()


def trazas(usr, filtro=None):
    n, r = usr.query("x:Trace", filtro or {})
    return n, r


def ids(usr, objeto, filtro=None):
    n, r = usr.query(objeto, filtro or {})
    return list(r.get("ids") or []) if n != "error" else None


def obtener(usr, objeto, lista):
    if not lista:
        return []
    out = []
    for i in range(0, len(lista), 200):
        _, r = usr.get(objeto, lista[i:i + 200])
        out.extend(r.get("list") or [])
    return out


def lista(v):
    """Las listas de objetos del registro viajan como mapa {"0": …, "1": …}."""
    if isinstance(v, dict):
        return [v[k] for k in sorted(v, key=lambda k: int(k) if k.isdigit() else k)]
    return list(v or [])


def nombres_eventos(traza):
    return [e.get("event") for e in lista(traza.get("events"))]


def buscar_correo(usr, asunto):
    _, r = usr.una("Email/query", {"accountId": usr.cuenta(), "filter": {"subject": asunto}}, USING_MAIL)
    return list(r.get("ids") or [])


class Flujo:
    """Lee un flujo SSE en segundo plano."""

    def __init__(self, puerto, ruta, auth=None):
        self.lineas = []
        self.estado = None
        self.parar = False
        self.conn = http.client.HTTPConnection("127.0.0.1", puerto, timeout=1)
        cabeceras = {"Authorization": auth} if auth else {}
        self.conn.request("GET", ruta, headers=cabeceras)
        resp = self.conn.getresponse()
        self.conn.sock.settimeout(None)
        self.estado = resp.status
        self.resp = resp
        if resp.status == 200:
            threading.Thread(target=self._leer, daemon=True).start()
        else:
            self.cuerpo = resp.read().decode("utf-8", "replace")

    def _leer(self):
        while not self.parar:
            try:
                linea = self.resp.readline()
            except (OSError, ValueError, AttributeError):
                break
            if not linea:
                break
            self.lineas.append(linea.decode("utf-8", "replace").rstrip("\n"))

    def texto(self):
        return "\n".join(self.lineas)

    def cerrar(self):
        self.parar = True
        try:
            self.conn.sock.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        self.conn.close()


def basic(usuario, clave):
    return "Basic " + base64.b64encode(f"{usuario}:{clave}".encode()).decode()


def pruebas(srv, webhook):
    base = f"http://127.0.0.1:{srv.p_http}"

    print("== el operador prepara inquilinos, dominios y cuentas")
    rec = Jmap(base, "admin", ADMIN_PW)
    dom_op = rec.crear("x:Domain", {"name": "operador.test", "dkimManagement": {"@type": "Manual"}})
    rec.crear("x:Account", {"@type": "User", "name": "root", "domainId": dom_op, "roles": {"@type": "Admin"},
                            "credentials": {"0": {"@type": "Password", "secret": PW}}})
    root = Jmap(base, "root@operador.test", PW)
    # Sin límites de conexiones por IP (la prueba abre muchas desde 127.0.0.1; se crean al primer arranque).
    _, lim = root.query("x:MtaInboundThrottle")
    if lim.get("ids"):
        root.set("x:MtaInboundThrottle", update={i: {"enable": False} for i in lim["ids"]})
        root.set("x:Action", create={"r": {"@type": "ReloadSettings"}})
    permisos = {"@type": "Merge", "enabledPermissions": PERMISOS_TELEMETRIA, "disabledPermissions": {}}
    t_a = root.crear("x:Tenant", {"name": "Empresa A", "permissions": permisos})
    t_b = root.crear("x:Tenant", {"name": "Empresa B", "permissions": permisos})
    dom_a = root.crear("x:Domain", {"name": "a.test", "memberTenantId": t_a, "dkimManagement": {"@type": "Manual"}})
    dom_b = root.crear("x:Domain", {"name": "b.test", "memberTenantId": t_b, "dkimManagement": {"@type": "Manual"}})
    cuenta = lambda nombre, dom, tenant, **extra: root.crear("x:Account", {  # noqa: E731
        "@type": "User", "name": nombre, "domainId": dom, "memberTenantId": tenant,
        "credentials": {"0": {"@type": "Password", "secret": PW}}, **extra})
    cuenta("admin", dom_a, t_a, roles={"@type": "Admin"})
    cuenta("admin", dom_b, t_b, roles={"@type": "Admin"})
    cuenta("ana", dom_a, t_a)
    cuenta("beto", dom_b, t_b)
    adm_a = Jmap(base, "admin@a.test", PW)
    adm_b = Jmap(base, "admin@b.test", PW)
    ana = Jmap(base, "ana@a.test", PW)
    beto = Jmap(base, "beto@b.test", PW)

    # --------------------------------------------------------------------------------------------------------
    print("== historial de entregas: varios correos de entrada y uno de salida")
    inicio = time.time()
    for i in range(3):
        r = enviar(srv, "ana@a.test", f"Para ana {i}", remitente=f"emisor{i}@externo.example")
        ok(r is None, f"SMTP a ana@a.test ({i + 1}/3) → aceptado", str(r))
    for i in range(2):
        r = enviar(srv, "beto@b.test", f"Para beto {i}", remitente="otro@externo.example")
        ok(r is None, f"SMTP a beto@b.test ({i + 1}/2) → aceptado", str(r))
    r = enviar(srv, "destino@remoto.invalid", "Hacia fuera", remitente="ana@a.test", auth=("ana@a.test", PW))
    ok(r is None, "envío autenticado de ana a un dominio inexistente → aceptado", str(r))
    ok(esperar(lambda: len(buscar_correo(ana, "Para ana")) == 3), "los 3 correos llegan a ana")
    ok(esperar(lambda: len(buscar_correo(beto, "Para beto")) == 2), "los 2 correos llegan a beto")

    entrada = esperar(lambda: (lambda l: l if l and len(l) >= 6 else None)(
        ids(root, "x:Trace", {"event": "smtp.connection-start"})), 30)
    entrada = entrada or ids(root, "x:Trace", {"event": "smtp.connection-start"}) or []
    ok(len(entrada) >= 6, "x:Trace/InboundDelivery: una traza por sesión SMTP con mensaje (≥6)", str(entrada))
    tr_in = obtener(root, "x:Trace", entrada)
    de_ana = [t for t in tr_in if "ana@a.test" in t.get("to", "") and t.get("from", "").startswith("emisor")]
    de_beto = [t for t in tr_in if "beto@b.test" in t.get("to", "")]
    ok(len(de_ana) == 3 and len(de_beto) == 2, "las trazas llevan remitente y destinatarios",
       json.dumps([(t.get("from"), t.get("to")) for t in tr_in]))
    if de_ana:
        t = de_ana[0]
        nombres = nombres_eventos(t)
        ok(t.get("size", 0) > 0 and t.get("timestamp", "").endswith("Z"), "con tamaño y fecha", json.dumps(t)[:300])
        ok(nombres[:1] == ["smtp.connection-start"] and "queue.message-queued" in nombres
           and nombres[-1] == "smtp.connection-end", "con los eventos de la sesión, de principio a fin",
           str(nombres))
        ok(not any(n and n.endswith("raw-input") for n in nombres), "sin los datos del protocolo")
        mf = [e for e in lista(t["events"]) if e["event"] == "smtp.mail-from"]
        ok(mf and any(kv["key"] == "from" and kv["value"].get("@type") == "String"
                      for kv in lista(mf[0]["keyValues"])),
           "valores tipados (@type) en los pares clave/valor", json.dumps(mf)[:300])

    salida = esperar(lambda: (lambda l: l if l and len(l) >= 6 else None)(
        ids(root, "x:Trace", {"event": "delivery.attempt-start"})), 60)
    salida = salida or ids(root, "x:Trace", {"event": "delivery.attempt-start"}) or []
    ok(len(salida) >= 6, "x:Trace/OutboundDelivery: un intento de entrega por mensaje (≥6)", str(salida))
    tr_out = obtener(root, "x:Trace", salida)
    local = [t for t in tr_out if "ana@a.test" in t.get("to", "")]
    fuera = [t for t in tr_out if "remoto.invalid" in t.get("to", "")]
    ok(local and "delivery.attempt-end" in nombres_eventos(local[0]),
       "entrega local: traza de salida con su estado", str(nombres_eventos(local[0]) if local else tr_out[:1]))
    if local:
        print("    eventos de una entrega local:", nombres_eventos(local[0]))
    ok(fuera, "envío a dominio inexistente: traza de salida", json.dumps([t.get("to") for t in tr_out]))
    if fuera:
        n = nombres_eventos(fuera[0])
        print("    eventos del envío fallido:", n)
        ok(fuera[0].get("from") == "ana@a.test" and any("error" in x or "fail" in x or "lookup" in x for x in n),
           "y con el motivo del fallo", str(n))

    print("== historial de entregas: filtros")
    n_beto = ids(root, "x:Trace", {"text": "BETO@b.test", "event": "smtp.connection-start"})
    ok(n_beto is not None and len(n_beto) == 2, "texto (sin distinguir mayúsculas) → solo las de beto", str(n_beto))
    ok(ids(root, "x:Trace", {"timestampIsGreaterThan": "2099-01-01T00:00:00Z"}) == [],
       "fecha posterior a todo → nada")
    hoy = time.strftime("%Y-%m-%d", time.gmtime())
    ok(len(ids(root, "x:Trace", {"timestamp": hoy}) or []) >= 12, "fecha de hoy (día entero) → todas")
    ok(ids(root, "x:Trace", {"timestamp": "2020-01-01"}) == [], "otro día → nada")
    desde = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(inicio - 2))
    ok(len(ids(root, "x:Trace", {"timestampIsGreaterThanOrEqual": desde}) or []) >= 12, "desde el inicio de la prueba")
    if fuera:
        qid = [kv["value"]["value"] for e in lista(fuera[0]["events"]) for kv in lista(e["keyValues"])
               if kv["key"] == "queueId"]
        por_cola = ids(root, "x:Trace", {"queueId": str(qid[0]), "event": "delivery.attempt-start"}) if qid else []
        ok(por_cola == [fuera[0]["id"]], "por id de cola (decimal) → el intento de esa cola", str(por_cola))
    # Un mensaje que se queda en la cola (cola en pausa): «Delivery History» del panel usa su id.
    root.set("x:Action", create={"p": {"@type": "PauseMtaQueue"}})
    r = enviar(srv, "lento@remoto.invalid", "En cola", remitente="ana@a.test", auth=("ana@a.test", PW))
    cola = esperar(lambda: ids(root, "x:QueuedMessage"), 10)
    ok(r is None and cola, "un envío autenticado queda en la cola", f"{r} {cola}")
    if cola:
        por_cola = esperar(lambda: ids(root, "x:Trace", {"queueId": cola[0]}), 10)
        tr = obtener(root, "x:Trace", por_cola or [])
        ok(tr and all("lento@remoto.invalid" in t.get("to", "") for t in tr),
           "por cola (id de x:QueuedMessage, como «Delivery History» del panel)", str(por_cola))
        root.set("x:QueuedMessage", destroy=cola)
    root.set("x:Action", create={"r": {"@type": "ResumeMtaQueue"}})
    ok(ids(root, "x:Trace", {"queueId": "zzzz-no-existe"}) == [], "cola inexistente → nada")
    asc = ids(root, "x:Trace", None) or []
    _, r = root.query("x:Trace", {}, sort=[{"property": "timestamp", "isAscending": True}])
    ok(list(r.get("ids") or []) == list(reversed(asc)), "orden por fecha ascendente/descendente")
    _, r = root.query("x:Trace", {}, limit=2, position=1)
    ok(list(r.get("ids") or []) == asc[1:3], "paginación (position/limit)", json.dumps(r)[:200])
    n, r = root.set("x:Trace", destroy=[entrada[0]], create={"x": {"from": "a@b.c"}})
    ok(n == "error" or (entrada[0] in (r.get("notDestroyed") or {}) and "x" in (r.get("notCreated") or {})),
       "las trazas no se pueden borrar ni crear por la API", json.dumps(r)[:200])
    ok(entrada[0] in (ids(root, "x:Trace") or []), "y la traza sigue ahí")

    print("== historial de entregas: aislamiento entre inquilinos")
    vis_a = ids(adm_a, "x:Trace") or []
    vis_b = ids(adm_b, "x:Trace") or []
    tr_a = obtener(adm_a, "x:Trace", vis_a)
    tr_b = obtener(adm_b, "x:Trace", vis_b)
    ok(tr_a and all("beto@b.test" not in json.dumps(t) for t in tr_a),
       "el admin de A ve sus trazas y ninguna de beto", str(len(tr_a)))
    ok(tr_b and all("ana@a.test" not in json.dumps(t) for t in tr_b),
       "el admin de B ve sus trazas y ninguna de ana", str(len(tr_b)))
    ok(len([t for t in tr_a if "ana@a.test" in t.get("to", "")]) >= 6 and fuera and fuera[0]["id"] in vis_a,
       "A ve entrada y salida de ana (también el envío autenticado)", str(len(tr_a)))
    ajenas = [t["id"] for t in de_beto]
    _, r = adm_a.get("x:Trace", ajenas)
    ok(mi.no_encontrado("x:Trace/get", r, ajenas), "get de trazas de B desde A → notFound")
    ok(ids(adm_a, "x:Trace", {"text": "beto"}) == [], "buscar «beto» desde A → nada")
    n, r = ana.query("x:Trace", {})
    ok(n == "error" and r.get("type") == "forbidden", "un usuario sin permisos no ve el historial", json.dumps(r))
    for objeto in ("x:Alert", "x:TracingStore", "x:MetricsStore"):
        n, r = adm_a.get(objeto)
        ok(n == "error" and r.get("type") == "forbidden", f"{objeto} es solo del operador", json.dumps(r)[:200])

    # --------------------------------------------------------------------------------------------------------
    print("== historial de métricas")
    time.sleep(5)

    def puntos(usr, filtro=None):
        lista = ids(usr, "x:Metric", filtro) or []
        return obtener(usr, "x:Metric", lista)

    encolados = esperar(lambda: (lambda p: p if sum(x.get("count", 0) for x in p) >= 5 else None)(
        puntos(root, {"metric": "queue.message-queued"})), 20) or []
    total = sum(x.get("count", 0) for x in encolados)
    ok(total >= 5, "x:Metric: queue.message-queued agregado por intervalos (diferencias)", str(encolados)[:300])
    ok(all(p.get("@type") == "Counter" and p.get("nodeId") == 0 for p in encolados),
       "los contadores son Counter del nodo 0")
    todos = puntos(root, {"nodeId": 0})
    tipos = {p.get("@type") for p in todos}
    ok({"Counter", "Gauge"} <= tipos, "hay contadores y medidores", str(tipos))
    hist = [p for p in todos if p.get("@type") == "Histogram"]
    ok(hist and all("sum" in p and p["count"] > 0 for p in hist), "y histogramas con count y sum", str(hist[:2]))
    ok(not any(p.get("metric") == "cache.swap-pending" for p in todos), "sin cache.swap-pending (documentación)")
    ok(ids(root, "x:Metric", {"nodeId": 7}) == [], "filtro por otro nodo → nada")
    ok(ids(root, "x:Metric", {"timestampIsGreaterThan": "2099-01-01T00:00:00Z"}) == [], "filtro por fecha")

    p_a = puntos(adm_a, {"metric": "queue.message-queued"})
    p_b = puntos(adm_b, {"metric": "queue.message-queued"})
    s_a = sum(p.get("count", 0) for p in p_a)
    s_b = sum(p.get("count", 0) for p in p_b)
    ok(s_a == 3 and s_b == 2, "cada inquilino ve solo los contadores de sus trazas (A=3, B=2)", f"A={s_a} B={s_b}")
    todo_a = puntos(adm_a)
    ok(todo_a and all(p.get("@type") == "Counter" for p in todo_a),
       "un inquilino no ve medidores ni histogramas del servidor", str({p.get("@type") for p in todo_a}))
    if encolados:
        _, r = adm_a.get("x:Metric", [encolados[0]["id"]])
        lst = r.get("list") or []
        ok(not lst or lst[0].get("count", 0) <= 3, "get de un punto del servidor desde A → solo su parte")
    n, r = ana.query("x:Metric", {})
    ok(n == "error" and r.get("type") == "forbidden", "un usuario sin permisos no ve métricas")

    # --------------------------------------------------------------------------------------------------------
    print("== telemetría en vivo")
    estado, cuerpo = root.http("GET", "/api/token/tracing")
    ok(estado == 200 and len(cuerpo) > 20, "token de 60 s para el flujo de eventos", str(estado))
    token = cuerpo.decode()
    estado, _ = ana.http("GET", "/api/token/tracing")
    ok(estado in (401, 403), "un usuario sin liveTracing no obtiene token", str(estado))
    flujo = Flujo(srv.p_http, "/api/live/tracing?token=" + urllib.parse.quote(token) + "&filter=vivo%40externo")
    ok(flujo.estado == 200, "flujo con token en la URL (EventSource)", str(flujo.estado))
    flujo_a = Flujo(srv.p_http, "/api/live/tracing", auth=basic("admin@a.test", PW))
    ok(flujo_a.estado == 200, "flujo del admin de A con cabecera Authorization", str(flujo_a.estado))
    time.sleep(1)
    enviar(srv, "ana@a.test", "En vivo A", remitente="vivo@externo.example")
    enviar(srv, "beto@b.test", "En vivo B", remitente="nadie@externo.example")
    esperar(lambda: "smtp.rcpt-to" in flujo.texto() and "beto@b.test" not in flujo_a.texto()
            and "ana@a.test" in flujo_a.texto(), 15)
    time.sleep(2)
    t = flujo.texto()
    ok(t.startswith("event: ping\ndata: {\"interval\": 30000}"), "empieza con ping (intervalo 30 s)", t[:80])
    ok("event: trace" in t and "smtp.mail-from" in t and "vivo@externo.example" in t,
       "eventos en lotes `trace` con el filtro", t[:300])
    ok("smtp.rcpt-to" in t and "ana@a.test" in t, "y los siguientes eventos de la misma sesión")
    ok("nadie@externo.example" not in t and "beto@b.test" not in t, "sin las sesiones que no coinciden")
    datos = [json.loads(linea[6:]) for linea in t.splitlines() if linea.startswith("data: [")]
    ok(datos and isinstance(datos[0][0].get("keyValues"), list)
       and all("@type" in kv["value"] for kv in datos[0][0]["keyValues"]),
       "formato de la documentación: keyValues en lista, valores con @type", t[:300])
    ta = flujo_a.texto()
    ok("ana@a.test" in ta and "smtp.connection-start" in ta, "el admin de A recibe sus sesiones completas", ta[:200])
    ok("beto@b.test" not in ta and "nadie@externo.example" not in ta, "y ninguna de B", ta[-300:])
    flujo.cerrar()
    flujo_a.cerrar()
    estado, cuerpo = root.http("GET", "/api/token/tracing")
    otro_token = cuerpo.decode()
    m = Flujo(srv.p_http, "/api/live/metrics?token=" + urllib.parse.quote(otro_token))
    ok(m.estado in (401, 403), "un token de trazas no vale para métricas", str(m.estado))
    m = Flujo(srv.p_http, "/api/live/metrics?metrics=smtp.active-connections,queue.message-queued,nada&interval=1",
              auth=basic("root@operador.test", PW))
    ok(m.estado == 200, "flujo de métricas del operador")
    esperar(lambda: m.texto().count("event: metrics") >= 2, 10)
    tm = m.texto()
    m.cerrar()
    datos = [json.loads(linea[6:]) for linea in tm.splitlines() if linea.startswith("data: ")]
    ok(tm.count("event: metrics") >= 2, "actualizaciones cada `interval` s", tm[:200])
    ok(datos and {d["metric"] for d in datos[0]} <= {"smtp.active-connections", "queue.message-queued"}
       and any(d["metric"] == "smtp.active-connections" and d["@type"] == "Gauge" for d in datos[0]),
       "solo las métricas pedidas, con su tipo", json.dumps(datos[:1]))
    estado, _ = adm_a.http("GET", "/api/token/metrics")
    ok(estado == 403, "el admin de un inquilino no puede ver las métricas en vivo del servidor", str(estado))
    m = Flujo(srv.p_http, "/api/live/metrics", auth=basic("admin@a.test", PW))
    ok(m.estado == 403, "ni abriendo el flujo directamente", str(m.estado))

    # --------------------------------------------------------------------------------------------------------
    print("== alertas (webhook con reintentos y correo a un buzón local)")
    conexiones = []
    for _ in range(3):
        s = smtplib.SMTP("127.0.0.1", srv.p_smtp, timeout=30)
        s.ehlo("abiertas.example")
        conexiones.append(s)
    disparo = esperar(lambda: webhook.eventos("triggered"), 30)
    ok(disparo, "al superar el umbral llega el evento por webhook", str(webhook.peticiones[:2]))
    ok(len(webhook.peticiones) >= 2, "tras un primer intento fallido (500), el webhook reintenta")
    if disparo:
        ok("conexiones=3" in json.dumps(disparo[0]), "con el valor de la métrica en el mensaje",
           json.dumps(disparo[0])[:300])
    correo = esperar(lambda: buscar_correo(root, "Conexiones SMTP"), 30)
    ok(correo, "y el correo de alerta llega al buzón local del operador")
    time.sleep(7)
    ok(len(webhook.eventos("triggered")) == 1 and len(buscar_correo(root, "Conexiones SMTP")) == 1,
       "mientras sigue, no se repite (histéresis)",
       f"eventos={len(webhook.eventos('triggered'))} correos={len(buscar_correo(root, 'Conexiones SMTP'))}")
    if correo:
        _, r = root.una("Email/get", {"accountId": root.cuenta(), "ids": correo[:1],
                                      "properties": ["subject", "from", "header:Auto-Submitted:asText",
                                                     "preview"]}, USING_MAIL)
        c = (r.get("list") or [{}])[0]
        ok(c.get("subject") == "Conexiones SMTP: 3"
           and (c.get("from") or [{}])[0].get("email") == "alertas@operador.test"
           and (c.get("header:Auto-Submitted:asText") or "").strip() == "auto-generated",
           "asunto con el valor, remitente y Auto-Submitted", json.dumps(c))
        ok("%{no.existe}%" in (c.get("preview") or ""), "un nombre desconocido se deja tal cual", c.get("preview"))
    for s in conexiones:
        s.quit()
    resuelta = esperar(lambda: webhook.eventos("resolved"), 30)
    ok(resuelta, "al bajar del umbral llega el aviso de resuelta")
    ok(esperar(lambda: len(buscar_correo(root, "Conexiones SMTP")) == 2, 30)
       and buscar_correo(root, "Resolved"), "y el correo de resuelta")
    time.sleep(5)
    ok(len(webhook.eventos("triggered")) == 1 and len(webhook.eventos("resolved")) == 1,
       "sin repetirse en bucle", f"{len(webhook.eventos('triggered'))}/{len(webhook.eventos('resolved'))}")

    # --------------------------------------------------------------------------------------------------------
    try:
        db = sqlite3.connect(os.path.join(srv.dir, "datos"))
        for tabla, que in (("o", "trazas"), ("x", "métricas")):
            nk, b = db.execute(f"SELECT count(*), coalesce(sum(length(k) + length(v)), 0) FROM {tabla}").fetchone()
            print(f"    espacio del historial de {que}: {nk} claves, {b} bytes")
        nt = len(ids(root, "x:Trace") or [])
        print(f"    {nt} trazas → {b and round(db.execute('SELECT sum(length(k)+length(v)) FROM o').fetchone()[0] / max(nt, 1))} bytes por traza")
        db.close()
    except sqlite3.Error as e:
        print("    (no se pudo medir el espacio)", e)

    print("== retención y purga")
    viejas = ids(root, "x:Trace") or []
    viejos = ids(root, "x:Metric", {"metric": "queue.message-queued"}) or []
    _, r = root.set("x:DataRetention", update={"singleton": {"holdTracesFor": 3000, "holdMetricsFor": 3000}})
    ok("singleton" in (r.get("updated") or {}), "retención de 3 s", json.dumps(r)[:200])
    reiniciar(srv)
    time.sleep(4)
    ahora = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    _, r = root.set("x:Task", create={"p": {"@type": "StoreMaintenance", "maintenanceType": "purgeData",
                                            "status": {"@type": "Pending", "due": ahora}}})
    ok("p" in (r.get("created") or {}), "tarea de purga de datos (purgeData)", json.dumps(r)[:300])
    quedan = esperar(lambda: (lambda q: q if q is not None and not (set(q) & set(viejas)) else None)(
        ids(root, "x:Trace")), 60)
    ok(quedan is not None and viejas, "las trazas viejas se purgan", str(len(viejas)))
    _, r = root.get("x:Trace", viejas[:5])
    ok(mi.no_encontrado("x:Trace/get", r, viejas[:5]), "y get de una traza purgada → notFound")
    nuevos = ids(root, "x:Metric", {"metric": "queue.message-queued"}) or []
    ok(viejos and not (set(nuevos) & set(viejos)), "las métricas viejas se purgan", f"{len(viejos)} → {len(nuevos)}")


def main():
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    os.environ.update({"BEZENTI_METRICAS_INTERVALO": "2", "BEZENTI_ALERTAS_INTERVALO": "2",
                       "BEZENTI_TRAZAS_VOLCADO": "1"})
    srv = Servidor(os.path.abspath(sys.argv[1]))
    webhook = Webhook()
    error = None
    try:
        configurar(srv, webhook)
        pruebas(srv, webhook)
    except Exception as e:  # noqa: BLE001 — se informa y se guarda el registro
        error = e
        mi.FALLOS.append(f"excepción: {e!r}")
    finally:
        srv.parar()
        if mi.FALLOS:
            destino = os.path.join(tempfile.gettempdir(), "bezenti-telemetria.log")
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
