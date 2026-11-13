#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 bezenti
#
# SPDX-License-Identifier: AGPL-3.0-only
"""Pruebas de integración de la IA (edición bezenti, fase 4).

Levanta el servidor real (binario `stalwart` compilado con `sqlite` y `bezenti`) con una base SQLite nueva y un
**servidor HTTP falso** que imita a cada proveedor (OpenAI Chat Completions, Anthropic Messages, Gemini
generateContent, Ollama /api/chat y TypeSafe /v1/systemone), con respuestas buenas, errores, lentitud y JSON
inválido. Solo usa la biblioteca estándar de Python.

Se prueba por la API real (JMAP, SMTP):
- cada proveedor como único nivel: cabeceras de autenticación, salida estructurada, hechos enviados (= aiFacts),
  etiquetas AI_*, puntuación ajustada y cabecera X-Spam-AI (acción x:Action ClassifySpam);
- logprobs, escalado a un segundo nivel, modo Shadow, compuerta (condición y remitente autenticado);
- fallos: HTTP 500 (con un reintento), 401 (cortacircuitos abierto al instante → «unavailable»), lentitud por
  encima del plazo, JSON inválido → AI_ERROR sin bloquear nada;
- SMTP de extremo a extremo: correo dudoso → llamada al modelo → puntuación y cabecera en el buzón; modelo caído →
  el correo se entrega igual;
- Sieve: script de usuario con llm_prompt que archiva según la respuesta; sin modelo → se queda en la bandeja;
- errores de configuración (extraParameters que no es un objeto → recarga rechazada);
- la clave API nunca aparece en el registro del servidor.

Uso: ia.py RUTA_AL_BINARIO_STALWART
"""

import base64
import json
import os
import shutil
import signal
import smtplib
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

USING = ["urn:ietf:params:jmap:core", "urn:stalwart:jmap"]
USING_MAIL = ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"]
USING_SIEVE = ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:sieve"]
ADMIN_PW = "Rec0very-Adm1n-Secret!"
PW = "Cl4ve-Muy-Segura-2026!"
CLAVE = "sk-prueba-NO-DEBE-SALIR-EN-EL-LOG-7f3a"

FALLOS = []
PROBADOS = []


def ok(cond, nombre, detalle=""):
    PROBADOS.append(nombre)
    if cond:
        print(f"  ok    {nombre}")
    else:
        FALLOS.append(nombre)
        print(f"  FALLO {nombre} {detalle}")


def puerto_libre():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def esperar_puerto(puerto, segundos=120):
    fin = time.time() + segundos
    while time.time() < fin:
        try:
            with socket.create_connection(("127.0.0.1", puerto), timeout=1):
                return
        except OSError:
            time.sleep(0.3)
    raise SystemExit(f"el puerto {puerto} no abrió en {segundos} s")


# ----------------------------------------------------------------------------------------------------------------
# Servidor falso de proveedores de IA
# ----------------------------------------------------------------------------------------------------------------

PETICIONES = []  # (proveedor, ruta, cabeceras, cuerpo)
CERROJO = threading.Lock()

VEREDICTOS = {
    "phish": {"category": "phishing", "confidence": "high",
              "signals": {"impersonation": True, "credential_request": True, "requested": False},
              "explanation": "Fake bank login page"},
    "ham": {"category": "legitimate", "confidence": "high",
            "signals": {"impersonation": False, "credential_request": False, "requested": True}},
    "unsure": {"category": "newsletter", "confidence": "low",
               "signals": {"impersonation": False, "credential_request": False, "requested": False}},
}


def comportamiento(modelo):
    """Nombre del modelo → qué hace el proveedor falso."""
    for clave in ("slow", "badjson", "garbage", "e500", "e401", "logprobs"):
        if modelo.startswith(clave):
            return clave
    for clave in VEREDICTOS:
        if modelo.startswith(clave):
            return clave
    return "phish"


class Falso(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def responder(self, estado, cuerpo):
        datos = cuerpo if isinstance(cuerpo, bytes) else json.dumps(cuerpo).encode()
        self.send_response(estado)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(datos)))
        self.end_headers()
        try:
            self.wfile.write(datos)
        except OSError:
            pass

    def do_POST(self):
        largo = int(self.headers.get("Content-Length") or 0)
        crudo = self.rfile.read(largo)
        try:
            cuerpo = json.loads(crudo)
        except ValueError:
            cuerpo = None
        ruta = self.path
        proveedor = ruta.strip("/").split("/")[0]
        with CERROJO:
            PETICIONES.append((proveedor, ruta, {k.lower(): v for k, v in self.headers.items()}, cuerpo))
        if cuerpo is None:
            return self.responder(400, {"error": {"message": "invalid json"}})

        if proveedor == "gemini":
            modelo = ruta.split("/models/")[1].split(":")[0]
        else:
            modelo = cuerpo.get("model", "")
        modo = comportamiento(modelo)

        if modo == "slow":
            time.sleep(6)
            modo = "phish"
        if modo == "e500":
            return self.responder(500, {"error": {"message": "boom", "type": "server_error"}})
        if modo == "e401":
            return self.responder(401, {"error": {"message": "invalid api key", "type": "authentication_error"}})
        if modo == "badjson":
            return self.responder(200, b"{esto no es json")

        estructurado = any(k in cuerpo for k in ("response_format", "json_schema", "format", "output_config")) or (
            proveedor == "gemini" and "responseJsonSchema" in cuerpo.get("generationConfig", {}))
        if proveedor == "typesafe":
            return self.responder(200, {
                "model": "jev-1.14",
                "answers": dict(
                    {"category": {"type": "choice", "choice": "phishing", "confidence": 0.9,
                                  "probabilities": {"phishing": 0.93, "legitimate": 0.02}}},
                    **{q: {"type": "noul", "noul": 0.9 if q in ("signal_impersonation",) else 0.1}
                       for q in cuerpo.get("questions", {}) if q != "category"}),
                "usage": {"input_tokens": 321, "output_tokens": 0},
            })

        if modo == "garbage":
            texto = "I think this is spam, honestly."
        elif not estructurado:
            texto = "Work"  # llm_prompt (Sieve)
        elif modo == "logprobs":
            texto = json.dumps(VEREDICTOS["phish"])
        else:
            texto = json.dumps(VEREDICTOS[modo])

        if proveedor == "openai":
            eleccion = {"index": 0, "finish_reason": "stop", "message": {"role": "assistant", "content": texto}}
            if modo == "logprobs":
                partes = ['{"', 'category', '":"', 'phish', 'ing', '", ', texto[len('{"category": "phishing", '):]]
                eleccion["message"]["content"] = "".join(partes)
                eleccion["logprobs"] = {"content": [
                    {"token": t, "logprob": (-0.03 if t == "phish" else (-0.02 if t == "ing" else 0.0)),
                     "top_logprobs": ([{"token": "phish", "logprob": -0.03}, {"token": "legit", "logprob": -3.6}]
                                      if t == "phish" else [])}
                    for t in partes]}
            return self.responder(200, {"id": "chatcmpl-falso", "model": modelo + "-2026", "choices": [eleccion],
                                        "usage": {"prompt_tokens": 500, "completion_tokens": 40,
                                                  "prompt_tokens_details": {"cached_tokens": 128}}})
        if proveedor == "anthropic":
            return self.responder(200, {"id": "msg_falso", "type": "message", "role": "assistant", "model": modelo,
                                        "content": [{"type": "text", "text": texto}], "stop_reason": "end_turn",
                                        "usage": {"input_tokens": 480, "output_tokens": 35,
                                                  "cache_read_input_tokens": 0}})
        if proveedor == "gemini":
            return self.responder(200, {"responseId": "r-falso", "modelVersion": modelo,
                                        "candidates": [{"finishReason": "STOP",
                                                        "content": {"role": "model", "parts": [{"text": texto}]}}],
                                        "usageMetadata": {"promptTokenCount": 470, "candidatesTokenCount": 30}})
        if proveedor == "ollama":
            return self.responder(200, {"model": modelo, "message": {"role": "assistant", "content": texto},
                                        "done": True, "done_reason": "stop",
                                        "prompt_eval_count": 460, "eval_count": 33})
        return self.responder(404, {"error": "unknown provider"})


def peticiones(proveedor=None, modelo=None):
    with CERROJO:
        lista = list(PETICIONES)
    res = []
    for p, ruta, cab, cuerpo in lista:
        if proveedor and p != proveedor:
            continue
        if modelo:
            m = ruta.split("/models/")[1].split(":")[0] if p == "gemini" else (cuerpo or {}).get("model")
            if m != modelo:
                continue
        res.append((p, ruta, cab, cuerpo))
    return res


# ----------------------------------------------------------------------------------------------------------------
# Cliente JMAP y servidor Stalwart
# ----------------------------------------------------------------------------------------------------------------

class Jmap:
    def __init__(self, base, usuario, clave):
        self.base = base
        self.usuario = usuario
        self.auth = "Basic " + base64.b64encode(f"{usuario}:{clave}".encode()).decode()
        self._sesion = None

    def http(self, metodo, ruta, cuerpo=None, crudo=None, tipo="application/json"):
        datos = crudo if crudo is not None else (None if cuerpo is None else json.dumps(cuerpo).encode())
        req = urllib.request.Request(self.base + ruta, data=datos, method=metodo)
        req.add_header("Authorization", self.auth)
        if datos is not None:
            req.add_header("Content-Type", tipo)
        try:
            with urllib.request.urlopen(req, timeout=120) as r:
                return r.status, r.read()
        except urllib.error.HTTPError as e:
            return e.code, e.read()

    def sesion(self):
        if self._sesion is None:
            estado, cuerpo = self.http("GET", "/jmap/session")
            assert estado == 200, f"sesión de {self.usuario}: {estado} {cuerpo[:300]}"
            self._sesion = json.loads(cuerpo)
        return self._sesion

    def cuenta(self, capacidad="urn:ietf:params:jmap:mail"):
        return self.sesion()["primaryAccounts"][capacidad]

    def una(self, metodo, args, using=USING):
        estado, cuerpo = self.http("POST", "/jmap/", {"using": using, "methodCalls": [[metodo, args, "c0"]]})
        if estado != 200:
            return "error", {"type": f"http{estado}", "description": cuerpo[:300].decode("utf-8", "replace")}
        r = json.loads(cuerpo)["methodResponses"][0]
        return r[0], r[1]

    def crear(self, objeto, datos):
        nombre, r = self.una(f"{objeto}/set", {"create": {"n": datos}})
        if nombre == "error" or "n" not in (r.get("created") or {}):
            raise AssertionError(f"no se pudo crear {objeto}: {json.dumps(r)[:600]}")
        return r["created"]["n"]["id"]

    def set(self, objeto, **args):
        return self.una(f"{objeto}/set", args)


class Servidor:
    def __init__(self, binario):
        self.binario = binario
        self.dir = tempfile.mkdtemp(prefix="bezenti-ia-")
        self.config = os.path.join(self.dir, "config.json")
        with open(self.config, "w") as f:
            json.dump({"@type": "Sqlite", "path": os.path.join(self.dir, "datos")}, f)
        self.p_rec = puerto_libre()
        self.p_http = puerto_libre()
        self.p_smtp = puerto_libre()
        self.log = os.path.join(self.dir, "servidor.log")
        self.proc = None

    def arrancar(self, recuperacion):
        env = dict(os.environ)
        env.update({
            "STALWART_HOSTNAME": "mail.empresa.test",
            "STALWART_RECOVERY_ADMIN": f"admin:{ADMIN_PW}",
            "STALWART_RECOVERY_MODE_PORT": str(self.p_rec),
        })
        if recuperacion:
            env["STALWART_RECOVERY_MODE"] = "1"
        else:
            env.pop("STALWART_RECOVERY_MODE", None)
        self.proc = subprocess.Popen([self.binario, "--config", self.config], env=env,
                                     stdout=open(self.log, "a"), stderr=subprocess.STDOUT, cwd=self.dir)
        esperar_puerto(self.p_rec if recuperacion else self.p_http)

    def parar(self):
        if self.proc and self.proc.poll() is None:
            self.proc.send_signal(signal.SIGTERM)
            try:
                self.proc.wait(30)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        self.proc = None


def configurar(srv):
    print("== configuración inicial (modo recuperación)")
    srv.arrancar(recuperacion=True)
    rec = Jmap(f"http://127.0.0.1:{srv.p_rec}", "admin", ADMIN_PW)
    for nombre, proto, puerto in (("http", "http", srv.p_http), ("smtp", "smtp", srv.p_smtp)):
        rec.crear("x:NetworkListener", {"name": nombre, "protocol": proto, "bind": {f"127.0.0.1:{puerto}": True},
                                        "useTls": False, "implicitTls": False})
    rec.crear("x:Tracer", {"@type": "Stdout", "level": "debug", "enable": True})
    rec.crear("x:Asn", {"@type": "Disabled"})
    n, r = rec.set("x:SpamSettings", update={"singleton": {"enable": True, "spamFilterRulesUrl": None}})
    assert "singleton" in (r.get("updated") or {}), f"x:SpamSettings: {r}"
    rec.set("x:SpamPyzor", update={"singleton": {"enable": False}})
    n, r = rec.set("x:MtaStageAuth", update={"singleton": {"require": {"else": "false"}}})
    assert "singleton" in (r.get("updated") or {}), f"x:MtaStageAuth: {r}"
    _, apps = rec.una("x:Application/query", {})
    if apps.get("ids"):
        rec.set("x:Application", update={i: {"enable": False} for i in apps["ids"]})
    srv.parar()
    srv.arrancar(recuperacion=False)
    esperar_puerto(srv.p_smtp)


# ----------------------------------------------------------------------------------------------------------------
# Pruebas
# ----------------------------------------------------------------------------------------------------------------

PHISHING = (
    "From: \"Banco Nacional\" <seguridad@banco-falso.test>\r\n"
    "To: ana@empresa.test\r\n"
    "Subject: {asunto}\r\n"
    "Message-ID: <{mid}@banco-falso.test>\r\n"
    "Date: Fri, 13 Nov 2026 10:00:00 +0000\r\n"
    "MIME-Version: 1.0\r\n"
    "Content-Type: text/html; charset=utf-8\r\n"
    "\r\n"
    "<html><body><p>Estimado cliente, su cuenta ser&aacute; bloqueada hoy.</p>"
    "<p><a href=\"http://login.banco-falso.test/verificar\">Banco Nacional - Iniciar sesi&oacute;n</a></p>"
    "<p>{relleno}</p></body></html>\r\n"
)


def mensaje(asunto="Verifique su cuenta", relleno="Gracias.", mid=None):
    return PHISHING.format(asunto=asunto, relleno=relleno, mid=mid or str(time.time_ns()))


def accion(adm, datos):
    nombre, r = adm.set("x:Action", create={"a": datos})
    if nombre == "error" or "a" not in (r.get("created") or {}):
        return None, r
    return r["created"]["a"], r


def recargar(adm):
    res, r = accion(adm, {"@type": "ReloadSettings"})
    return res is not None, r


def clasificar(adm, raw, **extra):
    datos = {"@type": "ClassifySpam", "message": raw, "remoteIp": "192.0.2.25",
             "ehloDomain": "mail.banco-falso.test", "mailFrom": "seguridad@banco-falso.test",
             "rcptTo": {"ana@empresa.test": True}}
    datos.update(extra)
    inicio = time.time()
    res, r = accion(adm, datos)
    if res is None:
        raise AssertionError(f"ClassifySpam falló: {json.dumps(r)[:500]}")
    res["_segundos"] = time.time() - inicio
    return res


def spam_ai(adm, **props):
    datos = {"@type": "Enable", "mode": "Automatic", "condition": {"else": "true"}, "deadline": 8000,
             "learn": {"@type": "Disable"}, "webSearch": {"@type": "Disable"}}
    datos.update(props)
    n, r = adm.set("x:SpamAi", update={"singleton": datos})
    assert "singleton" in (r.get("updated") or {}), f"x:SpamAi: {json.dumps(r)[:600]}"
    hecho, r = recargar(adm)
    assert hecho, f"ReloadSettings: {json.dumps(r)[:600]}"


def spam_ai_off(adm):
    adm.set("x:SpamAi", update={"singleton": {"@type": "Disable"}})
    recargar(adm)


def modelo(adm, tipo, nombre, modelo_id, ruta, **extra):
    datos = {"@type": tipo, "name": nombre, "model": modelo_id, "url": ruta,
             "apiKey": {"@type": "Value", "secret": CLAVE}, "timeout": 20000}
    datos.update(extra)
    return adm.crear("x:AiModel", datos)


def etiquetas(res):
    return res.get("tags") or {}


def pruebas(srv, falso):
    base = f"http://127.0.0.1:{srv.p_http}"
    adm = Jmap(base, "admin", ADMIN_PW)
    adm.crear("x:Domain", {"name": "empresa.test", "dkimManagement": {"@type": "Manual"}})
    dom = adm.una("x:Domain/query", {})[1]["ids"][0]
    adm.crear("x:Account", {"@type": "User", "name": "ana", "domainId": dom,
                            "credentials": {"0": {"@type": "Password", "secret": PW}}})

    print("== línea base: sin IA")
    base_res = clasificar(adm, mensaje())
    base_score = base_res.get("score", 0)
    ok(base_res.get("aiGate") is None and not any(t.startswith("AI_") for t in etiquetas(base_res)),
       "sin x:SpamAi no hay compuerta ni etiquetas AI_", json.dumps(base_res)[:300])

    print("== un proveedor por vez como único nivel")
    casos = [
        ("OpenAI", "OpenAi", "openai", "phish-oa", f"{falso}/openai/v1", {"profile": "OpenAi"}),
        ("Anthropic", "Anthropic", "anthropic", "phish-claude", f"{falso}/anthropic", {}),
        ("Gemini", "Gemini", "gemini", "phish-gemini", f"{falso}/gemini/v1beta", {}),
        ("Ollama", "Ollama", "ollama", "phish-ollama", f"{falso}/ollama", {"keepAlive": 300000}),
        ("TypeSafe", "TypeSafe", "typesafe", "jev-latest", f"{falso}/typesafe", {}),
    ]
    for etiqueta, tipo, prov, mid, url, extra in casos:
        mid_id = modelo(adm, tipo, f"m-{prov}", mid, url, **extra)
        spam_ai(adm, tiers={"0": {"modelId": mid_id}})
        antes = len(peticiones(prov))
        res = clasificar(adm, mensaje())
        tags = etiquetas(res)
        reqs = peticiones(prov)[antes:]
        ok(res.get("aiGate") == "run", f"{etiqueta}: compuerta «run»", str(res.get("aiGate")))
        ok(len(reqs) == 1, f"{etiqueta}: una sola petición al proveedor", str(len(reqs)))
        if not reqs:
            continue
        _, ruta, cab, cuerpo = reqs[0]
        verdict = res.get("aiVerdict") or ""
        if prov == "typesafe":
            ok(verdict.startswith("phishing; p=0.93; confidence=0.90; model=jev-1.14; tier=1"),
               f"{etiqueta}: veredicto con la probabilidad del proveedor", verdict)
            ok(cab.get("authorization") == f"Bearer {CLAVE}" and ruta == "/typesafe/v1/systemone",
               f"{etiqueta}: Bearer y /v1/systemone")
            ok(cuerpo.get("questions", {}).get("category", {}).get("type") == "choice"
               and "phishing" in cuerpo["questions"]["category"]["criteria"]
               and cuerpo["questions"].get("signal_impersonation", {}).get("type") == "noul"
               and cuerpo.get("state") == json.loads(res.get("aiFacts") or "null"),
               f"{etiqueta}: pregunta choice de categorías, noul por señal y los hechos como state")
        else:
            ok(verdict.startswith("phishing; p=0.90; confidence=0.80; model=")
               and "; tier=1" in verdict and 'reason="Fake bank login page"' not in verdict,
               f"{etiqueta}: veredicto phishing (confianza verbal alta)", verdict)
            if prov == "openai":
                ok(cab.get("authorization") == f"Bearer {CLAVE}" and ruta == "/openai/v1/chat/completions"
                   and cuerpo["response_format"]["type"] == "json_schema"
                   and cuerpo["response_format"]["json_schema"]["strict"] is True
                   and "max_completion_tokens" in cuerpo
                   and cuerpo["messages"][0]["role"] == "system" and cuerpo["messages"][1]["content"] == res.get("aiFacts"),
                   f"{etiqueta}: Bearer, /chat/completions, json_schema estricto y hechos = aiFacts")
            elif prov == "anthropic":
                ok(cab.get("x-api-key") == CLAVE and cab.get("anthropic-version") == "2023-06-01"
                   and ruta == "/anthropic/v1/messages"
                   and cuerpo["output_config"]["format"]["type"] == "json_schema"
                   and "phishing" in cuerpo["system"] and cuerpo["messages"][0]["content"] == res.get("aiFacts")
                   and "temperature" not in cuerpo,
                   f"{etiqueta}: x-api-key, anthropic-version, output_config.format y system prompt")
            elif prov == "gemini":
                gc = cuerpo["generationConfig"]
                ok(cab.get("x-goog-api-key") == CLAVE and ruta == "/gemini/v1beta/models/phish-gemini:generateContent"
                   and gc["responseMimeType"] == "application/json" and "responseJsonSchema" in gc
                   and "phishing" in cuerpo["systemInstruction"]["parts"][0]["text"]
                   and cuerpo["contents"][0]["parts"][0]["text"] == res.get("aiFacts"),
                   f"{etiqueta}: x-goog-api-key, generateContent y responseJsonSchema")
            elif prov == "ollama":
                ok(cab.get("authorization") == f"Bearer {CLAVE}" and ruta == "/ollama/api/chat"
                   and cuerpo["stream"] is False and cuerpo["format"]["type"] == "object"
                   and cuerpo["keep_alive"] == "300s" and "num_predict" in cuerpo["options"],
                   f"{etiqueta}: /api/chat sin streaming, format con esquema y keep_alive")
        ok("AI_PHISHING_HIGH" in tags and tags["AI_PHISHING_HIGH"].get("score") == 7.0
           and "AI_IMPERSONATION" in tags,
           f"{etiqueta}: etiquetas AI_PHISHING_HIGH (7.0) y AI_IMPERSONATION", json.dumps(tags)[:300])
        ok(res.get("score", 0) >= base_score + 10, f"{etiqueta}: la puntuación sube con el veredicto",
           f"{base_score} → {res.get('score')}")
        adm.set("x:SpamAi", update={"singleton": {"@type": "Disable"}})
        adm.set("x:AiModel", destroy=[mid_id])
        recargar(adm)

    print("== hechos enviados al modelo (aiFacts)")
    oa = modelo(adm, "OpenAi", "oa", "phish-oa", f"{falso}/openai/v1", profile="OpenAi")
    spam_ai(adm, tiers={"0": {"modelId": oa}}, maxInputChars=200)
    res = clasificar(adm, mensaje(relleno="x" * 3000))
    hechos = json.loads(res.get("aiFacts") or "{}").get("email", {})
    ok(hechos.get("subject") == "Verifique su cuenta" and hechos.get("from", {}).get("domain") == "banco-falso.test"
       and hechos.get("bodyTruncated") is True and len(hechos.get("body", "")) <= 200
       and any(l.get("host") == "login.banco-falso.test" and "Banco Nacional" in l.get("text", "")
               for l in hechos.get("links", []))
       and "authentication" in hechos and "recipients" in hechos,
       "hechos: asunto, remitente, enlaces (texto y destino), autenticación y cuerpo truncado a maxInputChars",
       json.dumps(hechos)[:500])

    print("== explicación en la cabecera")
    spam_ai(adm, tiers={"0": {"modelId": oa}}, explanation=True)
    res = clasificar(adm, mensaje())
    ok('reason="Fake bank login page"' in (res.get("aiVerdict") or ""), "explanation → reason=\"…\" en el veredicto",
       res.get("aiVerdict"))
    ok("explanation" in peticiones("openai")[-1][3]["response_format"]["json_schema"]["schema"]["required"],
       "explanation forma parte del esquema pedido")

    print("== logprobs (OpenAI)")
    lp = modelo(adm, "OpenAi", "lp", "logprobs-oa", f"{falso}/openai/v1", profile="OpenAi", logprobs=True)
    spam_ai(adm, tiers={"0": {"modelId": lp}})
    res = clasificar(adm, mensaje())
    req = peticiones("openai", "logprobs-oa")[-1][3]
    ok(req.get("logprobs") is True and req.get("top_logprobs") == 5, "se piden logprobs y top_logprobs")
    ok((res.get("aiVerdict") or "").startswith("phishing; p=0.95; confidence=0.94"),
       "la probabilidad y la confianza salen de los logprobs", res.get("aiVerdict"))

    print("== escalado a un segundo nivel")
    dudoso = modelo(adm, "Ollama", "dudoso", "unsure-ollama", f"{falso}/ollama")
    spam_ai(adm, tiers={"0": {"modelId": dudoso, "escalateBelow": 0.3}, "1": {"modelId": oa}})
    res = clasificar(adm, mensaje())
    v = res.get("aiVerdict") or ""
    ok(v.startswith("phishing;") and "tier=2" in v and "tier1=newsletter/0.55" in v,
       "veredicto poco concluyente → segundo nivel; la cabecera recuerda el primero", v)
    spam_ai(adm, tiers={"0": {"modelId": oa, "escalateOn": {"phishing": True}}, "1": {"modelId": dudoso}})
    res = clasificar(adm, mensaje())
    v = res.get("aiVerdict") or ""
    ok("tier=2" in v and "tier1=phishing" in v and "AI_UNCERTAIN" in etiquetas(res),
       "escalateOn: la categoría escala; un veredicto de baja confianza da AI_UNCERTAIN", v)

    print("== modo Shadow")
    spam_ai(adm, tiers={"0": {"modelId": oa}}, mode="Shadow")
    res = clasificar(adm, mensaje())
    ok("; shadow" in (res.get("aiVerdict") or "") and not any(t.startswith("AI_") for t in etiquetas(res))
       and abs(res.get("score", 0) - base_score) < 0.01,
       "Shadow: veredicto registrado, sin etiquetas ni cambio de puntuación", json.dumps(res)[:300])

    print("== compuerta")
    spam_ai(adm, tiers={"0": {"modelId": oa}}, condition={"else": "false"})
    antes = len(peticiones())
    res = clasificar(adm, mensaje())
    ok(res.get("aiGate") == "condition" and len(peticiones()) == antes and res.get("aiFacts") is None,
       "condition falsa → «condition», sin llamada al modelo")
    spam_ai(adm, tiers={"0": {"modelId": oa}}, condition={"else": "spam_score > -2 && spam_score < spam_threshold + 6 && spam_probability < 0"})
    res = clasificar(adm, mensaje())
    ok(res.get("aiGate") == "run", "condición con spam_score, spam_threshold y spam_probability", str(res.get("aiGate")))
    res = clasificar(adm, mensaje(), authenticatedAs="ana@empresa.test")
    ok(res.get("aiGate") == "authenticated", "remitente autenticado → «authenticated»", str(res.get("aiGate")))

    print("== fallos del proveedor: el correo sigue el camino clásico")
    e500 = modelo(adm, "OpenAi", "e500", "e500-oa", f"{falso}/openai/v1", profile="OpenAi")
    spam_ai(adm, tiers={"0": {"modelId": e500}})
    antes = len(peticiones("openai", "e500-oa"))
    res = clasificar(adm, mensaje())
    ok((res.get("aiVerdict") or "").startswith('error; tier=1; error="HTTP 500') and "AI_ERROR" in etiquetas(res)
       and str(res.get("result")).lower() in ("ham", "spam") and len(peticiones("openai", "e500-oa")) - antes == 2,
       "HTTP 500 → un reintento, AI_ERROR y clasificación normal",
       f'{res.get("result")} {len(peticiones("openai", "e500-oa")) - antes} {res.get("aiVerdict")}')

    lento = modelo(adm, "Anthropic", "lento", "slow-claude", f"{falso}/anthropic")
    spam_ai(adm, tiers={"0": {"modelId": lento}}, deadline=2000)
    res = clasificar(adm, mensaje())
    ok("AI_ERROR" in etiquetas(res) and "timed out" in (res.get("aiVerdict") or "") and res["_segundos"] < 5.5,
       "modelo lento → se corta en el plazo (deadline) y AI_ERROR", f"{res['_segundos']:.1f}s {res.get('aiVerdict')}")

    roto = modelo(adm, "Gemini", "roto", "badjson-gemini", f"{falso}/gemini/v1beta")
    basura = modelo(adm, "Ollama", "basura", "garbage-ollama", f"{falso}/ollama")
    spam_ai(adm, tiers={"0": {"modelId": roto}, "1": {"modelId": basura}})
    res = clasificar(adm, mensaje())
    ok("AI_ERROR" in etiquetas(res) and "tier=2" in (res.get("aiVerdict") or "")
       and "Invalid JSON" in (res.get("aiVerdict") or ""),
       "JSON inválido y respuesta sin veredicto → se prueba el siguiente nivel y luego AI_ERROR", res.get("aiVerdict"))
    spam_ai(adm, tiers={"0": {"modelId": e500}, "1": {"modelId": oa}})
    res = clasificar(adm, mensaje())
    ok((res.get("aiVerdict") or "").startswith("phishing;") and "tier=2" in res.get("aiVerdict", ""),
       "si un nivel falla se usa el siguiente", res.get("aiVerdict"))

    print("== cortacircuitos")
    e401 = modelo(adm, "OpenAi", "e401", "e401-oa", f"{falso}/openai/v1", profile="OpenAi")
    spam_ai(adm, tiers={"0": {"modelId": e401}}, breakerThreshold=5, breakerCooldown=60000)
    res1 = clasificar(adm, mensaje())
    antes = len(peticiones("openai", "e401-oa"))
    res2 = clasificar(adm, mensaje())
    ok("AI_ERROR" in etiquetas(res1) and res2.get("aiGate") == "unavailable"
       and len(peticiones("openai", "e401-oa")) == antes and not any(t.startswith("AI_") for t in etiquetas(res2)),
       "401 abre el cortacircuitos al instante → «unavailable» sin llamar ni etiquetar")
    spam_ai(adm, tiers={"0": {"modelId": e500}}, breakerThreshold=2, breakerCooldown=60000)
    clasificar(adm, mensaje())
    clasificar(adm, mensaje())
    res3 = clasificar(adm, mensaje())
    ok(res3.get("aiGate") == "unavailable", "breakerThreshold fallos seguidos → «unavailable»", str(res3.get("aiGate")))

    print("== SMTP de extremo a extremo")
    spam_ai(adm, tiers={"0": {"modelId": oa}})
    ana = Jmap(base, "ana@empresa.test", PW)

    def entregar(asunto):
        with smtplib.SMTP("127.0.0.1", srv.p_smtp, timeout=60) as s:
            s.sendmail("seguridad@banco-falso.test", ["ana@empresa.test"], mensaje(asunto=asunto))
        for _ in range(60):
            n, r = ana.una("Email/query", {"accountId": ana.cuenta(), "filter": {"subject": asunto}}, USING_MAIL)
            if n != "error" and r.get("ids"):
                n, r = ana.una("Email/get", {"accountId": ana.cuenta(), "ids": r["ids"],
                                             "properties": ["header:X-Spam-AI:asText", "header:X-Spam-Result:asText",
                                                            "header:X-Spam-Score:asText"]}, USING_MAIL)
                return r["list"][0]
            time.sleep(1)
        return None

    correo = entregar("Dudoso con IA")
    ok(correo is not None and (correo.get("header:X-Spam-AI:asText") or "").startswith("phishing; p=0.90")
       and "AI_PHISHING_HIGH" in (correo.get("header:X-Spam-Result:asText") or ""),
       "correo dudoso por SMTP → llamada al modelo, X-Spam-AI y AI_PHISHING_HIGH en el buzón", json.dumps(correo)[:400])

    caido = modelo(adm, "Ollama", "caido", "phish-caido", f"http://127.0.0.1:{puerto_libre()}")
    spam_ai(adm, tiers={"0": {"modelId": caido}})
    correo = entregar("Modelo caido")
    ok(correo is not None and (correo.get("header:X-Spam-AI:asText") or "").startswith("error; tier=1"),
       "modelo caído → el correo se entrega igual, con X-Spam-AI: error", json.dumps(correo)[:400])
    spam_ai_off(adm)

    print("== Sieve: llm_prompt en un script de usuario")
    modelo(adm, "OpenAi", "clasificador", "sieve-oa", f"{falso}/openai/v1", profile="Generic")
    recargar(adm)
    script = (
        'require ["fileinto", "mailbox", "vnd.stalwart.expressions"];\r\n'
        'let "reply" "llm_prompt(\'clasificador\', \'Classify: \' + header.subject, 0.2)";\r\n'
        # contains_ignore_case compara como texto: con contains, el `false` de un fallo (0) es «igual» a
        # cualquier texto no numérico (comparación numérica de Sieve) y el correo iría a la carpeta «0».
        'if eval "contains_ignore_case([\'Work\', \'Family\'], reply)" {\r\n'
        '    fileinto :create "${reply}";\r\n'
        '}\r\n'
    )
    estado, cuerpo = ana.http("POST", f"/jmap/upload/{ana.cuenta()}/", crudo=script.encode(), tipo="application/sieve")
    blob = json.loads(cuerpo).get("blobId") if estado in (200, 201) else None
    n, r = ana.una("SieveScript/set", {"accountId": ana.cuenta("urn:ietf:params:jmap:sieve"),
                                       "create": {"s": {"name": "ia", "blobId": blob}},
                                       "onSuccessActivateScript": "#s"}, USING_SIEVE)
    ok(n != "error" and "s" in (r.get("created") or {}), "se instala el script con llm_prompt", json.dumps(r)[:400])

    def buzon_de(asunto):
        with smtplib.SMTP("127.0.0.1", srv.p_smtp, timeout=60) as s:
            s.sendmail("pedro@socio.test", ["ana@empresa.test"],
                       f"From: pedro@socio.test\r\nTo: ana@empresa.test\r\nSubject: {asunto}\r\n\r\nHola Ana.\r\n")
        for _ in range(60):
            n, r = ana.una("Email/query", {"accountId": ana.cuenta(), "filter": {"subject": asunto}}, USING_MAIL)
            if n != "error" and r.get("ids"):
                n, r = ana.una("Email/get", {"accountId": ana.cuenta(), "ids": r["ids"], "properties": ["mailboxIds"]},
                               USING_MAIL)
                ids = list(r["list"][0]["mailboxIds"])
                n, r = ana.una("Mailbox/get", {"accountId": ana.cuenta(), "ids": ids, "properties": ["name"]},
                               USING_MAIL)
                return [m["name"] for m in r["list"]]
            time.sleep(1)
        return None

    antes = len(peticiones("openai", "sieve-oa"))
    ok(buzon_de("Reunion de presupuesto") == ["Work"], "llm_prompt responde «Work» → fileinto Work")
    reqs = peticiones("openai", "sieve-oa")[antes:]
    ok(len(reqs) == 1 and reqs[0][3]["messages"] == [{"role": "user", "content": "Classify: Reunion de presupuesto"}]
       and reqs[0][3].get("temperature") == 0.2 and "response_format" not in reqs[0][3],
       "el prompt va sin system prompt, con la temperatura del script y sin salida estructurada",
       json.dumps(reqs[0][3] if reqs else None)[:300])
    adm.set("x:AiModel", destroy=[i for i in adm.una("x:AiModel/query", {})[1].get("ids", [])])
    recargar(adm)
    buzones = buzon_de("Sin modelo")
    ok(buzones == ["Inbox"], "sin el modelo, llm_prompt devuelve false y el correo queda en Inbox", str(buzones))

    print("== errores de configuración")
    mal = modelo(adm, "OpenAi", "mal", "phish", f"{falso}/openai/v1", extraParameters="[1, 2]")
    hecho, r = recargar(adm)
    ok(not hecho and "Extra parameters must be a JSON object" in json.dumps(r),
       "extraParameters que no es un objeto → la recarga se rechaza", json.dumps(r)[:300])
    adm.set("x:AiModel", destroy=[mal])
    hecho, r = recargar(adm)
    ok(hecho, "al quitarlo la recarga vuelve a funcionar", json.dumps(r)[:300])

    print("== telemetría y secretos")
    with open(srv.log, errors="replace") as f:
        registro = f.read()
    for evento in ("ai.request", "ai.api-error", "ai.spam-verdict", "ai.spam-skipped", "ai.breaker-open"):
        ok(evento in registro, f"el registro contiene {evento}")
    ok("inputTokens" in registro and "model" in registro, "ai.request lleva modelo y tokens")
    ok(CLAVE not in registro, "la clave API nunca aparece en el registro")
    estado, cuerpo = adm.http("GET", "/metrics/prometheus")
    _ = estado, cuerpo  # Prometheus puede no estar activo; la métrica se prueba en las pruebas unitarias de trc


def main():
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    falso_srv = ThreadingHTTPServer(("127.0.0.1", 0), Falso)
    threading.Thread(target=falso_srv.serve_forever, daemon=True).start()
    falso = f"http://127.0.0.1:{falso_srv.server_address[1]}"
    srv = Servidor(os.path.abspath(sys.argv[1]))
    error = None
    try:
        configurar(srv)
        pruebas(srv, falso)
    except Exception as e:  # noqa: BLE001
        error = e
        FALLOS.append(f"excepción: {e!r}")
    finally:
        srv.parar()
        falso_srv.shutdown()
        if FALLOS:
            destino = os.path.join(tempfile.gettempdir(), "bezenti-ia.log")
            shutil.copy(srv.log, destino)
            print(f"\nregistro del servidor en {destino}")
        shutil.rmtree(srv.dir, ignore_errors=True)
    if error is not None:
        import traceback
        traceback.print_exception(error)
    print(f"\n{len(PROBADOS) - len(FALLOS)}/{len(PROBADOS)} comprobaciones correctas")
    if FALLOS:
        print("FALLOS:\n  " + "\n  ".join(FALLOS))
        sys.exit(1)


if __name__ == "__main__":
    main()
