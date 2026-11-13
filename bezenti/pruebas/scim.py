#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 bezenti
#
# SPDX-License-Identifier: AGPL-3.0-only
"""Pruebas de integración de la fase 7: SCIM 2.0 (edición bezenti).

Levanta el servidor real (binario `stalwart` con `sqlite` y `bezenti`) con una base SQLite nueva y lo prueba por
HTTP (`/scim/v2`), JMAP, SMTP e IMAP, solo con la biblioteca estándar de Python. Reutiliza las utilidades de
`multi_inquilino.py` (mismo directorio). Especificación: bezenti/especificaciones/scim.md.

Escenario: el operador crea los inquilinos A y B (ambos con `scimAccess` en su techo; B con `maxAccounts: 2`), sus
dominios (a.test y b.test abiertos a SCIM; a-cerrado.test sin abrir) y una cuenta de aprovisionamiento en cada uno
con su clave API. Después:

- Descubrimiento sin autenticación; 401 sin credencial, con Basic o con un token falso; 403 sin `scimAccess`.
- Prueba de conexión de Entra (filtro con un userName aleatorio → lista vacía).
- Ciclo de vida de un usuario: crear (estilo Entra, Okta y Keycloak) → buscar con filtros → PATCH estilo Entra y
  estilo Okta → PUT → recibe correo por SMTP y lo lee por IMAP → `active: "False"` → no puede entrar (pero sigue
  recibiendo) → reactivar → borrar. ETag / If-Match / If-None-Match.
- Grupos con miembros (dirección derivada, PATCH de Entra y de Okta, grupos anidados no), Bulk con `bulkId`,
  paginación por índice y por cursor, orden, proyección, errores (409, 404, 400, 405, 412, 501).
- Aislamiento entre inquilinos con el token de cada uno, cuotas del inquilino y el token del operador.

Uso: scim.py RUTA_AL_BINARIO_STALWART
"""

import base64
import imaplib
import json
import os
import shutil
import smtplib
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import multi_inquilino as mi  # noqa: E402
from multi_inquilino import ADMIN_PW, PW, USING_MAIL, Jmap, Servidor, ok  # noqa: E402

URN_USER = "urn:ietf:params:scim:schemas:core:2.0:User"
URN_GROUP = "urn:ietf:params:scim:schemas:core:2.0:Group"
URN_ENT = "urn:ietf:params:scim:schemas:extension:enterprise:2.0:User"
URN_PATCH = "urn:ietf:params:scim:api:messages:2.0:PatchOp"
URN_SEARCH = "urn:ietf:params:scim:api:messages:2.0:SearchRequest"
URN_BULK = "urn:ietf:params:scim:api:messages:2.0:BulkRequest"
URN_ERROR = "urn:ietf:params:scim:api:messages:2.0:Error"
SCIM_PERMS = {p: True for p in ("scimAccess", "sysAccountGet", "sysAccountCreate", "sysAccountUpdate", "sysAccountDestroy")}


class Scim:
    def __init__(self, base, token=None, auth=None):
        self.base = base.rstrip("/") + "/scim/v2"
        self.auth = auth if auth is not None else (f"Bearer {token}" if token else None)

    def pide(self, metodo, ruta, cuerpo=None, cabeceras=None, crudo=None):
        datos = crudo if crudo is not None else (None if cuerpo is None else json.dumps(cuerpo).encode())
        req = urllib.request.Request(self.base + ruta, data=datos, method=metodo)
        if self.auth:
            req.add_header("Authorization", self.auth)
        if datos is not None:
            req.add_header("Content-Type", "application/scim+json")
        for k, v in (cabeceras or {}).items():
            req.add_header(k, v)
        try:
            with urllib.request.urlopen(req, timeout=60) as r:
                estado, hdrs, cuerpo_r = r.status, dict(r.headers), r.read()
        except urllib.error.HTTPError as e:
            estado, hdrs, cuerpo_r = e.code, dict(e.headers), e.read()
        try:
            doc = json.loads(cuerpo_r) if cuerpo_r else None
        except ValueError:
            doc = {"_crudo": cuerpo_r[:300].decode("utf-8", "replace")}
        return estado, {k.lower(): v for k, v in hdrs.items()}, doc

    def get(self, ruta, **params):
        q = ("?" + urllib.parse.urlencode(params, quote_via=urllib.parse.quote)) if params else ""
        return self.pide("GET", ruta + q)

    def post(self, ruta, cuerpo, **kw):
        return self.pide("POST", ruta, cuerpo, **kw)

    def put(self, ruta, cuerpo, **kw):
        return self.pide("PUT", ruta, cuerpo, **kw)

    def patch(self, ruta, operaciones, **kw):
        return self.pide("PATCH", ruta, {"schemas": [URN_PATCH], "Operations": operaciones}, **kw)

    def delete(self, ruta, **kw):
        return self.pide("DELETE", ruta, **kw)

    def busca_usuario(self, filtro):
        e, _, d = self.get("/Users", filter=filtro)
        return e, [r["id"] for r in (d or {}).get("Resources", [])]


def es_error(estado, doc, esperado, scim_type=None):
    return (
        estado == esperado
        and isinstance(doc, dict)
        and URN_ERROR in (doc.get("schemas") or [])
        and doc.get("status") == str(esperado)
        and (scim_type is None or doc.get("scimType") == scim_type)
    )


def enviar(srv, para, asunto):
    msg = (f"From: Remitente <remitente@externo.example>\r\nTo: {para}\r\nSubject: {asunto}\r\n"
           f"Message-ID: <{time.time_ns()}@externo.example>\r\n\r\nHola.\r\n")
    try:
        with smtplib.SMTP("127.0.0.1", srv.p_smtp, timeout=30) as s:
            s.ehlo("externo.example")
            s.sendmail("remitente@externo.example", [para], msg)
        return None
    except smtplib.SMTPRecipientsRefused as e:
        return str(e.recipients)
    except smtplib.SMTPResponseException as e:
        return f"{e.smtp_code} {e.smtp_error!r}"


def imap_login(srv, usuario, clave):
    try:
        imap = imaplib.IMAP4("127.0.0.1", srv.p_imap)
        imap.login(usuario, clave)
        return imap
    except imaplib.IMAP4.error:
        return None


def imap_busca(srv, usuario, clave, asunto, segundos=30):
    fin = time.time() + segundos
    while time.time() < fin:
        imap = imap_login(srv, usuario, clave)
        if imap is None:
            return False
        imap.select("INBOX")
        typ, data = imap.search(None, f'(SUBJECT "{asunto}")')
        imap.logout()
        if typ == "OK" and data and data[0].split():
            return True
        time.sleep(1)
    return False


def clave_api(cliente, descripcion="SCIM"):
    """La cuenta crea su propia clave API (modo Inherit) y devuelve el secreto."""
    n, r = cliente.set("x:ApiKey", create={"k": {"description": descripcion, "permissions": {"@type": "Inherit"}}})
    creada = (r.get("created") or {}).get("k")
    assert creada and creada.get("secret"), f"no se pudo crear la clave API: {json.dumps(r)[:400]}"
    return creada["secret"]


def main():
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    srv = Servidor(os.path.abspath(sys.argv[1]))
    error = None
    try:
        mi.configurar(srv)
        pruebas(srv)
    except Exception as e:  # noqa: BLE001 — se informa y se guarda el registro
        error = e
        mi.FALLOS.append(f"excepción: {e!r}")
    finally:
        srv.parar()
        if mi.FALLOS:
            destino = os.path.join(tempfile.gettempdir(), "bezenti-scim.log")
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


def pruebas(srv):
    base = f"http://127.0.0.1:{srv.p_http}"
    rec = Jmap(base, "admin", ADMIN_PW)

    print("== el operador prepara inquilinos, dominios y cuentas de aprovisionamiento")
    dom_op = rec.crear("x:Domain", {"name": "operador.test", "dkimManagement": {"@type": "Manual"}, "allowScimProvisioning": True})
    rec.crear("x:Account", {"@type": "User", "name": "root", "domainId": dom_op, "roles": {"@type": "Admin"},
                            "credentials": {"0": {"@type": "Password", "secret": PW}}})
    root = Jmap(base, "root@operador.test", PW)
    scim_tenant = {"@type": "Merge", "enabledPermissions": {"scimAccess": True}, "disabledPermissions": {}}
    t_a = root.crear("x:Tenant", {"name": "Empresa A", "permissions": scim_tenant})
    t_b = root.crear("x:Tenant", {"name": "Empresa B", "permissions": scim_tenant, "quotas": {"maxAccounts": 2}})
    dom_a = root.crear("x:Domain", {"name": "a.test", "memberTenantId": t_a, "allowScimProvisioning": True, "dkimManagement": {"@type": "Manual"}})
    dom_a2 = root.crear("x:Domain", {"name": "a-cerrado.test", "memberTenantId": t_a, "dkimManagement": {"@type": "Manual"}})
    dom_b = root.crear("x:Domain", {"name": "b.test", "memberTenantId": t_b, "allowScimProvisioning": True, "dkimManagement": {"@type": "Manual"}})
    prov = {"@type": "Merge", "enabledPermissions": SCIM_PERMS, "disabledPermissions": {}}
    scim_a_id = root.crear("x:Account", {"@type": "User", "name": "scim", "domainId": dom_a, "memberTenantId": t_a,
                                         "description": "SCIM (IdP de A)", "roles": {"@type": "User"}, "permissions": prov,
                                         "credentials": {"0": {"@type": "Password", "secret": PW}}})
    root.crear("x:Account", {"@type": "User", "name": "scim", "domainId": dom_b, "memberTenantId": t_b,
                             "roles": {"@type": "User"}, "permissions": prov,
                             "credentials": {"0": {"@type": "Password", "secret": PW}}})
    root.crear("x:Account", {"@type": "User", "name": "normal", "domainId": dom_a2, "memberTenantId": t_a,
                             "credentials": {"0": {"@type": "Password", "secret": PW}}})
    token_a = clave_api(Jmap(base, "scim@a.test", PW))
    token_b = clave_api(Jmap(base, "scim@b.test", PW))
    token_op = clave_api(root)
    token_normal = clave_api(Jmap(base, "normal@a-cerrado.test", PW))
    a = Scim(base, token_a)
    b = Scim(base, token_b)
    op = Scim(base, token_op)
    anon = Scim(base)

    print("== descubrimiento (sin autenticación)")
    e, h, d = anon.get("/ServiceProviderConfig")
    ok(e == 200 and h.get("content-type", "").startswith("application/scim+json"), "GET /ServiceProviderConfig → 200 application/scim+json", f"{e} {h.get('content-type')}")
    ok(d and d["patch"]["supported"] and d["bulk"]["maxOperations"] == 1000 and d["filter"]["maxResults"] == 200
       and not d["changePassword"]["supported"] and d["etag"]["supported"] and d["sort"]["supported"]
       and d["authenticationSchemes"][0]["type"] == "oauthbearertoken", "capacidades anunciadas", json.dumps(d)[:300])
    e, _, d = anon.get("/ServiceProviderConfig", filter='id eq "x"')
    ok(es_error(e, d, 403), "filter en un endpoint de descubrimiento → 403", f"{e} {d}")
    e, _, d = anon.get("/ResourceTypes")
    ok(e == 200 and {r["id"] for r in d["Resources"]} == {"User", "Group"}, "GET /ResourceTypes")
    e, _, d = anon.get("/ResourceTypes/Group")
    ok(e == 200 and d["endpoint"] == "/Groups", "GET /ResourceTypes/Group")
    e, _, d = anon.get("/Schemas")
    ok(e == 200 and {r["id"] for r in d["Resources"]} == {URN_USER, URN_GROUP}, "GET /Schemas")
    e, _, d = anon.get("/Schemas/" + URN_USER)
    nombres = {at["name"] for at in d.get("attributes", [])} if d else set()
    ok(e == 200 and "userName" in nombres and "password" not in nombres and "title" not in nombres,
       "el esquema User publica solo lo que se guarda (sin password)", str(nombres))
    e, _, d = a.get("/Me")
    ok(es_error(e, d, 501), "/Me → 501", f"{e} {d}")
    e, _, d = a.get("/Desconocido")
    ok(es_error(e, d, 404), "ruta desconocida → 404", f"{e} {d}")
    e, _, _ = anon.pide("OPTIONS", "/Users")
    ok(e == 204, "OPTIONS → 204", str(e))

    print("== autenticación y permisos")
    e, h, d = anon.get("/Users")
    ok(es_error(e, d, 401) and "bearer" in h.get("www-authenticate", "").lower(), "sin credencial → 401 con WWW-Authenticate", f"{e} {h.get('www-authenticate')} {d}")
    basic = Scim(base, auth="Basic " + base64.b64encode(f"scim@a.test:{PW}".encode()).decode())
    e, _, d = basic.get("/Users")
    ok(es_error(e, d, 401) and "bearer token" in d.get("detail", ""), "Basic (aunque sea válido) → 401", f"{e} {d}")
    e, _, d = Scim(base, "token-falso").get("/Users")
    ok(es_error(e, d, 401), "token de portador falso → 401", f"{e} {d}")
    e, _, d = Scim(base, token_normal).get("/Users")
    ok(es_error(e, d, 403) and "scimAccess" in d.get("detail", ""), "cuenta sin scimAccess → 403", f"{e} {d}")

    print("== prueba de conexión de Entra")
    e, _, d = a.get("/Users", filter=f'userName eq "{uuid.uuid4()}"')
    ok(e == 200 and d["totalResults"] == 0 and d["Resources"] == [], "filtro con un userName aleatorio → 200 y lista vacía", f"{e} {d}")
    e, _, d = a.get("/Groups", filter='displayName eq "nada"', excludedAttributes="members")
    ok(e == 200 and d["totalResults"] == 0, "lo mismo en /Groups", f"{e} {d}")

    print("== crear usuarios (estilo Entra, Okta y Keycloak)")
    ana_doc = {
        "schemas": [URN_USER, URN_ENT], "externalId": "ext-ana", "userName": "Ana@A.test", "active": True,
        "displayName": "Ana Pérez", "title": "CTO", "preferredLanguage": "es-ES",
        "emails": [{"primary": True, "type": "work", "value": "ana@a.test"}],
        "name": {"givenName": "Ana", "familyName": "Pérez", "formatted": "Ana Pérez"},
        URN_ENT: {"department": "IT", "manager": {"value": "x"}},
        "phoneNumbers": [{"type": "work", "value": "+51 1 000"}],
    }
    e, h, ana = a.post("/Users", ana_doc)
    ok(e == 201 and ana and ana.get("id") and h.get("location", "").endswith("/scim/v2/Users/" + ana["id"]) and h.get("etag", "").startswith("W/"),
       "POST /Users (Entra) → 201 con Location y ETag", f"{e} {h} {ana}")
    ana_id = ana["id"]
    ok(ana["userName"] == "ana@a.test" and ana["displayName"] == "Ana Pérez" and ana["active"] is True
       and ana["externalId"] == "ext-ana" and ana["meta"]["resourceType"] == "User" and ana["meta"].get("created")
       and "title" not in ana and URN_ENT not in ana and ana["locale"] == "es-ES",
       "atributos guardados y los no guardados descartados", json.dumps(ana)[:400])
    _, r = root.get("x:Account", [ana_id], ["name", "domainId", "memberTenantId", "externalId", "description", "roles", "permissions"])
    cuenta = (r.get("list") or [{}])[0]
    ok(cuenta.get("memberTenantId") == t_a and cuenta.get("externalId") == "ext-ana" and cuenta.get("roles", {}).get("@type") == "User"
       and cuenta.get("permissions", {}).get("@type") == "Inherit", "la cuenta queda en el inquilino A, rol User y permisos Inherit", json.dumps(cuenta))
    e, _, d = a.post("/Users", {"schemas": [URN_USER], "userName": "ana@a.test"})
    ok(es_error(e, d, 409, "uniqueness"), "userName repetido → 409 uniqueness", f"{e} {d}")
    e, _, d = a.post("/Users", {"schemas": [URN_USER], "userName": "otra@a.test", "externalId": "ext-ana"})
    ok(es_error(e, d, 409, "uniqueness"), "externalId repetido → 409 uniqueness", f"{e} {d}")

    e, _, luis = a.post("/Users", {
        "schemas": [URN_USER], "userName": "luis@a.test", "name": {"givenName": "Luis", "familyName": "Gómez"},
        "emails": [{"primary": True, "value": "luis@a.test", "type": "work"}], "displayName": "Luis Gómez",
        "locale": "ES_pe", "timezone": "America/Lima", "externalId": "00u1", "groups": [], "password": "Secr3t-Okta!", "active": True,
    })
    ok(e == 201 and luis["locale"] == "es-PE" and luis["preferredLanguage"] == "es-PE" and luis["timezone"] == "America/Lima", "POST /Users (Okta, con password) → 201; locale normalizado", f"{e} {luis}")
    luis_id = luis["id"]
    ok(imap_login(srv, "luis@a.test", "Secr3t-Okta!") is None, "la contraseña que manda Okta se descarta (no sirve para entrar)")
    e, _, eva = a.post("/Users", {"schemas": [URN_USER], "userName": "eva@a.test", "name": {"givenName": "Eva", "familyName": "Ruiz"}})
    ok(e == 201 and eva["displayName"] == "Eva Ruiz", "POST /Users (Keycloak, solo partes del nombre) → displayName unido", f"{e} {eva}")
    eva_id = eva["id"]

    for desc, doc, tipo in (
        ("atributo mal escrito (dispalyName)", {"schemas": [URN_USER], "userName": "x@a.test", "dispalyName": "X"}, "invalidSyntax"),
        ("esquema desconocido", {"schemas": [URN_USER, "urn:ejemplo:raro"], "userName": "x@a.test"}, "invalidSyntax"),
        ("sin schemas", {"userName": "x@a.test"}, "invalidSyntax"),
        ("dominio no abierto a SCIM", {"schemas": [URN_USER], "userName": "x@a-cerrado.test"}, "invalidValue"),
        ("dominio inexistente", {"schemas": [URN_USER], "userName": "x@nadie.test"}, "invalidValue"),
        ("userName sin dominio", {"schemas": [URN_USER], "userName": "solo-nombre"}, "invalidValue"),
        ("alias en un dominio no abierto", {"schemas": [URN_USER], "userName": "x@a.test", "emails": [{"value": "x@a-cerrado.test"}]}, "invalidValue"),
        ("locale desconocido", {"schemas": [URN_USER], "userName": "x@a.test", "locale": "xx-YY"}, "invalidValue"),
        ("zona horaria desconocida", {"schemas": [URN_USER], "userName": "x@a.test", "timezone": "Marte/Olimpo"}, "invalidValue"),
    ):
        e, _, d = a.post("/Users", doc)
        ok(es_error(e, d, 400, tipo), f"POST con {desc} → 400 {tipo}", f"{e} {d}")
    e, _, d = a.pide("POST", "/Users", crudo=b"{no es json")
    ok(es_error(e, d, 400, "invalidSyntax"), "JSON mal formado → 400 invalidSyntax", f"{e} {d}")

    print("== buscar con filtros")
    for filtro, esperado in (
        ('userName eq "ANA@a.test"', {ana_id}),
        ('externalId eq "ext-ana"', {ana_id}),
        ('externalId eq "EXT-ANA"', set()),
        ('emails.value eq "luis@a.test"', {luis_id}),
        ('emails[type eq "work" and value eq "eva@a.test"]', {eva_id}),
        ('displayName co "pérez"', {ana_id}),
        ('userName sw "l" or userName sw "e"', {luis_id, eva_id}),
        ('not (userName sw "scim") and active eq true', {ana_id, luis_id, eva_id}),
        ('userName eq "luis@a.test" and timezone eq "America/Lima"', {luis_id}),
        ('meta.created gt "2020-01-01T00:00:00Z" and externalId pr', {ana_id, luis_id}),
    ):
        e, ids = a.busca_usuario(filtro)
        ok(e == 200 and set(ids) == esperado, f"filtro {filtro}", f"{e} {ids}")
    for filtro in ('userName eq', 'userName zz "x"', 'title eq "CTO"', 'meta.lastModified gt "2020-01-01T00:00:00Z"', '(userName eq "x"'):
        e, _, d = a.get("/Users", filter=filtro)
        ok(es_error(e, d, 400, "invalidFilter"), f"filtro inválido «{filtro}» → 400 invalidFilter", f"{e} {d}")
    e, _, d = a.post("/Users/.search", {"schemas": [URN_SEARCH], "filter": 'userName eq "eva@a.test"', "attributes": ["userName"]})
    ok(e == 200 and d["totalResults"] == 1 and set(d["Resources"][0]) <= {"schemas", "id", "userName", "meta"}, "POST /Users/.search con attributes", f"{e} {d}")

    print("== GET de un recurso, ETag e If-None-Match")
    e, h, d = a.get(f"/Users/{ana_id}")
    etag = h.get("etag")
    ok(e == 200 and etag and d["meta"]["version"] == etag, "GET /Users/{id} → ETag = meta.version", f"{e} {etag}")
    e, _, _ = a.pide("GET", f"/Users/{ana_id}", cabeceras={"If-None-Match": etag})
    ok(e == 304, "If-None-Match con la versión actual → 304", str(e))
    e, _, d = a.get(f"/Users/{ana_id}", excludedAttributes="emails,groups")
    ok(e == 200 and "emails" not in d and d["id"] == ana_id, "excludedAttributes")

    print("== PATCH estilo Entra")
    e, h, d = a.patch(f"/Users/{ana_id}", [
        {"op": "Replace", "path": "displayName", "value": "Ana P."},
        {"op": "Add", "path": "emails", "value": [{"value": "ana.alias@a.test", "type": "work"}]},
        {"op": "Replace", "path": f"{URN_ENT}:department", "value": "Ventas"},
        {"op": "Replace", "path": "title", "value": "CEO"},
        {"op": "Add", "path": "name.givenName", "value": "Ana"},
    ])
    emails = {x["value"] for x in (d or {}).get("emails", [])}
    ok(e == 200 and d["displayName"] == "Ana P." and emails == {"ana@a.test", "ana.alias@a.test"} and h.get("etag") != etag,
       "PATCH (Replace/Add con mayúscula, extensión y title ignorados) → 200", f"{e} {d}")
    e, _, d = a.patch(f"/Users/{ana_id}", [{"op": "Replace", "path": 'emails[type eq "work"].value', "value": "x@a.test"}])
    ok(es_error(e, d, 400) and "more than one 'emails' element" in d.get("detail", ""), "emails[type eq \"work\"].value con alias → 400", f"{e} {d}")
    e, _, d = a.patch(f"/Users/{ana_id}", [{"op": "remove", "path": "emails[primary eq true]"}])
    ok(es_error(e, d, 400) and "userName" in d.get("detail", ""), "quitar la dirección principal → 400 (remite a userName)", f"{e} {d}")
    e, _, d = a.patch(f"/Users/{ana_id}", [{"op": "replace", "path": "id", "value": "otro"}])
    ok(es_error(e, d, 400, "mutability"), "PATCH de id → 400 mutability", f"{e} {d}")
    e, _, d = a.patch(f"/Users/{ana_id}", [{"op": "remove", "path": "userName"}])
    ok(es_error(e, d, 400, "mutability"), "quitar userName (obligatorio) → 400 mutability", f"{e} {d}")
    e, _, d = a.patch(f"/Users/{ana_id}", [{"op": "replace", "path": "nombreRaro", "value": 1}])
    ok(es_error(e, d, 400, "invalidPath"), "PATCH de un atributo desconocido → 400 invalidPath", f"{e} {d}")
    e, _, d = a.pide("PATCH", f"/Users/{ana_id}", {"Operations": [{"op": "replace", "path": "displayName", "value": "x"}]})
    ok(es_error(e, d, 400, "invalidSyntax"), "PATCH sin schemas → 400 invalidSyntax", f"{e} {d}")
    e, _, d = a.patch(f"/Users/{eva_id}", [{"op": "Replace", "path": 'emails[type eq "work"].value', "value": "eva.ruiz@a.test"}])
    ok(e == 200 and d["userName"] == "eva.ruiz@a.test", "emails[type eq \"work\"].value sin alias → renombra la cuenta (como dice la doc)", f"{e} {d}")

    print("== PATCH y PUT estilo Okta")
    e, _, d = a.patch(f"/Users/{luis_id}", [{"op": "replace", "value": {"displayName": "Luis G.", "locale": "es-PE", "emails": [
        {"value": "luis@a.test", "primary": True, "type": "work"}, {"value": "soporte@a.test", "type": "work"}]}}])
    ok(e == 200 and d["displayName"] == "Luis G." and d["locale"] == "es-PE" and len(d["emails"]) == 2, "PATCH replace sin path (Okta)", f"{e} {d}")
    e, h, d = a.get(f"/Users/{luis_id}")
    etag_luis = h.get("etag")
    e, _, d = a.put(f"/Users/{luis_id}", {"schemas": [URN_USER], "id": luis_id, "userName": "luis@a.test", "displayName": "Luis Gómez",
                                          "externalId": "00u1", "active": True, "emails": [{"value": "luis@a.test", "primary": True}],
                                          "meta": {"resourceType": "User"}}, cabeceras={"If-Match": etag_luis})
    ok(e == 200 and len(d["emails"]) == 1 and d["locale"] == "en-US" and "timezone" not in d and d["displayName"] == "Luis Gómez",
       "PUT completo (Okta) reemplaza: alias fuera, locale y zona por defecto", f"{e} {d}")
    e, _, d = a.put(f"/Users/{luis_id}", {"schemas": [URN_USER], "userName": "luis@a.test"}, cabeceras={"If-Match": etag_luis})
    ok(es_error(e, d, 412), "If-Match con una versión vieja → 412", f"{e} {d}")

    print("== correo: un usuario creado por SCIM recibe por SMTP y lee por IMAP")
    n, r = root.set("x:Account", update={ana_id: {"credentials": {"0": {"@type": "Password", "secret": PW}}}})
    ok(ana_id in (r.get("updated") or {}), "el administrador le pone contraseña (SCIM no transporta credenciales)", json.dumps(r)[:300])
    ok(enviar(srv, "ana@a.test", "scim-principal") is None and enviar(srv, "ana.alias@a.test", "scim-alias") is None, "SMTP acepta la principal y el alias")
    ok(imap_busca(srv, "ana@a.test", PW, "scim-principal") and imap_busca(srv, "ana@a.test", PW, "scim-alias"), "IMAP lee los dos mensajes")

    print("== suspender (active false) y reactivar")
    e, _, d = a.patch(f"/Users/{ana_id}", [{"op": "Replace", "path": "active", "value": "False"}])
    ok(e == 200 and d["active"] is False, "PATCH active \"False\" (Entra) → active false", f"{e} {d}")
    ok(imap_login(srv, "ana@a.test", PW) is None, "suspendida: IMAP no la deja entrar")
    ok(enviar(srv, "ana@a.test", "scim-suspendida") is None, "suspendida: sigue recibiendo correo")
    e, ids = a.busca_usuario("active eq false")
    ok(e == 200 and ids == [ana_id], "filtro active eq false", str(ids))
    e, _, d = a.patch(f"/Users/{ana_id}", [{"op": "replace", "value": {"active": True}}])
    ok(e == 200 and d["active"] is True, "PATCH active true (Okta) → reactivada", f"{e} {d}")
    _, r = root.get("x:Account", [ana_id], ["permissions"])
    ok(r["list"][0]["permissions"] == {"@type": "Inherit"}, "al reactivar vuelve exactamente a Inherit", json.dumps(r["list"][0]))
    ok(imap_busca(srv, "ana@a.test", PW, "scim-suspendida"), "reactivada: entra y ve el correo que llegó suspendida")
    e, _, d = a.patch(f"/Users/{scim_a_id}", [{"op": "replace", "path": "active", "value": False}])
    ok(es_error(e, d, 403) and "own service principal" in d.get("detail", ""), "desactivarse a sí mismo → 403", f"{e} {d}")
    e, _, d = a.delete(f"/Users/{scim_a_id}")
    ok(es_error(e, d, 403), "borrarse a sí mismo → 403", f"{e} {d}")

    print("== grupos")
    e, h, g = a.post("/Groups", {"schemas": [URN_GROUP], "displayName": "Sales EMEA", "externalId": "g-1",
                                 "members": [{"value": luis_id, "type": "User"}]})
    ok(e == 201 and g["displayName"] == "Sales EMEA" and [m["value"] for m in g.get("members", [])] == [luis_id],
       "POST /Groups con miembros → 201", f"{e} {g}")
    g_id = g["id"]
    _, r = root.get("x:Account", [g_id], ["name", "domainId", "memberTenantId"])
    cg = r["list"][0]
    ok(cg["name"] == "sales-emea" and cg["domainId"] == dom_a and cg["memberTenantId"] == t_a, "dirección derivada sales-emea@a.test en el inquilino A", json.dumps(cg))
    e, _, d = a.get(f"/Users/{luis_id}")
    ok([x["value"] for x in d.get("groups", [])] == [g_id], "el usuario muestra el grupo en groups", json.dumps(d.get("groups")))
    e, _, d = a.post("/Groups", {"schemas": [URN_GROUP], "displayName": "sales emea"})
    ok(es_error(e, d, 409, "uniqueness"), "displayName de grupo repetido → 409 uniqueness", f"{e} {d}")
    e, _, g2 = a.post("/Groups", {"schemas": [URN_GROUP], "displayName": "Sales  EMEA!!"})
    _, r = root.get("x:Account", [g2["id"]], ["name"]) if e == 201 else (None, {"list": [{}]})
    ok(e == 201 and r["list"][0].get("name") == "sales-emea-2", "dirección ocupada → sales-emea-2", f"{e} {r}")
    e, _, d = a.post("/Groups", {"schemas": [URN_GROUP], "displayName": "Anidado", "members": [{"value": g_id}]})
    ok(es_error(e, d, 400, "invalidValue"), "un grupo como miembro → 400 (no hay grupos anidados)", f"{e} {d}")
    e, _, d = a.post("/Groups", {"schemas": [URN_GROUP], "displayName": "Fantasma", "members": [{"value": "zzzzzz"}]})
    ok(es_error(e, d, 400, "invalidValue"), "miembro inexistente → 400", f"{e} {d}")
    e, _, d = a.patch(f"/Groups/{g_id}", [{"op": "Add", "path": "members", "value": [{"value": eva_id}, {"value": ana_id}]},
                                         {"op": "Remove", "path": "members", "value": [{"value": luis_id}]}])
    ok(e == 200 and {m["value"] for m in d.get("members", [])} == {eva_id, ana_id}, "PATCH de miembros estilo Entra", f"{e} {d}")
    e, _, d = a.patch(f"/Groups/{g_id}", [{"op": "remove", "path": f'members[value eq "{eva_id}"]'},
                                         {"op": "replace", "value": {"id": g_id, "displayName": "Ventas EMEA"}}])
    ok(e == 200 and [m["value"] for m in d.get("members", [])] == [ana_id] and d["displayName"] == "Ventas EMEA",
       "PATCH estilo Okta (filtro de valor y replace con id)", f"{e} {d}")
    e, _, d = a.get("/Groups", filter='displayName eq "ventas emea"', excludedAttributes="members")
    ok(e == 200 and [r["id"] for r in d["Resources"]] == [g_id] and "members" not in d["Resources"][0], "buscar grupo por displayName sin miembros", f"{e} {d}")
    e, _, d = a.get("/Groups", filter=f'members.value eq "{ana_id}"')
    ok(e == 200 and [r["id"] for r in d["Resources"]] == [g_id], "buscar grupos por miembro", f"{e} {d}")
    e, _, d = a.put(f"/Groups/{g_id}", {"schemas": [URN_GROUP], "displayName": "Ventas EMEA", "members": [{"value": luis_id}, {"value": eva_id}]})
    ok(e == 200 and {m["value"] for m in d.get("members", [])} == {luis_id, eva_id}, "PUT de grupo reemplaza los miembros", f"{e} {d}")
    e, _, _ = a.delete(f"/Groups/{g2['id']}")
    e2, _, d = a.delete(f"/Groups/{g_id}")
    ok(e == 204 and e2 == 204, "DELETE de grupos → 204", f"{e} {e2} {d}")
    e, _, d = a.get(f"/Users/{luis_id}")
    ok("groups" not in d, "el usuario ya no está en el grupo borrado", json.dumps(d.get("groups")))

    print("== Bulk")
    ops = [{"method": "POST", "path": "/Users", "bulkId": f"u{i}", "data": {"schemas": [URN_USER], "userName": f"bulk{i}@a.test"}} for i in range(5)]
    ops.append({"method": "POST", "path": "/Groups", "bulkId": "g", "data": {"schemas": [URN_GROUP], "displayName": "Bulk",
                "members": [{"value": "bulkId:u0"}, {"value": "bulkId:u1"}]}})
    ops.append({"method": "PATCH", "path": "/Users/bulkId:u2", "data": {"schemas": [URN_PATCH], "Operations": [{"op": "replace", "path": "displayName", "value": "Bulk Dos"}]}})
    ops.append({"method": "DELETE", "path": "/Users/bulkId:nadie"})
    e, _, d = a.post("/Bulk", {"schemas": [URN_BULK], "Operations": ops})
    estados = [o["status"] for o in (d or {}).get("Operations", [])]
    ok(e == 200 and estados == ["201"] * 6 + ["200", "409"], "Bulk con bulkId (usuarios, grupo con miembros, PATCH, referencia rota)", f"{e} {estados} {json.dumps(d)[:600]}")
    bulk_ids = {o["bulkId"]: o["location"].rsplit("/", 1)[1] for o in d["Operations"] if o.get("bulkId") and o.get("location")}
    e, _, gb = a.get(f"/Groups/{bulk_ids.get('g')}")
    ok(e == 200 and {m["value"] for m in gb.get("members", [])} == {bulk_ids["u0"], bulk_ids["u1"]}, "el grupo del Bulk tiene a u0 y u1", f"{e} {gb}")
    e, _, d = a.post("/Bulk", {"schemas": [URN_BULK], "failOnErrors": 1, "Operations": [
        {"method": "POST", "path": "/Users", "bulkId": "x", "data": {"schemas": [URN_USER], "userName": "bulk0@a.test"}},
        {"method": "POST", "path": "/Users", "bulkId": "y", "data": {"schemas": [URN_USER], "userName": "nunca@a.test"}}]})
    ok(e == 200 and [o["status"] for o in d["Operations"]] == ["409"], "failOnErrors=1 se detiene tras el primer error", f"{e} {d}")

    print("== paginación y orden")
    e, _, d = a.get("/Users", count=2, startIndex=1, sortBy="userName")
    total = d["totalResults"]
    ok(e == 200 and d["itemsPerPage"] == 2 and d["startIndex"] == 1 and total >= 9, "página 1 (count=2)", f"{e} {d}")
    vistos = []
    for inicio in range(1, total + 1, 2):
        _, _, d = a.get("/Users", count=2, startIndex=inicio, sortBy="userName")
        vistos += [r["userName"] for r in d["Resources"]]
    ok(len(vistos) == total == len(set(vistos)) and vistos == sorted(vistos), "recorrer por índice: todos, sin repetir y en orden", str(vistos))
    _, _, d = a.get("/Users", count=3, sortBy="userName", sortOrder="descending")
    ok([r["userName"] for r in d["Resources"]] == sorted(vistos, reverse=True)[:3], "sortOrder=descending")
    _, _, d = a.get("/Users", count=0)
    ok(d["totalResults"] == total and d["Resources"] == [], "count=0 solo cuenta")
    por_cursor, cursor, vueltas = [], "", 0
    while cursor is not None and vueltas < 20:
        e, _, d = a.get("/Users", count=4, cursor=cursor)
        por_cursor += [r["id"] for r in d.get("Resources", [])]
        cursor = d.get("nextCursor")
        vueltas += 1
    ok(len(por_cursor) == total == len(set(por_cursor)), "recorrer por cursor (RFC 9865)", f"{len(por_cursor)} de {total}")
    e, _, d = a.get("/Users", count=4, cursor="")
    e, _, d2 = a.get("/Users", count=4, cursor=d.get("nextCursor", ""), filter='userName sw "b"')
    ok(es_error(e, d2, 400, "invalidCursor"), "cursor de otra consulta → 400 invalidCursor", f"{d2}")
    e, _, d = a.get("/Users", cursor="", startIndex=2)
    ok(es_error(e, d, 400, "invalidValue"), "cursor y startIndex a la vez → 400", f"{e} {d}")
    e, _, d = a.get("/Users", sortBy="phoneNumbers")
    ok(es_error(e, d, 400, "invalidValue"), "sortBy no admitido → 400", f"{e} {d}")

    print("== errores HTTP")
    e, _, d = a.get("/Users/zzzzzz")
    ok(es_error(e, d, 404), "GET de un id inexistente → 404", f"{e} {d}")
    e, _, d = a.put("/Users/zzzzzz", {"schemas": [URN_USER], "userName": "z@a.test"})
    ok(es_error(e, d, 404), "PUT de un id inexistente → 404", f"{e} {d}")
    e, h, d = a.delete("/Users")
    ok(es_error(e, d, 405) and "POST" in h.get("allow", ""), "DELETE de la colección → 405 con Allow", f"{e} {h.get('allow')} {d}")
    e, _, d = a.get(f"/Groups/{ana_id}")
    ok(es_error(e, d, 404), "un usuario pedido como grupo → 404", f"{e} {d}")

    print("== aislamiento entre inquilinos")
    e, _, d = b.get("/Users")
    ids_b = {r["id"] for r in d["Resources"]}
    ok(e == 200 and not ids_b & {ana_id, luis_id, eva_id, scim_a_id}, "B no ve usuarios de A al listar", str(ids_b))
    e, ids = b.busca_usuario('userName eq "ana@a.test"')
    ok(e == 200 and ids == [], "B no encuentra a ana@a.test por filtro")
    e, ids = b.busca_usuario('externalId eq "ext-ana"')
    ok(e == 200 and ids == [], "B no encuentra el externalId de A")
    for metodo, args in (("GET", ()), ("PUT", ({"schemas": [URN_USER], "userName": "luis@a.test"},)),
                         ("PATCH", ([{"op": "replace", "path": "displayName", "value": "hackeado"}],)), ("DELETE", ())):
        fn = {"GET": b.get, "PUT": b.put, "PATCH": b.patch, "DELETE": b.delete}[metodo]
        e, _, d = fn(f"/Users/{luis_id}", *args)
        ok(es_error(e, d, 404), f"B: {metodo} de un usuario de A → 404 (no 403)", f"{e} {d}")
    _, _, d = a.get(f"/Users/{luis_id}")
    ok(d["displayName"] == "Luis Gómez", "el usuario de A sigue intacto")
    e, _, d = b.post("/Users", {"schemas": [URN_USER], "userName": "intruso@a.test"})
    ok(es_error(e, d, 400, "invalidValue"), "B no puede crear usuarios en un dominio de A", f"{e} {d}")
    e, _, d = b.post("/Users", {"schemas": [URN_USER], "userName": "beto@b.test", "emails": [{"value": "beto@a.test"}]})
    ok(es_error(e, d, 400, "invalidValue"), "B no puede poner alias en un dominio de A", f"{e} {d}")
    e, _, d = b.post("/Groups", {"schemas": [URN_GROUP], "displayName": "Robo", "members": [{"value": luis_id}]})
    ok(es_error(e, d, 400, "invalidValue"), "B no puede meter usuarios de A en sus grupos", f"{e} {d}")
    e, _, beto = b.post("/Users", {"schemas": [URN_USER], "userName": "beto@b.test", "externalId": "ext-ana"})
    ok(e == 201 and beto["externalId"] == "ext-ana", "el externalId es único por ámbito: B puede usar uno que ya usa A", f"{e} {beto}")
    e, _, d = b.post("/Users", {"schemas": [URN_USER], "userName": "carla@b.test"})
    ok(es_error(e, d, 403), "cuota del inquilino B (maxAccounts 2) → 403", f"{e} {d}")
    e, _, d = b.post("/.search", {"schemas": [URN_SEARCH]})
    ok(e == 200 and {r["id"] for r in d["Resources"]} & {ana_id, luis_id} == set(), "/.search de B no ve nada de A")

    print("== el operador")
    e, _, d = op.get("/Users", filter='userName eq "luis@a.test" or userName eq "beto@b.test"')
    ok(e == 200 and {r["userName"] for r in d["Resources"]} == {"luis@a.test", "beto@b.test"}, "el operador ve los usuarios de A y de B", f"{e} {d}")
    e, _, d = op.post("/.search", {"schemas": [URN_SEARCH], "filter": 'displayName eq "Bulk"'})
    ok(e == 200 and d["totalResults"] == 1 and d["Resources"][0]["schemas"] == [URN_GROUP], "/.search en la raíz junta usuarios y grupos", f"{e} {d}")
    e, _, d = op.post("/Users", {"schemas": [URN_USER], "userName": "zoe@a.test"})
    _, r = root.get("x:Account", [d.get("id")], ["memberTenantId"]) if e == 201 else (None, {"list": [{}]})
    ok(e == 201 and r["list"][0].get("memberTenantId") == t_a, "el operador crea en a.test y la cuenta queda en el inquilino A", f"{e} {d} {r}")
    e, _, d = op.post("/Users", {"schemas": [URN_USER], "userName": "carla@b.test"})
    ok(es_error(e, d, 403), "la cuota de B también vale para el operador", f"{e} {d}")

    print("== borrar")
    e, _, d = a.delete(f"/Users/{ana_id}")
    ok(e == 204, "DELETE /Users/{id} → 204", f"{e} {d}")
    e, _, d = a.get(f"/Users/{ana_id}")
    ok(es_error(e, d, 404), "después, GET → 404")
    ok(imap_login(srv, "ana@a.test", PW) is None, "la cuenta borrada ya no entra por IMAP")
    ok(enviar(srv, "ana@a.test", "despues") is not None, "y SMTP ya no acepta su dirección")
    _, r = rec.query("x:Account")


if __name__ == "__main__":
    main()
