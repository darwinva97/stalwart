#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 bezenti
#
# SPDX-License-Identifier: AGPL-3.0-only
"""Pruebas de integración de la fase 6: almacén de blobs repartido y réplicas de lectura (edición bezenti).

Levanta el servidor real y lo prueba por SMTP, IMAP y JMAP, solo con la biblioteca estándar de Python.
Especificación: `bezenti/especificaciones/replicas-y-reparto.md`.

1. Almacén de blobs repartido (`x:BlobStore` = `Sharded`, tres `FileSystem`) sobre SQLite: se reciben 30
   correos por SMTP, se leen íntegros por IMAP, los blobs quedan repartidos entre los tres directorios (cada
   uno con una parte razonable) y, tras reiniciar, se siguen leyendo.
2. Réplicas de lectura (solo con `--pg DIR`, un primario y una réplica física levantados con
   `postgres_replica.sh`): el almacén de datos es PostgreSQL con `readReplicas`; el correo recibido se lee al
   momento por IMAP y JMAP, la réplica atiende consultas (sus estadísticas suben), y si la réplica cae el
   servidor sigue recibiendo y sirviendo correo con el primario; cuando vuelve, se usa otra vez.

Uso: replicas_reparto.py RUTA_AL_BINARIO_STALWART [--pg DIR]
"""

import imaplib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import multi_inquilino as mi  # noqa: E402
from multi_inquilino import ADMIN_PW, PW, USING_MAIL, Jmap, Servidor, ok  # noqa: E402
from archivo_enmascaradas import enviar, esperar  # noqa: E402


def configurar(srv, extra=()):
    """Modo recuperación: listeners, registro, sin antispam ni descargas; `extra` = más singletons."""
    srv.arrancar(recuperacion=True)
    rec = Jmap(f"http://127.0.0.1:{srv.p_rec}", "admin", ADMIN_PW)
    for nombre, proto, puerto in (("http", "http", srv.p_http), ("smtp", "smtp", srv.p_smtp),
                                  ("imap", "imap", srv.p_imap)):
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
        *extra,
    ):
        _, r = rec.set(objeto, update={"singleton": cambios})
        assert "singleton" in (r.get("updated") or {}), f"{objeto}: {json.dumps(r)[:600]}"
    _, apps = rec.query("x:Application")
    if apps.get("ids"):
        rec.set("x:Application", update={i: {"enable": False} for i in apps["ids"]})
    srv.parar()
    arrancar(srv)


def arrancar(srv):
    srv.arrancar(recuperacion=False)
    mi.esperar_puerto(srv.p_smtp)
    mi.esperar_puerto(srv.p_imap)


def crear_usuario(srv):
    rec = Jmap(f"http://127.0.0.1:{srv.p_http}", "admin", ADMIN_PW)
    # Sin límite de conexiones SMTP por IP (se crean al primer arranque normal): la prueba manda muchos correos
    # seguidos desde 127.0.0.1.
    _, lim = rec.query("x:MtaInboundThrottle")
    if lim.get("ids"):
        rec.set("x:MtaInboundThrottle", update={i: {"enable": False} for i in lim["ids"]})
        rec.set("x:Action", create={"r": {"@type": "ReloadSettings"}})
    dom = rec.crear("x:Domain", {"name": "reparto.test", "dkimManagement": {"@type": "Manual"}})
    rec.crear("x:Account", {"@type": "User", "name": "ana", "domainId": dom,
                            "credentials": {"0": {"@type": "Password", "secret": PW}}})
    return Jmap(f"http://127.0.0.1:{srv.p_http}", "ana@reparto.test", PW)


def leer_imap(srv, asuntos):
    """Lee por IMAP todos los correos de INBOX; devuelve {asunto: cuerpo} de los pedidos."""
    imap = imaplib.IMAP4("127.0.0.1", srv.p_imap)
    imap.login("ana@reparto.test", PW)
    imap.select("INBOX")
    vistos = {}
    typ, data = imap.search(None, "ALL")
    for num in (data[0].split() if typ == "OK" and data and data[0] else []):
        typ, partes = imap.fetch(num, "(RFC822)")
        if typ != "OK":
            continue
        crudo = next((p[1] for p in partes if isinstance(p, tuple)), b"").decode("utf-8", "replace")
        cabeceras, _, cuerpo = crudo.partition("\r\n\r\n")
        asunto = next((linea[9:] for linea in cabeceras.split("\r\n") if linea.startswith("Subject: ")), "")
        if asunto in asuntos:
            vistos[asunto] = cuerpo.strip()
    imap.logout()
    return vistos


def contar_archivos(d):
    return sum(len(f) for _, _, f in os.walk(d))


# ------------------------------------------------------------------------------------------------------------
def prueba_reparto(binario):
    print("== 1. almacén de blobs repartido entre tres directorios (datos en SQLite)")
    srv = Servidor(binario)
    try:
        dirs = [os.path.join(srv.dir, f"blobs-{i}") for i in range(3)]
        configurar(srv, extra=(("x:BlobStore", {
            "@type": "Sharded",
            "stores": {str(i): {"@type": "FileSystem", "path": d, "depth": 1} for i, d in enumerate(dirs)},
        }),))
        usr = crear_usuario(srv)
        antes = [contar_archivos(d) for d in dirs]
        asuntos = {}
        for i in range(30):
            asunto = f"Repartido {i}"
            cuerpo = f"Cuerpo del correo {i}: " + ("x" * (100 + 37 * i))
            asuntos[asunto] = cuerpo
            ok(enviar(srv, "ana@reparto.test", asunto, cuerpo) is None, f"SMTP acepta el correo {i}")
        _, r = usr.una("Email/query", {"accountId": usr.cuenta()}, USING_MAIL)
        ok(esperar(lambda: len(usr.una("Email/query", {"accountId": usr.cuenta(), "filter": {"subject": "Repartido"}},
                                       USING_MAIL)[1].get("ids") or []) >= 30),
           "los 30 correos llegan (JMAP)")
        vistos = leer_imap(srv, asuntos)
        ok(len(vistos) == 30 and all(vistos[a] == c for a, c in asuntos.items()),
           "IMAP los lee íntegros (cada blob sale de su fragmento)", f"{len(vistos)} leídos")
        despues = [contar_archivos(d) for d in dirs]
        nuevos = [b - a for a, b in zip(antes, despues)]
        total = sum(nuevos)
        print(f"     blobs nuevos por directorio: {nuevos}")
        ok(total >= 30, "hay al menos un blob por correo", str(nuevos))
        ok(all(n >= total * 0.15 for n in nuevos), "reparto razonable entre los tres directorios", str(nuevos))
        # Un blob concreto está en un solo directorio.
        nombres = [set(f for _, _, fs in os.walk(d) for f in fs) for d in dirs]
        ok(not (nombres[0] & nombres[1]) and not (nombres[0] & nombres[2]) and not (nombres[1] & nombres[2]),
           "ningún blob está repetido en dos fragmentos")
        # JMAP: descargar el mensaje entero de uno de ellos.
        _, r = usr.una("Email/query", {"accountId": usr.cuenta(), "filter": {"subject": "Repartido 7"}}, USING_MAIL)
        eid = (r.get("ids") or [None])[0]
        _, r = usr.una("Email/get", {"accountId": usr.cuenta(), "ids": [eid], "properties": ["blobId"]}, USING_MAIL)
        blob = ((r.get("list") or [{}])[0]).get("blobId")
        estado, cuerpo = usr.http("GET", f"/jmap/download/{usr.cuenta()}/{blob}/m.eml?accept=message/rfc822")
        ok(estado == 200 and asuntos["Repartido 7"].encode() in cuerpo, "JMAP descarga el mensaje del fragmento")

        print("== 1b. reinicio: los blobs se siguen encontrando")
        srv.parar()
        arrancar(srv)
        vistos = leer_imap(srv, asuntos)
        ok(len(vistos) == 30 and all(vistos[a] == c for a, c in asuntos.items()), "tras reiniciar, IMAP lee los 30")
    finally:
        srv.parar()
        guardar_registro(srv, "reparto")


# ------------------------------------------------------------------------------------------------------------
class Pg:
    def __init__(self, dir_pg):
        self.dir = dir_pg
        self.env = {}
        with open(os.path.join(dir_pg, "entorno.sh")) as f:
            for linea in f:
                k, _, v = linea.strip().removeprefix("export ").partition("=")
                self.env[k] = v
        self.p1 = int(self.env["BEZENTI_PG_PUERTO_PRIMARIO"])
        self.p2 = int(self.env["BEZENTI_PG_PUERTO_REPLICA"])
        self.bin = os.path.dirname(self.env["BEZENTI_PG_CTL"])

    def psql(self, puerto, sql, base="postgres"):
        r = subprocess.run([os.path.join(self.bin, "psql"), "-h", "127.0.0.1", "-p", str(puerto), "-U", "stalwart",
                            "-d", base, "-qAt", "-c", sql], capture_output=True, text=True, timeout=30)
        assert r.returncode == 0, r.stderr
        return r.stdout.strip()

    def pg_ctl_replica(self, *orden):
        subprocess.run([os.path.join(self.bin, "pg_ctl"), "-D", self.env["BEZENTI_PG_DIR_REPLICA"], "-w", *orden,
                        "-l", os.path.join(self.dir, "replica.log")], check=True, capture_output=True, timeout=60)

    def lecturas_replica(self, base):
        # Filas leídas por las consultas servidas en la réplica (estadísticas propias del servidor réplica).
        self.psql(self.p2, "SELECT pg_stat_force_next_flush()", base)
        return int(self.psql(self.p2, "SELECT coalesce(sum(seq_tup_read + idx_tup_fetch), 0) "
                                      "FROM pg_stat_user_tables", base) or 0)


def prueba_replicas(binario, dir_pg):
    print("== 2. almacén de datos PostgreSQL con una réplica de lectura")
    pg = Pg(dir_pg)
    base = f"servidor_{os.getpid()}"
    pg.psql(pg.p1, f"CREATE DATABASE {base}")
    srv = Servidor(binario)
    try:
        ajustes = {"host": "127.0.0.1", "database": base, "authUsername": "stalwart",
                   "authSecret": {"@type": "None"}}
        with open(srv.config, "w") as f:
            json.dump({"@type": "PostgreSql", "port": pg.p1, **ajustes,
                       "readReplicas": {"0": {"port": pg.p2, **ajustes}}}, f)
        configurar(srv)
        usr = crear_usuario(srv)
        asuntos = {f"Replicado {i}": f"Cuerpo replicado {i}" for i in range(10)}
        for a, c in asuntos.items():
            ok(enviar(srv, "ana@reparto.test", a, c) is None, f"SMTP acepta «{a}»")
        ok(esperar(lambda: len(leer_imap(srv, asuntos)) == 10), "los 10 correos se leen por IMAP")

        print("== 2b. leer lo que se acaba de escribir: el correo recién entregado se ve al momento")
        bien = 0
        for i in range(10):
            a = f"Inmediato {i}"
            enviar(srv, "ana@reparto.test", a, f"inmediato {i}")
            if esperar(lambda: a in leer_imap(srv, {a}), segundos=20, paso=0.05):
                bien += 1
        # (Sin filtro de texto: el índice de búsqueda se actualiza aparte y aún puede no tenerlo.)
        _, r = usr.una("Email/query", {"accountId": usr.cuenta()}, USING_MAIL)
        eid = (r.get("ids") or [None])[0]
        ok(eid is not None, "hay correos para cambiar marcas")
        viejos = 0
        for i in range(20):
            marcar = i % 2 == 0
            usr.una("Email/set", {"accountId": usr.cuenta(),
                                  "update": {eid: {"keywords/$flagged": True if marcar else None}}}, USING_MAIL)
            _, r = usr.una("Email/get", {"accountId": usr.cuenta(), "ids": [eid], "properties": ["keywords"]},
                           USING_MAIL)
            kw = ((r.get("list") or [{}])[0]).get("keywords") or {}
            if bool(kw.get("$flagged")) != marcar:
                viejos += 1
        ok(bien == 10, "los 10 correos inmediatos se leen", f"{bien}/10")
        ok(viejos == 0, "escribir una marca y leerla al momento nunca devuelve el valor viejo", f"{viejos} viejos")

        print("== 2c. la réplica atiende lecturas")
        time.sleep(1.5)
        antes = pg.lecturas_replica(base)
        for _ in range(5):
            leer_imap(srv, asuntos)
            usr.una("Email/query", {"accountId": usr.cuenta()}, USING_MAIL)
        time.sleep(1.0)
        despues = pg.lecturas_replica(base)
        print(f"     filas leídas en la réplica: {antes} → {despues}")
        ok(despues > antes, "las estadísticas de la réplica suben al leer correo", f"{antes} → {despues}")

        print("== 2d. la réplica cae: el servidor sigue funcionando con el primario")
        pg.pg_ctl_replica("-m", "immediate", "stop")
        try:
            ok(len(leer_imap(srv, asuntos)) == 10, "IMAP sigue leyendo los 10 correos")
            ok(enviar(srv, "ana@reparto.test", "Con la replica caida", "sin replica") is None,
               "SMTP sigue aceptando correo")
            ok(esperar(lambda: "Con la replica caida" in leer_imap(srv, {"Con la replica caida"}), segundos=30),
               "y el correo nuevo se lee por IMAP")
            _, r = usr.una("Email/query", {"accountId": usr.cuenta()}, USING_MAIL)
            ok(len(r.get("ids") or []) >= 21, "JMAP sigue respondiendo", json.dumps(r)[:200])
        finally:
            pg.pg_ctl_replica("start")

        print("== 2e. la réplica vuelve y atiende lecturas otra vez")
        time.sleep(2.0)
        antes = pg.lecturas_replica(base)

        def vuelve():
            leer_imap(srv, asuntos)
            time.sleep(0.5)
            return pg.lecturas_replica(base) > antes
        ok(esperar(vuelve, segundos=30, paso=0.5), "la réplica recuperada vuelve a usarse")
    finally:
        srv.parar()
        guardar_registro(srv, "replicas")


def guardar_registro(srv, nombre):
    if mi.FALLOS or sys.exc_info()[0] is not None:
        destino = os.path.join(tempfile.gettempdir(), f"bezenti-{nombre}.log")
        shutil.copy(srv.log, destino)
        print(f"\nregistro del servidor en {destino}")
    else:
        shutil.rmtree(srv.dir, ignore_errors=True)


def main():
    args = sys.argv[1:]
    if not args:
        raise SystemExit(__doc__)
    binario = os.path.abspath(args[0])
    dir_pg = args[args.index("--pg") + 1] if "--pg" in args else None
    error = None
    try:
        prueba_reparto(binario)
        if dir_pg:
            prueba_replicas(binario, dir_pg)
        else:
            print("== 2. réplicas de lectura: omitida (sin --pg)")
    except Exception as e:  # noqa: BLE001
        error = e
        mi.FALLOS.append(f"excepción: {e!r}")
    if error is not None:
        import traceback
        traceback.print_exception(error)
    print(f"\n{len(mi.PROBADOS) - len(mi.FALLOS)}/{len(mi.PROBADOS)} comprobaciones correctas")
    if mi.FALLOS:
        print("FALLOS:\n  " + "\n  ".join(mi.FALLOS))
        sys.exit(1)


if __name__ == "__main__":
    main()
