# SPDX-FileCopyrightText: 2026 bezenti
#
# SPDX-License-Identifier: AGPL-3.0-only
"""Reglas que traducen el esquema a código: tipos, valores por defecto, validación, índices y parches.

Cada regla está deducida del código AGPL generado (la prueba de fidelidad comprueba que las reglas reproducen
ese código) y se aplica igual a lo de pago, que no tiene código de referencia.
"""

from comun import cadena_rust, nombre_rust, propiedad, serpiente


def interior(tipo):
    return tipo[7:-1] if tipo.startswith("Option<") else tipo


def opcional(tipo):
    return tipo.startswith("Option<")


def coleccion(tipo):
    t = interior(tipo)
    for pre in ("Map<", "List<", "VecMap<"):
        if t.startswith(pre):
            return pre[:-1]
    return None


def flotante(v):
    s = repr(float(v))
    return s


class Reglas:
    """Mezcla para `Modelo`."""

    # --------------------------------------------------------------------------------------- objetos

    def es_objeto(self, r):
        return any(x[1] == r for x in self.objetos())

    def esquema_de_tipo(self, nombre):
        """Rust → nombre de esquema."""
        return "x:" + nombre

    def grafo(self):
        """Esquema → esquemas de struct que usa (campos y variantes)."""
        if hasattr(self, "_grafo"):
            return self._grafo
        g = {}
        for k in self.e["schemas"]:
            if not self.incluido(k):
                continue
            hijos = set()
            s = self.e["schemas"][k]
            if s["type"] == "multiple":
                hijos |= {v["schemaName"] for v in s["variants"] if "schemaName" in v}
            else:
                for p in self.e["fields"].get(k, {}).get("properties", {}).values():
                    hijos |= set(objetos_de_tipo(p["type"]))
            g[k] = hijos
        self._grafo = g
        return g

    def almacenado(self, k):
        """¿Tiene formato binario (Pickle)? Lo libre, si lo tiene en el código limpio; lo de pago, si es
        alcanzable desde un objeto que se guarda en el registro."""
        r = nombre_rust(k)
        if r in self.p["multiples"]:
            return "pickle" in self.p["multiples"][r]
        if r in self.p["structs"]:
            return "nfields" in self.p["structs"][r]
        if not hasattr(self, "_almacenados"):
            vistos = set()
            pila = [o[0] for o in self.objetos() if not self.no_almacenado(o[1])]
            while pila:
                x = pila.pop()
                if x in vistos:
                    continue
                vistos.add(x)
                pila.extend(self.grafo().get(x, ()))
            self._almacenados = vistos
        return k in self._almacenados

    # ---------------------------------------------------------------------------------------- índices

    def ops_indice(self, k, c):
        """Operaciones de índice de un campo: [[op, args...]]."""
        r = nombre_rust(k)
        ps = self.p["structs"].get(r)
        if ps is not None and not c["pago"]:
            return [op[1:] for op in ps.get("indice", []) if op[0] == c["campo"]]
        # Campo de pago: claves ajenas para los identificadores de objetos, el índice anidado de los
        # sub-objetos que lo tienen y lo que diga decisiones.json.
        ops = []
        t = c["prop"]["type"]
        dec = self.decisiones.get("campos", {}).get(c["json"], {})
        objetivo = None
        if t["type"] == "objectId":
            objetivo = t["objectName"]
        elif t["type"] == "set" and t["class"]["type"] == "objectId":
            objetivo = t["class"]["objectName"]
        if objetivo:
            ops.append(["fk", nombre_rust(objetivo.split("/")[0])])
        for o in objetos_de_tipo(t):
            if self.tiene_indice(nombre_rust(o)):
                ops.append(["anidado"])
                break
        if r not in dec.get("solo_clave_ajena", []):
            ops += dec.get("indice", [])
        return ops

    def referido_por_indice(self, r):
        """¿Algún struct llama al índice de `r` (operación de índice anidado sobre un campo de ese tipo)?"""
        if not hasattr(self, "_referidos"):
            ref = set()
            for k, rr, _ in self.esquemas_struct():
                if self.es_multiple(k):
                    continue
                for c in self.campos(k):
                    if any(op[0] == "anidado" for op in self.ops_indice(k, c)):
                        for o in objetos_de_tipo(c["prop"]["type"]):
                            ref.add(nombre_rust(o))
            # Las variantes de un múltiple referido (o que es un objeto) también lo están.
            cambio = True
            while cambio:
                cambio = False
                for k, rr, _ in self.esquemas_struct():
                    if self.es_multiple(k) and (self.es_objeto(rr) or rr in ref):
                        for _, s, _, _ in self.variantes_multiple(k):
                            if s and s not in ref:
                                ref.add(s)
                                cambio = True
            self._referidos = ref
        return r in self._referidos

    def tiene_indice(self, r):
        cache = self.__dict__.setdefault("_tiene_indice", {})
        if r in cache:
            return cache[r]
        cache[r] = False  # ciclos (tipos recursivos): sin índice
        k = "x:" + r
        if self.es_multiple(k):
            res = any(s and self.tiene_indice(s) for _, s, _, _ in self.variantes_multiple(k))
        else:
            res = any(self.ops_indice(k, c) for c in self.campos(k))
        cache[r] = res
        return res

    def indice_struct(self, k, campos):
        """[(de_pago, [líneas])] del cuerpo de `fn index`."""
        res = []
        for c in campos:
            ops = self.ops_indice(k, c)
            if not ops:
                continue
            lineas = []
            for op in ops:
                lineas += codigo_indice(c, op)
            res.append((c["pago"], lineas))
        return res

    def indices_objeto(self, r):
        """IndexSchema de un tipo de objeto: [(Property, tipo, valor, de_pago)]."""
        res = [tuple(x) + (False,) for x in self.info_objeto(r).get("indices", [])]
        vistos = {x[0] for x in res}
        k = "x:" + r
        for s in self.esquemas_de(k):
            if s not in self.e["fields"]:
                continue
            for c in self.campos(s):
                if not c["pago"] or r not in self.p["objetos"]:
                    continue
                dec = self.decisiones.get("campos", {}).get(c["json"], {})
                e = dec.get("esquema_indice")
                if nombre_rust(s) in dec.get("solo_clave_ajena", []) or r in dec.get("solo_clave_ajena", []):
                    e = None
                if e and e[0] not in vistos:
                    vistos.add(e[0])
                    res.append(tuple(e) + (True,))
        return sorted(res, key=lambda x: (x[0] != "Text", x[0] != "Type", x[0]))

    # ------------------------------------------------------------------------------------ expresiones

    def contextos(self, k, campos):
        r = nombre_rust(k)
        ps = self.p["structs"].get(r, {})
        res = []
        for c in campos:
            if c["tipo"] != "Expression":
                continue
            if c["campo"] in ps.get("ctx", {}) and not c["pago"]:
                res.append((c, ps["ctx"][c["campo"]]))
            else:
                dec = self.decisiones.get("contextos", {}).get(f"{r}.{c['json']}")
                if dec is None:
                    raise SystemExit(f"falta en decisiones.json el contexto de la expresión {r}.{c['json']}")
                res.append((c, dec))
        return res

    # ------------------------------------------------------------------------------ valores por defecto

    def defecto_esquema(self, k, c):
        return self.e["fields"][k].get("defaults", {}).get(c["json"])

    def valor_defecto(self, k, c):
        d = self.defecto_esquema(k, c)
        if d is None:
            return "false" if c["tipo"] == "bool" else "Default::default()"
        t = c["prop"]["type"]
        self._orden_conjunto = self.p["structs"].get(nombre_rust(k), {}).get("conjuntos", {}).get(c["campo"])
        if t["type"] == "number" and t.get("format") == "size" and not opcional(c["tipo"]):
            if c["campo"] in self.p["structs"].get(nombre_rust(k), {}).get("u64", []):
                return f"{d}u64"
        return self.valor_rust(t, d, c["tipo"])

    def valor_rust(self, t, v, tipo):
        """Valor JSON por defecto → expresión Rust del tipo `tipo`."""
        if opcional(tipo):
            return f"Some({self.valor_rust(t, v, interior(tipo))})"
        k = t["type"]
        if tipo == "Expression":
            return valor_expresion(v)
        if k == "boolean":
            return "true" if v else "false"
        if k == "string":
            if tipo == "IpAddr":
                return f"IpAddr::from_str({cadena_rust(v)}).unwrap()"
            return f"{cadena_rust(v)}.to_string()"
        if k == "number":
            f = t.get("format")
            if f == "duration":
                return f"Duration::from_millis({v})"
            if f == "float":
                return f"Float::new({flotante(v)}f64)"
            if f == "size":
                return f"{v}"
            if f == "integer":
                return f"{v}i64"
            return f"{v}u64"
        if k == "enum":
            return f"{tipo}::{variante_enum(v)}"
        if k == "object":
            return self.valor_objeto(t["objectName"], v)
        if k == "set":
            items = list(v.keys()) if isinstance(v, dict) else list(v)
            sub = interior(tipo)[4:-1]
            vals = [self.valor_rust(t["class"], x, sub) for x in items]
            orden = getattr(self, "_orden_conjunto", None)
            if orden:
                vals = sorted(vals, key=lambda x: orden.index(x) if x in orden else len(orden))
            return f"Map::new(vec![{', '.join(vals)}])"
        if k == "objectList":
            vals = [self.valor_objeto(t["objectName"], x) for x in v.values()]
            return f"List::from_iter([{', '.join(vals)}])"
        if k == "map":
            kt, vt = interior(tipo)[7:-1].split(", ", 1)
            pares = [f"({self.valor_rust(t['keyClass'], a, kt)}, {self.valor_rust(t['valueClass'], b, vt)})"
                     for a, b in v.items()]
            return f"VecMap::from_iter([{', '.join(pares)}])"
        raise ValueError(f"valor por defecto no soportado: {t} {v}")

    def valor_objeto(self, esquema, v):
        r = nombre_rust(esquema)
        s = self.e["schemas"][esquema]
        if s["type"] == "multiple":
            tipo = v.get("@type")
            var = next(x for x in s["variants"] if x["name"] == tipo)
            if "schemaName" not in var:
                return f"{r}::{tipo}"
            resto = {a: b for a, b in v.items() if a != "@type"}
            if not resto:
                return f"{r}::{tipo}(Default::default())"
            return f"{r}::{tipo}({self.literal_struct(var['schemaName'], resto)})"
        if r == "Expression":
            return valor_expresion(v)
        return self.literal_struct(s["schemaName"], v)

    def literal_struct(self, esquema, v):
        r = nombre_rust(esquema)
        campos = {c["json"]: c for c in self.campos(esquema)}
        partes = []
        for j, x in v.items():
            c = campos[j]
            partes.append(f"{c['campo']}: {self.valor_rust(c['prop']['type'], x, c['tipo'])}")
        if len(partes) == len(campos):
            return f"{r} {{ {', '.join(partes)} }}"
        return f"{r} {{ {', '.join(partes)}, ..Default::default() }}"

    # ------------------------------------------------------------------------------------- validación

    def validacion(self, k, c):
        """Líneas de validación de un campo (vacío si no hay nada que validar)."""
        t = c["prop"]["type"]
        tipo = c["tipo"]
        p = propiedad(c["json"])
        r = nombre_rust(k)
        max_primero = p in self.p["structs"].get(r, {}).get("max_primero", [])
        col = coleccion(tipo)
        if col:
            if col == "Map":
                clase = t["class"]
                sub = interior(tipo)[4:-1]
                items = self.chequeo(clase, sub, p, max_primero)
                bucle = "for value in value.iter() {"
            elif col == "List":
                items = self.chequeo({"type": "object", "objectName": t["objectName"]},
                                     interior(tipo)[5:-1], p, max_primero)
                bucle = "for value in value.values() {"
            else:
                vt = interior(tipo)[7:-1].split(", ", 1)[1]
                items = self.chequeo(t["valueClass"], vt, p, max_primero)
                bucle = "for value in value.values() {"
            lineas = []
            if items:
                lineas += [bucle] + items + ["}"]
            if "minItems" in t:
                lineas += [f"if value.len() < {t['minItems']} {{",
                           f"errors.push(ValidationError::min_items(Property::{p}, {t['minItems']}));", "}"]
            if "maxItems" in t:
                lineas += [f"if value.len() > {t['maxItems']} {{",
                           f"errors.push(ValidationError::max_items(Property::{p}, {t['maxItems']}));", "}"]
            if not lineas:
                return []
            return [f"let value = &self.{c['campo']};"] + lineas
        if tipo == "Expression" and self.expr_opcional(k, c):
            return [f"let value = &self.{c['campo']};",
                    "if !value.match_.is_empty() || !value.else_.is_empty() {",
                    "value.validate(errors);", "}"]
        lineas = self.chequeo(t, interior(tipo), p, max_primero)
        if not lineas:
            return []
        if opcional(tipo):
            return [f"if let Some(value) = &self.{c['campo']} {{"] + lineas + ["}"]
        return [f"let value = &self.{c['campo']};"] + lineas

    def expr_opcional(self, k, c):
        """Expresiones que solo se validan si no están vacías. Lo libre, según el código limpio; lo de pago,
        las que no tienen valor por defecto."""
        ps = self.p["structs"].get(nombre_rust(k))
        if ps is not None and not c["pago"]:
            return c["campo"] in ps.get("expr_opcional", [])
        return self.defecto_esquema(k, c) is None

    def chequeo(self, t, tipo, p, max_primero=False):
        """Comprobaciones sobre `value` (referencia a un valor escalar u objeto)."""
        k = t["type"]
        if k == "string":
            if tipo in ("IpAddr", "IpAddrOrMask", "SocketAddr", "Id"):
                return ["if !value.is_valid() {", f"errors.push(ValidationError::invalid(Property::{p}, value));", "}"]
            if tipo != "String":
                return []
            res = ["if value.is_empty() {", f"errors.push(ValidationError::required(Property::{p}));", "}"]
            if "maxLength" in t:
                res += [f"if value.len() > {t['maxLength']} {{",
                        f"errors.push(ValidationError::max_length(Property::{p}, {t['maxLength']}));", "}"]
            return res
        if k == "objectId" or tipo == "Id":
            return ["if !value.is_valid() {", f"errors.push(ValidationError::required(Property::{p}));", "}"]
        if k == "blobId":
            return ["if value.is_empty() {", f"errors.push(ValidationError::required(Property::{p}));", "}"]
        if k == "utcDateTime":
            return ["if !value.is_valid() {", f"errors.push(ValidationError::invalid(Property::{p}, value));", "}"]
        if k == "number":
            f = t.get("format")
            res_min, res_max = [], []
            valido = []
            if f == "duration" and ("min" in t or "max" in t):
                valido = ["if !value.is_valid() {", f"errors.push(ValidationError::invalid(Property::{p}, value));", "}"]
            if "min" in t:
                lit, val = literal_numero(f, t["min"])
                res_min = [f"if *value < {lit} {{", f"errors.push(ValidationError::min_value(Property::{p}, {val}));", "}"]
            if "max" in t:
                lit, val = literal_numero(f, t["max"])
                res_max = [f"if *value > {lit} {{", f"errors.push(ValidationError::max_value(Property::{p}, {val}));", "}"]
            return valido + (res_max + res_min if max_primero else res_min + res_max)
        if k == "object":
            return ["value.validate(errors);"]
        return []

    # ----------------------------------------------------------------------------------------- parches

    def es_secreto(self, c, k=None):
        if k is not None and not c["pago"]:
            ps = self.p["structs"].get(nombre_rust(k))
            if ps is not None:
                return propiedad(c["json"]) in ps.get("secretos", [])
        t = c["prop"]["type"]
        return t["type"] == "string" and t.get("format") in ("secret", "secretText")

    def validadores(self, k, c):
        r = nombre_rust(k)
        ps = self.p["structs"].get(r)
        p = propiedad(c["json"])
        if ps is not None and not c["pago"]:
            for d in ps.get("parche", []):
                if d["prop"] == p:
                    return d.get("validadores", [])
            return []
        dec = self.decisiones.get("campos", {}).get(c["json"], {})
        if "validadores" in dec:
            return dec["validadores"]
        return validadores_por_formato(c["prop"]["type"])

    def codigo_parche(self, k, c, virtual=False, info=None):
        p = propiedad(c["json"])
        upd = c["prop"]["update"]
        if virtual:
            if upd == "serverSet":
                return f"Some(Property::{p}) => pointer.assert_server_set(),"
            return f"Some(property @ Property::{p}) => Ok(MaybeUnpatched::Unpatched {{ property, value }}),"
        if upd == "serverSet":
            return f"Some(Property::{p}) => pointer.assert_server_set(),"
        ptr = "pointer"
        if upd == "immutable":
            ptr += ".assert_read_only()?"
            if (info or {}).get("cuenta") or (c["pago"] and c["json"] == "accountId"):
                ptr += ".assert_can_set_account()?"
        vals = self.validadores(k, c)
        if vals:
            ptr += ".with_validators(&[" + ", ".join(f"StringValidator::{v}" for v in vals) + "])"
        return f"Some(Property::{p}) => self.{c['campo']}.patch({ptr}, value),"

    def orden_parche(self, k, campos):
        """[{pago, codigo}] de los brazos de `patch_property`."""
        r = nombre_rust(k)
        ps = self.p["structs"].get(r)
        props = self.e["fields"][k]["properties"]
        por_prop = {propiedad(c["json"]): c for c in campos}
        res = []
        if ps is None:
            for c in campos:
                res.append({"pago": True, "codigo": self.codigo_parche(k, c), "campo": c["campo"]})
            return res
        for d in ps.get("parche", []):
            p = d["prop"]
            if p in por_prop:
                c = por_prop[p]
                res.append({"pago": False, "codigo": self.codigo_parche(k, c, info=d), "campo": c["campo"]})
            else:
                j = next((j for j in props if propiedad(j) == p), None)
                if j is None:
                    # Propiedad antigua que ya no está en el esquema: se acepta y se ignora.
                    j = self.p["Property"]["json"][p]
                    prop = {"type": {"type": "string"}, "update": "mutable"}
                else:
                    prop = props[j]
                c = {"json": j, "prop": prop, "pago": False, "campo": serpiente(j)}
                res.append({"pago": False, "codigo": self.codigo_parche(k, c, virtual=True), "campo": None})
        # Los campos de pago van tras el brazo del campo que les precede en el struct.
        for n, c in enumerate(campos):
            if not c["pago"]:
                continue
            pos = 0
            for prev in reversed(campos[:n]):
                idx = next((i for i, x in enumerate(res) if x["campo"] == prev["campo"]), None)
                if idx is not None:
                    pos = idx + 1
                    break
            res.insert(pos, {"pago": True, "codigo": self.codigo_parche(k, c), "campo": c["campo"]})
        return res


def literal_numero(formato, v):
    """(literal para comparar, valor para el error) de un límite numérico."""
    if formato == "duration":
        return f"Duration::from_millis({int(v)})", f"{int(v)}"
    if formato == "float":
        return f"Float::new({flotante(v)})", (f"{int(v)}" if float(v).is_integer() else f"{v}")
    if formato == "integer":
        return f"({v})", f"{v}"
    return f"{v}", f"{v}"


def variante_enum(v):
    from comun import variante
    return variante(v)


def valor_expresion(v):
    otro = cadena_rust(v.get("else", ""))
    casos = list(v.get("match", {}).values())
    if not casos and "match" in v:
        return f"Expression {{ else_: {otro}.to_string(), match_: List::from_iter([]) }}"
    if not casos:
        return f"Expression {{ else_: {otro}.to_string(), ..Default::default() }}"
    ms = ", ".join(
        f"ExpressionMatch {{ if_: {cadena_rust(x['if'])}.to_string(), then: {cadena_rust(x['then'])}.to_string() }}"
        for x in casos)
    return f"Expression {{ else_: {otro}.to_string(), match_: List::from_iter([{ms}]) }}"


def objetos_de_tipo(t):
    """Esquemas de struct referidos por un tipo (objetos, listas y mapas de objetos)."""
    k = t["type"]
    if k in ("object", "objectList"):
        return [t["objectName"]]
    if k == "map":
        return objetos_de_tipo(t["valueClass"])
    return []


def validadores_por_formato(t):
    if t["type"] == "set":
        t = t["class"]
    if t["type"] != "string":
        return []
    return {
        "emailAddress": ["Email"],
        "uri": ["Trim"],
    }.get(t.get("format"), [])


def codigo_indice(c, op):
    tipo = c["tipo"]
    f = c["campo"]
    col = coleccion(tipo)
    if op[0] in ("unique", "search", "unique_global", "text"):
        llamada = f"i.{op[0]}(Property::{op[1]}, "
        if col:
            return [f"for value in self.{f}.iter() {{", llamada + "value);", "}"]
        if opcional(tipo):
            return [f"if let Some(value) = &self.{f} {{", llamada + "value);", "}"]
        return [llamada + f"&self.{f});"]
    if op[0] == "fk":
        filtro = f"Some({op[2]}.to_id())" if len(op) > 2 else "None"
        if col:
            return [f"for id in self.{f}.iter() {{", f"i.foreign_key(ObjectType::{op[1]}, Some(*id), {filtro});", "}"]
        if opcional(tipo):
            return [f"i.foreign_key(ObjectType::{op[1]}, self.{f}, {filtro});"]
        return [f"i.foreign_key(ObjectType::{op[1]}, self.{f}.into(), {filtro});"]
    if op[0] == "anidado":
        if col:
            return [f"for item in self.{f}.values() {{", "item.index(i);", "}"]
        if opcional(tipo):
            return [f"if let Some(value) = &self.{f} {{", "value.index(i);", "}"]
        return [f"self.{f}.index(i);"]
    if op[0] == "ugc":
        return [f"i.unique_global_composite(Property::{op[1]}, &self.{f}, {op[2]});"]
    raise ValueError(op)
