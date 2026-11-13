#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 bezenti
#
# SPDX-License-Identifier: AGPL-3.0-only
"""Pruebas de integración del multi-inquilino (edición bezenti, fase 2).

Levanta el servidor real (binario `stalwart` compilado con `sqlite` y `bezenti`) con una base SQLite nueva y lo
prueba **por la API real** (JMAP, SMTP, IMAP y HTTP), sin más dependencias que la biblioteca estándar de Python.

Escenario: el operador (superadmin) crea los inquilinos A (revendedor) y B, sus dominios y administradores; el
administrador de A crea su inquilino hijo A1. Después se intenta todo lo que un administrador de inquilino no debe
poder hacer (ver bezenti/especificaciones/multi-inquilino.md, §4) y se comprueba que no puede: leer, listar,
contar, modificar o borrar objetos de otro inquilino por cualquier método, moverlos, robar un nombre de dominio,
apuntar a objetos ajenos, subirse los permisos, tocar la configuración del servidor, compartir con otro inquilino…
También se prueban las cuotas, la jerarquía de revendedores, el logo y el flujo de correo (SMTP → IMAP/JMAP).

Uso: multi_inquilino.py RUTA_AL_BINARIO_STALWART
Termina con código 0 si todo pasa. Deja el registro del servidor en $TMPDIR si algo falla.
"""

import base64
import imaplib
import json
import os
import shutil
import signal
import smtplib
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

USING = ["urn:ietf:params:jmap:core", "urn:stalwart:jmap"]
USING_MAIL = ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"]
USING_PRINCIPALS = ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:principals"]
ADMIN_PW = "Rec0very-Adm1n-Secret!"
PW = "Cl4ve-Muy-Segura-2026!"
PNG = b"\x89PNG\r\n\x1a\n" + b"\x00" * 32

FALLOS = []
PROBADOS = []


def ok(cond, nombre, detalle=""):
    """Registra un ataque o comprobación."""
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


class Jmap:
    def __init__(self, base, usuario, clave):
        self.base = base
        self.usuario = usuario
        self.auth = "Basic " + base64.b64encode(f"{usuario}:{clave}".encode()).decode()
        self._sesion = None

    def http(self, metodo, ruta, cuerpo=None, cabeceras=None):
        datos = None if cuerpo is None else json.dumps(cuerpo).encode()
        req = urllib.request.Request(self.base + ruta, data=datos, method=metodo)
        req.add_header("Authorization", self.auth)
        if datos is not None:
            req.add_header("Content-Type", "application/json")
        for k, v in (cabeceras or {}).items():
            req.add_header(k, v)
        try:
            with urllib.request.urlopen(req, timeout=60) as r:
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

    def llamar(self, llamadas, using=USING):
        estado, cuerpo = self.http(
            "POST", "/jmap/", {"using": using, "methodCalls": [[m, a, f"c{i}"] for i, (m, a) in enumerate(llamadas)]}
        )
        if estado != 200:
            return [["error", {"type": f"http{estado}", "description": cuerpo[:300].decode("utf-8", "replace")}, "c0"]]
        return json.loads(cuerpo)["methodResponses"]

    def una(self, metodo, args, using=USING):
        r = self.llamar([(metodo, args)], using)[0]
        return r[0], r[1]

    def crear(self, objeto, datos):
        nombre, r = self.una(f"{objeto}/set", {"create": {"n": datos}})
        if nombre == "error" or "n" not in (r.get("created") or {}):
            raise AssertionError(f"{self.usuario} no pudo crear {objeto}: {json.dumps(r)[:600]}")
        return r["created"]["n"]["id"]

    def get(self, objeto, ids=None, props=None):
        args = {"ids": ids}
        if props is not None:
            args["properties"] = props
        return self.una(f"{objeto}/get", args)

    def query(self, objeto, filtro=None, **extra):
        args = dict(extra)
        if filtro is not None:
            args["filter"] = filtro
        return self.una(f"{objeto}/query", args)

    def set(self, objeto, **args):
        return self.una(f"{objeto}/set", args)


def ids_de(r):
    return set(r.get("ids") or [])


def no_encontrado(nombre, r, ids):
    """get de ids ajenos: todos en notFound y ninguno en list."""
    if nombre == "error":
        return True
    vistos = {o.get("id") for o in r.get("list", [])}
    return not (vistos & set(ids)) and set(ids) <= set(r.get("notFound") or [])


def error_tipo(nombre, r, tipo):
    return nombre == "error" and r.get("type") == tipo


class Servidor:
    def __init__(self, binario):
        self.binario = binario
        self.dir = tempfile.mkdtemp(prefix="bezenti-mt-")
        self.config = os.path.join(self.dir, "config.json")
        with open(self.config, "w") as f:
            json.dump({"@type": "Sqlite", "path": os.path.join(self.dir, "datos")}, f)
        self.p_rec = puerto_libre()
        self.p_http = puerto_libre()
        self.p_smtp = puerto_libre()
        self.p_imap = puerto_libre()
        self.log = os.path.join(self.dir, "servidor.log")
        self.proc = None

    def arrancar(self, recuperacion):
        env = dict(os.environ)
        env.update(
            {
                "STALWART_HOSTNAME": "mail.operador.test",
                "STALWART_RECOVERY_ADMIN": f"admin:{ADMIN_PW}",
                "STALWART_RECOVERY_MODE_PORT": str(self.p_rec),
            }
        )
        if recuperacion:
            env["STALWART_RECOVERY_MODE"] = "1"
        else:
            env.pop("STALWART_RECOVERY_MODE", None)
        self.proc = subprocess.Popen(
            [self.binario, "--config", self.config],
            env=env,
            stdout=open(self.log, "a"),
            stderr=subprocess.STDOUT,
            cwd=self.dir,
        )
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
    """Modo recuperación: listeners en puertos altos sin TLS, registro a la consola y sin descargas externas."""
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
    n, r = rec.set("x:SpamSettings", update={"singleton": {"enable": False, "spamFilterRulesUrl": None}})
    assert "singleton" in (r.get("updated") or {}), f"x:SpamSettings: {r}"
    n, r = rec.set("x:Imap", update={"singleton": {"allowPlainTextAuth": True}})
    assert "singleton" in (r.get("updated") or {}), f"x:Imap: {r}"
    n, r = rec.set("x:MtaStageAuth", update={"singleton": {"require": {"else": "false"}}})
    assert "singleton" in (r.get("updated") or {}), f"x:MtaStageAuth: {r}"
    _, apps = rec.query("x:Application")
    if apps.get("ids"):
        rec.set("x:Application", update={i: {"enable": False} for i in apps["ids"]})
    srv.parar()
    srv.arrancar(recuperacion=False)
    esperar_puerto(srv.p_smtp)
    esperar_puerto(srv.p_imap)


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
        FALLOS.append(f"excepción: {e!r}")
    finally:
        srv.parar()
        if FALLOS:
            destino = os.path.join(tempfile.gettempdir(), "bezenti-multi-inquilino.log")
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


def pruebas(srv):
    base = f"http://127.0.0.1:{srv.p_http}"
    rec = Jmap(base, "admin", ADMIN_PW)

    print("== el operador prepara el servidor")
    dom_op = rec.crear("x:Domain", {"name": "operador.test", "dkimManagement": {"@type": "Manual"}})
    root_id = rec.crear("x:Account", {
        "@type": "User", "name": "root", "domainId": dom_op, "roles": {"@type": "Admin"},
        "credentials": {"0": {"@type": "Password", "secret": PW}},
    })
    root = Jmap(base, "root@operador.test", PW)
    roles = root.get("x:Role")[1]["list"]
    rol_sysadmin = next(r["id"] for r in roles if r.get("description") == "System Administrator")
    rol_tenant = [r for r in roles if r.get("description") == "Tenant Administrator"]
    ok(len(rol_tenant) == 1, "se crea el rol por defecto «Tenant Administrator»")
    auth = root.get("x:Authentication")[1]["list"][0]
    ok(rol_tenant and rol_tenant[0]["id"] in (auth.get("defaultTenantRoleIds") or {}),
       "defaultTenantRoleIds incluye Tenant Administrator")

    reventa = {p: True for p in ("sysTenantGet", "sysTenantCreate", "sysTenantUpdate", "sysTenantDestroy", "sysTenantQuery")}
    t_a = root.crear("x:Tenant", {"name": "Empresa A", "permissions": {"@type": "Merge", "enabledPermissions": reventa, "disabledPermissions": {}},
                                  "quotas": {"maxDomains": 3}, "logo": "data:image/png;base64," + base64.b64encode(PNG).decode()})
    t_b = root.crear("x:Tenant", {"name": "Empresa B", "quotas": {"maxAccounts": 2, "maxDiskQuota": 4000}})
    nombre, r = root.set("x:Tenant", create={"d": {"name": "Empresa A"}})
    ok("d" in (r.get("notCreated") or {}), "el nombre de inquilino es único", json.dumps(r)[:300])

    dom_a = root.crear("x:Domain", {"name": "a.test", "memberTenantId": t_a})
    dom_b = root.crear("x:Domain", {"name": "b.test", "memberTenantId": t_b})
    root.crear("x:Account", {"@type": "User", "name": "admin", "domainId": dom_a, "memberTenantId": t_a,
                             "roles": {"@type": "Admin"}, "credentials": {"0": {"@type": "Password", "secret": PW}}})
    admin_b_id = root.crear("x:Account", {"@type": "User", "name": "admin", "domainId": dom_b, "memberTenantId": t_b,
                                          "roles": {"@type": "Admin"}, "credentials": {"0": {"@type": "Password", "secret": PW}}})
    nombre, r = root.set("x:Account", create={"x": {"@type": "User", "name": "cruzado", "domainId": dom_b, "memberTenantId": t_a,
                                                   "credentials": {"0": {"@type": "Password", "secret": PW}}}})
    ok("x" in (r.get("notCreated") or {}), "ni el operador puede poner una cuenta de A en un dominio de B", json.dumps(r)[:300])

    adm_a = Jmap(base, "admin@a.test", PW)
    adm_b = Jmap(base, "admin@b.test", PW)

    print("== el administrador de A (revendedor) crea su inquilino hijo A1")
    t_a1 = adm_a.crear("x:Tenant", {"name": "Cliente A1"})
    _, r = root.get("x:Tenant", [t_a1])
    ok(r["list"] and r["list"][0].get("bezentiParentTenantId") == t_a, "el padre de A1 es A (relleno automático)")
    dom_a1 = adm_a.crear("x:Domain", {"name": "a1.test", "memberTenantId": t_a1})
    adm_a.crear("x:Account", {"@type": "User", "name": "admin", "domainId": dom_a1, "memberTenantId": t_a1,
                              "roles": {"@type": "Admin"}, "credentials": {"0": {"@type": "Password", "secret": PW}}})
    adm_a1 = Jmap(base, "admin@a1.test", PW)
    user_a = adm_a.crear("x:Account", {"@type": "User", "name": "usuario", "domainId": dom_a,
                                       "credentials": {"0": {"@type": "Password", "secret": PW}}})
    _, r = adm_a.get("x:Account", [user_a], ["memberTenantId"])
    ok(r["list"] and r["list"][0].get("memberTenantId") == t_a, "lo que crea el admin de A queda en A")
    user_b = adm_b.crear("x:Account", {"@type": "User", "name": "usuario", "domainId": dom_b,
                                       "credentials": {"0": {"@type": "Password", "secret": PW}}})
    group_b = adm_b.crear("x:Account", {"@type": "Group", "name": "ventas", "domainId": dom_b})
    role_b = adm_b.crear("x:Role", {"description": "Rol de B", "enabledPermissions": {"authenticate": True}})
    list_b = adm_b.crear("x:MailingList", {"name": "todos", "domainId": dom_b, "recipients": {"usuario@b.test": True}})
    user_a1 = adm_a1.crear("x:Account", {"@type": "User", "name": "usuario", "domainId": dom_a1,
                                         "credentials": {"0": {"@type": "Password", "secret": PW}}})

    # Inventario de todo lo que no es de A (ni de sus descendientes), visto por el operador.
    ajenos = {}
    for obj in ("x:Account", "x:Domain", "x:Role", "x:MailingList", "x:DkimSignature", "x:OAuthClient"):
        _, r = root.query(obj)
        todos = ids_de(r)
        _, r = root.get(obj, list(todos), ["memberTenantId"])
        ajenos[obj] = [o["id"] for o in r.get("list", []) if o.get("memberTenantId") not in (t_a, t_a1)]
    ajenos["x:Tenant"] = [t_a, t_b]  # su propio inquilino y su hermano

    print("== ataques del administrador de A contra B y contra el operador")
    n, r = adm_a.get("x:Domain", [dom_b, dom_op])
    ok(no_encontrado(n, r, [dom_b, dom_op]), "get de dominios ajenos por id adivinado → notFound")
    n, r = adm_a.get("x:Account", [user_b, admin_b_id, root_id, group_b])
    ok(no_encontrado(n, r, [user_b, admin_b_id, root_id, group_b]), "get de cuentas ajenas por id → notFound")
    n, r = adm_a.get("x:Domain")
    vistos = {o["id"] for o in r.get("list", [])}
    ok(vistos == {dom_a, dom_a1} and not r.get("notFound"), "get sin ids solo devuelve lo suyo y no filtra ids ajenos", str(vistos))
    n, r = adm_a.query("x:Domain")
    ok(ids_de(r) == {dom_a, dom_a1}, "query sin filtro de dominios: solo A y A1", json.dumps(r)[:300])
    n, r = adm_a.query("x:Domain", calculateTotal=True)
    ok(r.get("total") == 2, "query con calculateTotal no cuenta dominios ajenos", json.dumps(r)[:300])
    n, r = adm_a.query("x:Account")
    ok(not (ids_de(r) & {user_b, admin_b_id, root_id, group_b}) and user_a in ids_de(r) and user_a1 in ids_de(r),
       "query sin filtro de cuentas: solo A y A1", json.dumps(r)[:300])
    n, r = adm_a.query("x:Account", limit=1, sort=[{"property": "id", "isAscending": True}])
    ok(len(r.get("ids") or []) == 1 and not (ids_de(r) & {user_b, admin_b_id, root_id}), "query paginada no se sale del ámbito")
    n, r = adm_a.query("x:Account", {"memberTenantId": t_b})
    ok(n != "error" and not ids_de(r), "query filtrando por el inquilino B → vacío")
    n, r = adm_a.query("x:Domain", {"name": "b.test"})
    ok(n == "error" or not ids_de(r), "query por nombre del dominio de B → vacío")
    n, r = adm_a.query("x:Account", {"text": "admin"})
    ok(not (ids_de(r) & {admin_b_id, root_id}), "búsqueda de texto no encuentra cuentas ajenas")
    n, r = adm_a.query("x:Role")
    ok(role_b not in ids_de(r) and rol_sysadmin not in ids_de(r), "query de roles: ni los de B ni los del operador")
    n, r = adm_a.query("x:MailingList")
    ok(list_b not in ids_de(r), "query de listas: no ve las de B")
    n, r = adm_a.set("x:Account", update={user_b: {"description": "hackeado"}})
    ok((r.get("notUpdated") or {}).get(user_b, {}).get("type") == "notFound", "modificar una cuenta de B → notFound")
    n, r = adm_a.set("x:Account", destroy=[user_b])
    ok((r.get("notDestroyed") or {}).get(user_b, {}).get("type") == "notFound", "borrar una cuenta de B → notFound")
    n, r = adm_a.set("x:Domain", update={dom_b: {"memberTenantId": t_a}})
    ok((r.get("notUpdated") or {}).get(dom_b, {}).get("type") == "notFound", "robarse el dominio de B cambiando su inquilino → notFound")
    n, r = adm_a.set("x:Account", update={user_a: {"memberTenantId": t_b}})
    ok(user_a in (r.get("notUpdated") or {}), "mover una cuenta propia al inquilino B → rechazado")
    n, r = adm_a.set("x:Account", update={user_a: {"memberTenantId": None}})
    ok(user_a in (r.get("notUpdated") or {}), "sacar una cuenta del inquilino (dejarla del operador) → rechazado")
    n, r = adm_a.set("x:Account", create={"x": {"@type": "User", "name": "intruso", "domainId": dom_a, "memberTenantId": t_b,
                                                "credentials": {"0": {"@type": "Password", "secret": PW}}}})
    ok("x" in (r.get("notCreated") or {}), "crear una cuenta en el inquilino B → rechazado")
    n, r = adm_a.set("x:Domain", create={"x": {"name": "b.test"}})
    err = (r.get("notCreated") or {}).get("x", {})
    ok(err.get("type") == "primaryKeyViolation" and "objectId" not in err, "crear un dominio con el nombre del de B → rechazado sin revelar su id", json.dumps(r)[:300])
    n, r = adm_a.set("x:Domain", update={dom_a: {"aliases": {"b.test": True}}})
    err = (r.get("notUpdated") or {}).get(dom_a, {})
    ok(err.get("type") == "primaryKeyViolation" and "objectId" not in err, "alias de dominio con el nombre del de B → rechazado sin revelar su id", json.dumps(r)[:300])
    n, r = adm_a.set("x:Account", create={"x": {"@type": "User", "name": "pirata", "domainId": dom_b,
                                                "credentials": {"0": {"@type": "Password", "secret": PW}}}})
    ok((r.get("notCreated") or {}).get("x", {}).get("type") == "invalidForeignKey", "cuenta en el dominio de B → invalidForeignKey", json.dumps(r)[:300])
    n, r = adm_a.set("x:Account", update={user_a: {"aliases": {"0": {"name": "ceo", "domainId": dom_b, "enable": True}}}})
    ok((r.get("notUpdated") or {}).get(user_a, {}).get("type") == "invalidForeignKey", "alias en el dominio de B → invalidForeignKey", json.dumps(r)[:300])
    n, r = adm_a.set("x:Account", update={user_a: {"memberGroupIds": {group_b: True}}})
    ok((r.get("notUpdated") or {}).get(user_a, {}).get("type") == "invalidForeignKey", "meterse en un grupo de B → invalidForeignKey", json.dumps(r)[:300])
    n, r = adm_a.set("x:MailingList", create={"x": {"name": "lista", "domainId": dom_b, "recipients": {"x@a.test": True}}})
    ok((r.get("notCreated") or {}).get("x", {}).get("type") == "invalidForeignKey", "lista en el dominio de B → invalidForeignKey")
    n, r = adm_a.set("x:Account", update={user_a: {"roles": {"@type": "Custom", "roleIds": {rol_sysadmin: True}}}})
    ok((r.get("notUpdated") or {}).get(user_a, {}).get("type") in ("invalidForeignKey", "forbidden"),
       "darse el rol System Administrator del operador → rechazado", json.dumps(r)[:300])
    n, r = adm_a.set("x:Account", update={user_a: {"roles": {"@type": "Custom", "roleIds": {role_b: True}}}})
    ok((r.get("notUpdated") or {}).get(user_a, {}).get("type") == "invalidForeignKey", "usar un rol de B → invalidForeignKey")
    _, r = adm_a.una("x:Account/get", {"ids": None, "properties": ["name"]})
    adm_a_id = next(o["id"] for o in r["list"] if o.get("name") == "admin" and o["id"] != user_a)
    n, r = adm_a.set("x:Account", update={adm_a_id: {"permissions": {"@type": "Merge", "enabledPermissions": {"impersonate": True, "sysNetworkListenerUpdate": True}, "disabledPermissions": {}}}})
    ok(adm_a_id in (r.get("notUpdated") or {}), "subirse los permisos (impersonate, configuración del servidor) → rechazado", json.dumps(r)[:300])
    n, r = adm_a.set("x:Account", create={"x": {"@type": "User", "name": "superman", "domainId": dom_a, "roles": {"@type": "Custom", "roleIds": {}},
                                                "permissions": {"@type": "Replace", "enabledPermissions": {"authenticate": True, "impersonate": True}, "disabledPermissions": {}},
                                                "credentials": {"0": {"@type": "Password", "secret": PW}}}})
    ok("x" in (r.get("notCreated") or {}), "crear una cuenta con impersonate → rechazado", json.dumps(r)[:300])
    n, r = adm_a.set("x:Role", create={"x": {"description": "Rol trampa", "enabledPermissions": {"sysNetworkListenerGet": True}}})
    ok("x" in (r.get("notCreated") or {}), "crear un rol con permisos del operador → rechazado", json.dumps(r)[:300])
    n, r = adm_a.get("x:NetworkListener")
    ok(error_tipo(n, r, "forbidden"), "tras los intentos, sigue sin acceso a la configuración del servidor")
    n, r = adm_a.set("x:DnsServer", create={"x": {"@type": "Tsig", "host": "1.1.1.1", "keyName": "k", "key": {"@type": "File", "filePath": "/etc/passwd"}}})
    ok("x" in (r.get("notCreated") or {}) and "File" in json.dumps(r), "secreto leído de un archivo del servidor → rechazado", json.dumps(r)[:300])
    n, r = adm_a.set("x:Domain", update={dom_a: {"logo": "https://127.0.0.1/x.png"}})
    # (la URL se acepta como dato; al servirla se aplica PublicOnly y no se descarga: ver la prueba del logo)

    for obj in ("x:NetworkListener", "x:SystemSettings", "x:DataStore", "x:Tracer", "x:Log",
                "x:Authentication", "x:Application", "x:DmarcInternalReport"):
        n, r = adm_a.get(obj)
        ok(error_tipo(n, r, "forbidden"), f"{obj}/get (configuración del servidor) → forbidden", json.dumps(r)[:200])
    # Fase 8: la cola, las cuentas borradas (x:Task) y los informes externos sí los ve, pero solo lo de su ámbito:
    # aquí no hay nada suyo, así que no ve nada (ni las tareas ni la cola del servidor).
    for obj in ("x:QueuedMessage", "x:Task", "x:ArfExternalReport"):
        n, r = adm_a.get(obj)
        ok(n != "error" and not r.get("list"), f"{obj}/get → solo lo de su ámbito (nada ajeno)", json.dumps(r)[:200])
    n, r = adm_a.query("x:Task")
    ok(n != "error" and not r.get("ids"), "x:Task/query → sin tareas del servidor", json.dumps(r)[:200])
    n, r = adm_a.query("x:QueuedMessage")
    ok(n != "error" and not r.get("ids"), "x:QueuedMessage/query → sin mensajes ajenos", json.dumps(r)[:200])
    n, r = adm_a.set("x:NetworkListener", create={"x": {"name": "puerta", "protocol": "http", "bind": {"0.0.0.0:9": True}}})
    ok(n == "error" or "x" in (r.get("notCreated") or {}), "crear un listener → rechazado")
    n, r = adm_a.set("x:Action", create={"x": {"@type": "InvalidateCaches"}})
    ok(n == "error" or "x" in (r.get("notCreated") or {}), "lanzar acciones del servidor → rechazado")
    n, r = adm_a.una("x:Domain/changes", {"sinceState": "0"})
    ok(n == "error", "x:Domain/changes no existe (no hay nada que filtrar)")
    n, r = adm_a.una("x:ApiKey/get", {"accountId": user_b, "ids": None})
    ok(n == "error", "usar el accountId de otra cuenta (suplantación) → error", json.dumps(r)[:200])

    print("== x:Tenant y revendedores")
    n, r = adm_a.get("x:Tenant", [t_a, t_b, t_a1])
    vistos = {o["id"] for o in r.get("list", [])}
    ok(vistos == {t_a1}, "el revendedor A solo ve a su hijo A1 (ni a sí mismo ni a B)", str(vistos))
    n, r = adm_a.query("x:Tenant")
    ok(ids_de(r) == {t_a1}, "x:Tenant/query del revendedor → solo A1")
    n, r = adm_a.set("x:Tenant", update={t_a: {"quotas": {"maxDomains": 1000}}})
    ok((r.get("notUpdated") or {}).get(t_a, {}).get("type") == "notFound", "el revendedor no puede subirse sus cuotas")
    n, r = adm_a.set("x:Tenant", update={t_a1: {"bezentiParentTenantId": t_b}})
    ok(t_a1 in (r.get("notUpdated") or {}), "mover A1 bajo B → rechazado")
    n, r = adm_a.set("x:Tenant", update={t_a1: {"bezentiParentTenantId": t_a1}})
    ok(t_a1 in (r.get("notUpdated") or {}), "A1 como padre de sí mismo → rechazado")
    n, r = adm_a.set("x:Tenant", update={t_a1: {"bezentiParentTenantId": None}})
    ok(t_a1 in (r.get("notUpdated") or {}), "convertir A1 en inquilino de primer nivel → rechazado")
    n, r = adm_a.set("x:Tenant", create={"x": {"name": "Hijo de B", "bezentiParentTenantId": t_b}})
    ok("x" in (r.get("notCreated") or {}), "crear un inquilino bajo B → rechazado")
    n, r = adm_a.set("x:Tenant", update={t_a1: {"permissions": {"@type": "Merge", "enabledPermissions": {"sysNetworkListenerGet": True}, "disabledPermissions": {}}}})
    n, r = adm_a1.get("x:NetworkListener")
    ok(error_tipo(n, r, "forbidden"), "el revendedor no puede dar a su hijo más de lo que tiene")
    n, r = root.get("x:Tenant", [t_b])
    n2, r2 = root.set("x:Tenant", update={t_b: {"bezentiParentTenantId": t_b}})
    ok(t_b in (r2.get("notUpdated") or {}), "ni el operador puede crear ciclos de inquilinos")

    n, r = adm_a.get("x:Account", [user_a1])
    ok(len(r.get("list", [])) == 1, "el revendedor A ve las cuentas de A1")
    n, r = adm_a.set("x:Account", update={user_a1: {"description": "gestionado por A"}})
    ok(user_a1 in (r.get("updated") or {}), "el revendedor A modifica cuentas de A1", json.dumps(r)[:300])
    n, r = adm_a.set("x:Domain", update={dom_a1: {"memberTenantId": t_a}})
    ok((r.get("notUpdated") or {}).get(dom_a1, {}).get("type") == "objectIsLinked",
       "mover a A el dominio de A1 mientras A1 tiene cuentas en él → objectIsLinked", json.dumps(r)[:300])
    n, r = adm_a1.get("x:Tenant", [t_a, t_a1, t_b])
    ok(not r.get("list"), "A1 no ve a su padre, ni a sí mismo, ni a B")
    n, r = adm_a1.query("x:Domain")
    ok(ids_de(r) == {dom_a1}, "A1 solo ve su dominio", json.dumps(r)[:200])
    n, r = adm_a1.get("x:Account", [user_a, user_b])
    ok(no_encontrado(n, r, [user_a, user_b]), "A1 no ve cuentas de A ni de B")
    n, r = adm_a1.set("x:Tenant", create={"x": {"name": "Nieto"}})
    ok(n == "error" or "x" in (r.get("notCreated") or {}), "A1 (sin permisos de revendedor) no crea inquilinos")
    n, r = adm_a1.set("x:Account", update={user_a: {"description": "x"}})
    ok((r.get("notUpdated") or {}).get(user_a, {}).get("type") == "notFound", "A1 no modifica cuentas de su padre")

    print("== prueba «fuzz»: cada método del registro con ids de fuera del ámbito")
    for atacante, nombre_atacante in ((adm_a, "admin A"), (adm_a1, "admin A1")):
        for obj, ids in ajenos.items():
            if atacante is adm_a1 and obj == "x:Tenant":
                ids = [t_a, t_b, t_a1]
            if not ids:
                continue
            n, r = atacante.get(obj, ids)
            ok(no_encontrado(n, r, ids), f"[{nombre_atacante}] {obj}/get con {len(ids)} ids ajenos → notFound")
            n, r = atacante.set(obj, update={i: {"description": "fuzz"} for i in ids})
            ok(n == "error" or all((r.get("notUpdated") or {}).get(i, {}).get("type") in ("notFound", "forbidden") for i in ids),
               f"[{nombre_atacante}] {obj}/set update con ids ajenos → notFound", json.dumps(r)[:200])
            n, r = atacante.set(obj, destroy=ids)
            ok(n == "error" or all((r.get("notDestroyed") or {}).get(i, {}).get("type") in ("notFound", "forbidden") for i in ids),
               f"[{nombre_atacante}] {obj}/set destroy con ids ajenos → notFound", json.dumps(r)[:200])
            n, r = atacante.query(obj)
            ok(n == "error" or not (ids_de(r) & set(ids)), f"[{nombre_atacante}] {obj}/query no devuelve ids ajenos")
    _, r = root.query("x:Domain")
    ok({dom_op, dom_a, dom_b, dom_a1} <= ids_de(r), "el operador ve todos los dominios")
    _, r = root.query("x:Tenant")
    ok({t_a, t_b, t_a1} <= ids_de(r), "el operador ve todos los inquilinos")

    print("== directorio de principales y compartir")
    usr_a = Jmap(base, "usuario@a.test", PW)
    n, r = adm_a.una("Principal/query", {"accountId": adm_a.cuenta(), "filter": {}}, USING_PRINCIPALS)
    ok(n != "error" and user_a in ids_de(r) and not (ids_de(r) & {user_b, admin_b_id, root_id, user_a1}),
       "Principal/query: solo cuentas de su inquilino", json.dumps(r)[:300])
    n, r = adm_a.una("Principal/query", {"accountId": adm_a.cuenta(), "filter": {"email": "usuario@b.test"}}, USING_PRINCIPALS)
    ok(n != "error" and not ids_de(r), "Principal/query por la dirección de una cuenta de B → vacío", json.dumps(r)[:300])
    n, r = adm_a.una("Principal/get", {"accountId": adm_a.cuenta(), "ids": [user_b, root_id]}, USING_PRINCIPALS)
    ok(n != "error" and no_encontrado(n, r, [user_b, root_id]), "Principal/get de cuentas de otro inquilino → notFound", json.dumps(r)[:300])
    n, r = usr_a.una("Mailbox/get", {"accountId": usr_a.cuenta(), "ids": None, "properties": ["role"]}, USING_MAIL)
    inbox = next(m["id"] for m in r["list"] if m.get("role") == "inbox")
    n, r = usr_a.una("Mailbox/set", {"accountId": usr_a.cuenta(), "update": {inbox: {"shareWith": {user_b: {"mayRead": True}}}}},
                     USING_MAIL + ["urn:ietf:params:jmap:principals"])
    ok(n != "error" and inbox in (r.get("notUpdated") or {}), "compartir un buzón con una cuenta de B → rechazado", json.dumps(r)[:300])
    n, r = usr_a.una("x:Account/query", {})
    ok(error_tipo(n, r, "forbidden"), "un usuario normal de A no puede listar cuentas", json.dumps(r)[:200])
    imap = imaplib.IMAP4("127.0.0.1", srv.p_imap)
    imap.login("usuario@a.test", PW)
    try:
        typ, _ = imap.setacl("INBOX", "usuario@b.test", "lr")
    except imaplib.IMAP4.error:
        typ = "NO"
    ok(typ != "OK", "IMAP SETACL con una cuenta de B → rechazado")
    try:
        typ, _ = imap.setacl("INBOX", "admin@a.test", "lr")
    except imaplib.IMAP4.error as e:
        typ = str(e)
    ok(typ == "OK", "IMAP SETACL con una cuenta del mismo inquilino → permitido", str(typ))
    imap.logout()

    print("== cuotas")
    n, r = adm_b.set("x:Account", create={"x": {"@type": "User", "name": "tercero", "domainId": dom_b,
                                                "credentials": {"0": {"@type": "Password", "secret": PW}}}})
    err = (r.get("notCreated") or {}).get("x", {})
    ok(err.get("type") == "overQuota" and "quota of 2 accounts" in (err.get("description") or ""),
       "maxAccounts de B (2: admin y usuario; el grupo no cuenta) → overQuota", json.dumps(r)[:300])
    n, r = adm_a1.set("x:Domain", create={"x": {"name": "otro-a1.test"}})
    ok("x" in (r.get("created") or {}), "A1 crea un dominio mientras A tiene cuota (3)", json.dumps(r)[:300])
    n, r = adm_a1.set("x:Domain", create={"x": {"name": "otro2-a1.test"}})
    ok((r.get("notCreated") or {}).get("x", {}).get("type") == "overQuota", "cuota jerárquica: maxDomains de A limita a su hijo A1", json.dumps(r)[:300])
    n, r = root.set("x:Domain", create={"x": {"name": "otro-a.test", "memberTenantId": t_a}})
    ok((r.get("notCreated") or {}).get("x", {}).get("type") == "overQuota", "las cuotas también valen para el operador")
    n, r = root.set("x:Domain", update={dom_op: {"memberTenantId": t_a}})
    ok(dom_op in (r.get("notUpdated") or {}), "mover un dominio a un inquilino lleno → rechazado")

    print("== correo: SMTP → IMAP/JMAP en un dominio del inquilino A")
    msg = ("From: remitente@externo.example\r\nTo: usuario@a.test\r\nSubject: Prueba multi-inquilino\r\n"
           "Message-ID: <mt-1@externo.example>\r\n\r\nHola desde fuera.\r\n")
    with smtplib.SMTP("127.0.0.1", srv.p_smtp, timeout=30) as s:
        s.ehlo("externo.example")
        s.sendmail("remitente@externo.example", ["usuario@a.test"], msg)
    encontrado = False
    for _ in range(60):
        n, r = usr_a.una("Email/query", {"accountId": usr_a.cuenta(), "filter": {"subject": "multi-inquilino"}}, USING_MAIL)
        if r.get("ids"):
            encontrado = True
            break
        time.sleep(1)
    ok(encontrado, "el correo llega y se ve por JMAP")
    try:
        imap = imaplib.IMAP4("127.0.0.1", srv.p_imap)
        imap.login("usuario@a.test", PW)
        imap.select("INBOX")
        typ, data = imap.search(None, '(SUBJECT "multi-inquilino")')
        ok(typ == "OK" and data and data[0].split(), "el correo se lee por IMAP")
        imap.logout()
    except imaplib.IMAP4.error as e:
        ok(False, "el correo se lee por IMAP", str(e))

    # Cuota de disco del inquilino B (4000 bytes): un APPEND grande falla con el texto de la documentación.
    grande = ("From: x@b.test\r\nTo: usuario@b.test\r\nSubject: grande\r\n\r\n" + "x" * 6000 + "\r\n").encode()
    try:
        imap = imaplib.IMAP4("127.0.0.1", srv.p_imap)
        imap.login("usuario@b.test", PW)
        try:
            typ, data = imap.append("INBOX", None, None, grande)
            texto = b" ".join(d for d in data if isinstance(d, bytes)).decode("utf-8", "replace")
        except imaplib.IMAP4.error as e:
            typ, texto = "NO", str(e)
        ok(typ != "OK" and "Organization disk quota exceeded" in texto, "IMAP APPEND sobre la cuota de disco de B → OVERQUOTA del inquilino", texto)
        imap.logout()
    except imaplib.IMAP4.error as e:
        ok(False, "IMAP APPEND sobre la cuota de disco de B", str(e))
    _, r = root.get("x:Tenant", [t_a], ["usedDiskQuota"])
    ok(r["list"] and r["list"][0].get("usedDiskQuota", 0) > 0, "usedDiskQuota de A refleja el correo recibido", json.dumps(r)[:200])

    # Correo a B por encima de su cuota de disco: no se entrega y se aplaza con el texto de la documentación.
    msg = ("From: remitente@externo.example\r\nTo: usuario@b.test\r\nSubject: Grande para B\r\n\r\n" + "y" * 6000 + "\r\n")
    with smtplib.SMTP("127.0.0.1", srv.p_smtp, timeout=30) as s:
        s.sendmail("remitente@externo.example", ["usuario@b.test"], msg)
    aplazado = ""
    for _ in range(30):
        _, r = root.query("x:QueuedMessage")
        if r.get("ids"):
            _, r = root.get("x:QueuedMessage", list(r["ids"]))
            aplazado = json.dumps(r)
            if "Organization over quota" in aplazado:
                break
        time.sleep(1)
    ok("Organization over quota" in aplazado, "SMTP a B por encima de su cuota de disco → entrega aplazada «Organization over quota.»", aplazado[:400])
    usr_b = Jmap(base, "usuario@b.test", PW)
    n, r = usr_b.una("Email/query", {"accountId": usr_b.cuenta(), "filter": {"subject": "Grande para B"}}, USING_MAIL)
    ok(n != "error" and not r.get("ids"), "el correo grande no llegó al buzón de B")

    # Las firmas DKIM que crea el servidor para un dominio son de su inquilino.
    dkim_a = []
    for _ in range(30):
        _, r = root.query("x:DkimSignature", {"domainId": dom_a})
        dkim_a = list(ids_de(r))
        if dkim_a:
            break
        time.sleep(1)
    _, r = root.get("x:DkimSignature", dkim_a, ["memberTenantId"])
    ok(dkim_a and all(o.get("memberTenantId") == t_a for o in r.get("list", [])), "las firmas DKIM automáticas de a.test son del inquilino A")
    _, r = adm_a.query("x:DkimSignature")
    ok(set(dkim_a) <= ids_de(r), "el admin de A ve las firmas DKIM de sus dominios")

    print("== logo por inquilino")
    for host, esperado in (("a.test", 200), ("mail.a1.test", 200), ("b.test", 404)):
        req = urllib.request.Request(f"{base}/logo", headers={"Host": host})
        try:
            with urllib.request.urlopen(req, timeout=30) as resp:
                estado, tipo, cuerpo = resp.status, resp.headers.get("Content-Type"), resp.read()
        except urllib.error.HTTPError as e:
            estado, tipo, cuerpo = e.code, None, b""
        if esperado == 200:
            ok(estado == 200 and tipo == "image/png" and cuerpo == PNG, f"GET /logo con Host {host} → logo de A")
        else:
            ok(estado == 404, f"GET /logo con Host {host} → sin logo")

    with open(srv.log, errors="replace") as f:
        ok("URL not allowed" in f.read(), "el logo con URL de red privada de un dominio de inquilino no se descarga (PublicOnly)")

    print("== esquema de la API para el panel")
    estado, cuerpo = root.http("GET", "/api/schema")
    esquema = {}
    if estado == 200:
        import gzip
        try:
            esquema = json.loads(gzip.decompress(cuerpo))
        except OSError:
            esquema = json.loads(cuerpo)
    ok("x:Tenant" in esquema.get("objects", {}) and "x:Enterprise" not in esquema.get("objects", {})
       and "bezentiParentTenantId" in esquema.get("fields", {}).get("x:Tenant", {}).get("properties", {}),
       "/api/schema sirve el esquema bezenti (inquilinos y revendedores, sin licencias)", str(estado))

    print("== borrar inquilinos")
    n, r = root.set("x:Tenant", destroy=[t_a])
    ok((r.get("notDestroyed") or {}).get(t_a, {}).get("type") == "objectIsLinked", "no se borra un inquilino con objetos o hijos")


if __name__ == "__main__":
    main()
