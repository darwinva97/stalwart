# SPDX-FileCopyrightText: 2026 bezenti
#
# SPDX-License-Identifier: AGPL-3.0-only
"""Modelo común: esquema + perfil + modo de generación + asignación de identificadores de pago."""

import os

from reglas import Reglas
from comun import cargar_json, guardar_json, nombre_rust, propiedad, serpiente, variante

# Enums del esquema que NO se generan en el registro porque se definen en otros crates.
ENUMS_EXTERNOS = {
    "EventType": "trc::EventType",
    "MetricType": "trc::MetricType",
    "Key": "trc::Key",
    "IncludeInAvailability": None,
}

# Lo que nunca se genera en la edición bezenti: el objeto de licencias y todo lo que solo existe para él.
OBJETOS_EXCLUIDOS = {"x:Enterprise"}
PREFIJOS_PERMISO_EXCLUIDOS = ("sysEnterprise",)


def tipo_rust(t, interior=False):
    """Tipo Rust de un tipo del esquema."""
    k = t["type"]
    if k == "string":
        r = {"ipAddress": "IpAddr", "ipNetwork": "IpAddrOrMask"}.get(t.get("format"), "String")
    elif k == "number":
        r = {"duration": "Duration", "float": "Float", "integer": "i64"}.get(t.get("format"), "u64")
    elif k == "boolean":
        r = "bool"
    elif k == "utcDateTime":
        r = "UTCDateTime"
    elif k == "blobId":
        r = "BlobId"
    elif k == "enum":
        r = ENUMS_EXTERNOS.get(t["enumName"]) or t["enumName"]
    elif k == "object":
        r = nombre_rust(t["objectName"])
    elif k == "objectId":
        r = "Id"
    elif k == "objectList":
        r = f"List<{nombre_rust(t['objectName'])}>"
    elif k == "set":
        r = f"Map<{tipo_rust(t['class'], True)}>"
    elif k == "map":
        r = f"VecMap<{tipo_rust(t['keyClass'], True)}, {tipo_rust(t['valueClass'], True)}>"
    else:
        raise ValueError(f"tipo desconocido: {t}")
    if t.get("nullable") and not interior:
        r = f"Option<{r}>"
    return r


class Asignador:
    """Identificadores numéricos persistidos para lo de pago (que no existe en el código limpio).

    Se guardan en `asignaciones.json` para que no cambien entre regeneraciones: un identificador ya asignado
    no se reasigna nunca. Los nuevos ocupan primero los huecos de la numeración del código limpio (números que
    el código libre no usa y que, por tanto, nunca reutilizará para otra cosa) y, si no hay, van al final.
    """

    def __init__(self, ruta, guardar=True):
        self.ruta = ruta if guardar else None
        self.datos = cargar_json(ruta) if ruta and os.path.exists(ruta) else {}
        self.cambiado = False

    def asignar(self, clave, ocupados, limite):
        """Devuelve el id de `clave`. `ocupados`: ids usados por el código libre; `limite`: tamaño del
        rango del código libre (los huecos son los números < limite no ocupados)."""
        if clave in self.datos:
            return self.datos[clave]
        usados = set(ocupados)
        prefijo = clave.rsplit(":", 1)[0] + ":"
        for k, v in self.datos.items():
            if k.startswith(prefijo) and k.rsplit(":", 1)[0] + ":" == prefijo:
                usados.add(v)
        i = 0
        while i in usados:
            i += 1
        self.datos[clave] = i
        self.cambiado = True
        return i

    def guardar(self):
        if self.cambiado and self.ruta:
            guardar_json(self.ruta, dict(sorted(self.datos.items())))


class Modelo(Reglas):
    def __init__(self, esquema, perfil, modo, asignador, decisiones=None):
        self.decisiones = decisiones or {}
        assert modo in ("fidelidad", "bezenti")
        self.e = esquema
        self.p = perfil
        self.modo = modo
        self.bezenti = modo == "bezenti"
        self.asig = asignador
        # En modo fidelidad se reproduce el código limpio tal cual (con su `not(feature = "enterprise")`).
        self.feature = "bezenti" if self.bezenti else "enterprise"
        self.cfg = f'#[cfg(feature = "{self.feature}")]'
        self.cfg_no = f'#[cfg(not(feature = "{self.feature}"))]'

    # ----------------------------------------------------------------------------------------- objetos

    def incluido(self, esquema):
        """¿Se genera este esquema (struct/enum Rust)?"""
        if not esquema.startswith("x:") or esquema in OBJETOS_EXCLUIDOS:
            return False
        return True

    def objetos(self):
        """Tipos de objeto (ObjectType): [(nombre_esquema, Rust, id, de_pago)] por nombre Rust."""
        po = self.p["ObjectType"]
        libres = set(po["ids"].values())
        res = []
        for k, v in self.e["objects"].items():
            if v["type"] == "view" or not self.incluido(k):
                continue
            r = nombre_rust(k)
            if r in po["ids"]:
                res.append((k, r, po["ids"][r], False))
            else:
                res.append((k, r, self.asig.asignar(f"objeto:{r}", libres, po["count"]), True))
        return sorted(res, key=lambda x: x[1])

    def propiedades(self):
        """Variantes de `Property`: [(json, Rust, id, de_pago)]; "@type" primero y el resto por nombre Rust."""
        pp = self.p["Property"]
        libres = set(pp["ids"].values())
        nombres = {}
        for k, v in self.e["fields"].items():
            if not self.incluido(k):
                continue
            for prop in v["properties"]:
                nombres.setdefault(propiedad(prop), prop)
        for r, j in pp["json"].items():
            nombres.setdefault(r, j)
        res = []
        for r, j in nombres.items():
            if r in pp["ids"]:
                res.append((j, r, pp["ids"][r], False))
            else:
                res.append((j, r, self.asig.asignar(f"propiedad:{r}", libres, pp["count"]), True))
        return sorted(res, key=lambda x: (x[1] != "Type", x[1]))

    def info_objeto(self, r):
        return self.p["objetos"].get(r) or self.pago_objeto(r)

    def pago_objeto(self, r):
        """Indicadores de un objeto de pago: se deducen del esquema con las reglas que cumplen todos los objetos
        libres (ver especificaciones/modelo.md) más las decisiones de `pago.json`."""
        k = "x:" + r
        o = self.e["objects"][k]
        perms = {v["name"] for v in self.e["enums"]["Permission"]}
        pre = o["permissionPrefix"]
        unico = o["type"] == "singleton"
        props = [set(self.e["fields"].get(s, {}).get("properties", {})) for s in self.esquemas_de(k)]
        flags = []
        if unico:
            flags.append("OBJ_SINGLETON")
        if any("memberTenantId" in p for p in props):
            flags.append("OBJ_FILTER_TENANT")
        if props and all("accountId" in p for p in props) and not unico:
            flags.append("OBJ_FILTER_ACCOUNT")
        extra = self.decisiones.get("objetos", {}).get(r, {})
        flags += [f for f in extra.get("flags_extra", []) if f not in flags]
        if not unico:
            for accion, f in (("Create", "OBJ_NO_CREATE"), ("Update", "OBJ_NO_UPDATE"),
                              ("Destroy", "OBJ_NO_DESTROY"), ("Query", "OBJ_NO_QUERY")):
                if pre + accion not in perms:
                    flags.append(f)
        if pre + "Get" not in perms:
            flags.append("OBJ_NO_GET")
        orden = ["OBJ_SINGLETON", "OBJ_FILTER_TENANT", "OBJ_FILTER_ACCOUNT", "OBJ_SEQ_ID", "OBJ_NOT_STORED",
                 "OBJ_NO_GET", "OBJ_NO_CREATE", "OBJ_NO_UPDATE", "OBJ_NO_DESTROY", "OBJ_NO_QUERY"]
        return {"flags": sorted(flags, key=orden.index), "version": 0,
                "indices": extra.get("indices", [])}

    def no_almacenado(self, r):
        return "OBJ_NOT_STORED" in self.info_objeto(r)["flags"]

    def permiso(self, esquema, accion):
        """Permiso de una acción sobre un objeto, o None. Los singletons se crean y destruyen con Update."""
        obj = self.e["objects"][esquema]
        if obj["type"] == "singleton" and accion in ("Create", "Destroy"):
            accion = "Update"
        nombre = obj["permissionPrefix"] + accion
        if nombre in self.permisos_conocidos():
            return variante(nombre)
        return None

    def permisos_conocidos(self):
        if not hasattr(self, "_permisos"):
            self._permisos = {v[0] for v in self.variantes("Permission")}
        return self._permisos

    def variante_pago(self, esquema, v):
        """¿Es de pago la variante `v` del esquema múltiple `esquema`?"""
        enum = self.enum_tipo(esquema)
        for nombre, rv, i, p in self.variantes(enum):
            if nombre == v["name"]:
                return p
        return True

    def campo_cuenta(self, esquema):
        """(campo, nulable) si el struct tiene `accountId`."""
        f = self.e["fields"].get(esquema)
        if not f or "accountId" not in f["properties"]:
            return None
        return ("account_id", bool(f["properties"]["accountId"]["type"].get("nullable")))

    def esquemas_de(self, esquema):
        """Esquemas de struct de un objeto (uno si es simple; los de sus variantes si es múltiple)."""
        s = self.e["schemas"][esquema]
        if s["type"] == "single":
            return [s["schemaName"]]
        return [v["schemaName"] for v in s["variants"] if "schemaName" in v]

    def tiene_expresiones(self, esquema):
        for s in self.esquemas_de(esquema):
            for prop in self.e["fields"].get(s, {}).get("properties", {}).values():
                t = prop["type"]
                if t["type"] == "object" and t.get("objectName") == "x:Expression":
                    return True
        return False

    def count_de(self, clave):
        return lambda _enum, vs: max([self.p[clave]["count"]] + [v[2] + 1 for v in vs if v[2] is not None])

    # ------------------------------------------------------------------------------------------- enums

    def enum_tipo(self, esquema):
        """Enum con los tipos (variantes) de un esquema múltiple: `{Nombre}Type`. Si ese nombre es el de un enum
        externo (x:Metric → trc::MetricType), se sintetiza `{Nombre}VariantType` a partir de las variantes."""
        r = nombre_rust(esquema) + "Type"
        if r in ENUMS_EXTERNOS:
            r = nombre_rust(esquema) + "VariantType"
        return r

    def enums_sintetizados(self):
        if not hasattr(self, "_sint"):
            self._sint = {}
            for k, s in self.e["schemas"].items():
                if s["type"] == "multiple" and self.incluido(k):
                    et = self.enum_tipo(k)
                    if et not in self.e["enums"]:
                        self._sint[et] = [{"name": v["name"]} for v in s["variants"]]
        return self._sint

    def enums(self):
        """Lista ordenada de enums a generar: [(nombre, de_pago)]."""
        res = []
        todos = dict(self.e["enums"])
        todos.update(self.enums_sintetizados())
        for nombre in sorted(todos):
            if nombre in ENUMS_EXTERNOS:
                continue
            res.append((nombre, nombre not in self.p["enums"]))
        return res

    def variantes(self, enum):
        """[(nombre_json, Variante, id, de_pago)] en el orden del esquema."""
        pe = self.p["enums"].get(enum)
        res = []
        libres = set(pe["ids"].values()) if pe else set()
        limite = pe["count"] if pe else 0
        for v in self.e["enums"].get(enum) or self.enums_sintetizados()[enum]:
            nombre = v["name"]
            if enum == "Permission" and nombre.startswith(PREFIJOS_PERMISO_EXCLUIDOS):
                if not pe or variante(nombre) not in pe["ids"]:
                    continue
            rv = variante(nombre)
            if pe and rv in pe["ids"]:
                res.append((nombre, rv, pe["ids"][rv], False))
            else:
                i = self.asig.asignar(f"enum:{enum}:{rv}", libres, limite)
                res.append((nombre, rv, i, True))
        return res

    def count_enum(self, enum, vs):
        pe = self.p["enums"].get(enum)
        c = pe["count"] if pe else 0
        return max([c] + [i + 1 for _, _, i, _ in vs if i is not None])

    # ----------------------------------------------------------------------------------------- structs

    def esquemas_struct(self):
        """[(nombre_esquema, Rust, de_pago)] de todos los structs/enums de structs.rs, por nombre Rust."""
        res = []
        for k in self.e["schemas"]:
            if not self.incluido(k):
                continue
            r = nombre_rust(k)
            pago = r not in self.p["structs"] and r not in self.p["multiples"]
            res.append((k, r, pago))
        return sorted(res, key=lambda x: x[1])

    def es_multiple(self, esquema):
        return self.e["schemas"][esquema]["type"] == "multiple"

    def variantes_multiple(self, esquema):
        """[(Variante, Rust del struct o None, id_pickle, de_pago)] de un esquema múltiple."""
        r = nombre_rust(esquema)
        pm = self.p["multiples"].get(r)
        res = []
        et = self.enum_tipo(esquema)
        ids_enum = {rv: (i, p) for _, rv, i, p in self.variantes(et)}
        for v in self.e["schemas"][esquema]["variants"]:
            nombre = v["name"]
            sub = nombre_rust(v["schemaName"]) if "schemaName" in v else None
            pago = pm is None or nombre not in pm["variantes"]
            if pm and "pickle" in pm and nombre in pm["pickle"]:
                i = pm["pickle"][nombre]
            else:
                i = ids_enum.get(nombre, (None,))[0]
            res.append((nombre, sub, i, pago))
        return res

    def campos(self, esquema):
        """Campos almacenados de un struct, en orden: [dict(json, campo, tipo, idx, bit, pago, prop)]."""
        r = nombre_rust(esquema)
        props = self.e["fields"][esquema]["properties"]
        ps = self.p["structs"].get(r)
        res = []
        if ps:
            libres = {c["json"] for c in ps["campos"]}
            for c in ps["campos"]:
                d = dict(c)
                d.setdefault("tipo", tipo_rust(props[c["json"]]["type"]))
                d["pago"] = False
                d["prop"] = props[c["json"]]
                res.append(d)
            pagos = [j for j, d in props.items() if d.get("enterprise") and j not in libres]
            ocupados = {c["idx"] for c in ps["campos"] if "idx" in c}
            # Los huecos con relleno (bloque vacío en el código libre) son de campos de pago obligatorios.
            rellenos = list(ps.get("rellenos", []))
            for j in pagos:
                t = tipo_rust(props[j]["type"])
                if "nfields" in ps and not usa_bit(t) and rellenos:
                    clave = f"campo:{r}:{j}"
                    if clave not in self.asig.datos:
                        self.asig.datos[clave] = rellenos.pop(0)
                        self.asig.cambiado = True
            ocupados |= set(ps.get("rellenos", []))
            bits = {c["bit"] for c in ps["campos"] if "bit" in c}
            for j in pagos:
                d = {"json": j, "campo": serpiente(j), "tipo": tipo_rust(props[j]["type"]), "pago": True,
                     "prop": props[j]}
                if "nfields" in ps:
                    d["idx"] = self.asig.asignar(f"campo:{r}:{j}", ocupados, ps["nfields"])
                    if usa_bit(d["tipo"]):
                        d["bit"] = self.asig.asignar(f"bit:{r}:{j}", bits, ps["nflags"])
                res.append(d)
            # Los campos de pago van junto al campo libre que les precede en el formato binario.
            libres_l = [d for d in res if not d["pago"]]
            for d in sorted([d for d in res if d["pago"]], key=lambda d: d.get("idx", 1 << 30)):
                pos = len(libres_l)
                if "idx" in d:
                    antes = [n for n, x in enumerate(libres_l) if x.get("idx", -1) < d["idx"]]
                    if antes:
                        pos = max(antes, key=lambda n: libres_l[n]["idx"]) + 1
                    else:
                        pos = 0
                libres_l.insert(pos, d)
            res = libres_l
        else:
            orden = self.orden_formulario(esquema)
            n_bit = 0
            for n, j in enumerate(orden):
                d = {"json": j, "campo": serpiente(j), "tipo": tipo_rust(props[j]["type"]), "pago": True,
                     "prop": props[j], "idx": n}
                if usa_bit(d["tipo"]):
                    d["bit"] = n_bit
                    n_bit += 1
                res.append(d)
        return res

    def orden_formulario(self, esquema):
        """Orden de campos de un struct nuevo: el del formulario del esquema y después el resto por nombre."""
        props = self.e["fields"][esquema]["properties"]
        orden = []
        for s in self.e["forms"].get(esquema, {}).get("sections", []):
            for f in s.get("fields", []):
                if f["name"] in props and f["name"] not in orden:
                    orden.append(f["name"])
        return orden + [j for j in props if j not in orden]

    def nfields(self, esquema, campos):
        ps = self.p["structs"].get(nombre_rust(esquema))
        n = ps["nfields"] if ps and "nfields" in ps else 0
        return max([n] + [c["idx"] + 1 for c in campos if "idx" in c])

    def nflags(self, esquema, campos):
        ps = self.p["structs"].get(nombre_rust(esquema))
        n = ps["nflags"] if ps and "nflags" in ps else 0
        return max([n] + [c["bit"] + 1 for c in campos if "bit" in c])


def usa_bit(tipo):
    return tipo == "bool" or tipo.startswith("Option<")
