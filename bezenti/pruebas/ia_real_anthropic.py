#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 bezenti
#
# SPDX-License-Identifier: AGPL-3.0-only
"""Prueba MANUAL (no se ejecuta en la CI) del clasificador con IA y de llm_prompt contra Anthropic de verdad.

Usa el proxy/la cuenta de Anthropic de la máquina a través de variables de entorno, sin imprimirlas nunca:
- ANTHROPIC_BASE_URL (opcional; por defecto https://api.anthropic.com)
- ANTHROPIC_API_KEY  → el servidor la lee él mismo (apiKey = variable de entorno), o
- ANTHROPIC_AUTH_TOKEN → se envía como `Authorization: Bearer` (httpHeaders) para proxies que lo piden.
Modelo: ANTHROPIC_MODEL o `claude-haiku-5-5`.

Uso: ia_real_anthropic.py RUTA_AL_BINARIO_STALWART
"""

import importlib.util
import json
import os
import shutil
import sys
import tempfile

sys.dont_write_bytecode = True
aqui = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("ia", os.path.join(aqui, "ia.py"))
ia = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ia)


def main():
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    if not (os.environ.get("ANTHROPIC_API_KEY") or os.environ.get("ANTHROPIC_AUTH_TOKEN")):
        raise SystemExit("faltan ANTHROPIC_API_KEY o ANTHROPIC_AUTH_TOKEN en el entorno")
    srv = ia.Servidor(os.path.abspath(sys.argv[1]))
    try:
        ia.configurar(srv)
        adm = ia.Jmap(f"http://127.0.0.1:{srv.p_http}", "admin", ia.ADMIN_PW)
        adm.crear("x:Domain", {"name": "empresa.test", "dkimManagement": {"@type": "Manual"}})
        datos = {"@type": "Anthropic", "name": "claude", "model": os.environ.get("ANTHROPIC_MODEL", "claude-haiku-5-5"),
                 "url": os.environ.get("ANTHROPIC_BASE_URL") or None, "maxOutputTokens": 1024, "timeout": 60000}
        if os.environ.get("ANTHROPIC_API_KEY"):
            datos["apiKey"] = {"@type": "EnvironmentVariable", "variableName": "ANTHROPIC_API_KEY"}
        else:
            datos["apiKey"] = {"@type": "None"}
            datos["httpHeaders"] = {"Authorization": "Bearer " + os.environ["ANTHROPIC_AUTH_TOKEN"]}
        mid = adm.crear("x:AiModel", datos)
        ia.spam_ai(adm, tiers={"0": {"modelId": mid}}, explanation=True, deadline=30000)

        print("== phishing real contra Anthropic")
        res = ia.clasificar(adm, ia.mensaje())
        print("  aiGate:", res.get("aiGate"))
        print("  aiVerdict:", res.get("aiVerdict"))
        print("  etiquetas AI_:", sorted(t for t in (res.get("tags") or {}) if t.startswith("AI_")))
        ia.ok(res.get("aiGate") == "run" and (res.get("aiVerdict") or "").split(";")[0] in ("phishing", "malware"),
              "Anthropic clasifica el phishing como phishing")

        print("== correo legítimo")
        legit = ("From: Pedro <pedro@socio.test>\r\nTo: ana@empresa.test\r\nSubject: Acta de la reunion\r\n"
                 "Message-ID: <acta@socio.test>\r\nIn-Reply-To: <previo@empresa.test>\r\n\r\n"
                 "Hola Ana, te adjunto el acta de ayer. Revisa el punto 3 cuando puedas. Saludos, Pedro\r\n")
        res = ia.clasificar(adm, legit, mailFrom="pedro@socio.test")
        print("  aiVerdict:", res.get("aiVerdict"))
        ia.ok((res.get("aiVerdict") or "").split(";")[0] == "legitimate", "Anthropic ve el correo legítimo")
    finally:
        srv.parar()
        with open(srv.log, errors="replace") as f:
            registro = f.read()
        for secreto in (os.environ.get("ANTHROPIC_API_KEY"), os.environ.get("ANTHROPIC_AUTH_TOKEN")):
            if secreto:
                ia.ok(secreto not in registro, "la credencial no aparece en el registro")
        if ia.FALLOS:
            destino = os.path.join(tempfile.gettempdir(), "bezenti-ia-real.log")
            shutil.copy(srv.log, destino)
            print(f"registro en {destino}")
        shutil.rmtree(srv.dir, ignore_errors=True)
    print(f"\n{len(ia.PROBADOS) - len(ia.FALLOS)}/{len(ia.PROBADOS)} comprobaciones correctas")
    sys.exit(1 if ia.FALLOS else 0)


if __name__ == "__main__":
    main()
