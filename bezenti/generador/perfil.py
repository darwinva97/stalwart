#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 bezenti
#
# SPDX-License-Identifier: AGPL-3.0-only
"""Extrae el «perfil» del registro a partir de los seis archivos AGPL generados por Stalwart.

El esquema (`resources/schema/schema.json.gz`) describe la API pero no todo lo que el código necesita:
los identificadores numéricos persistidos (de tipos de objeto, propiedades y variantes de enums), el orden de
los campos y su posición en el formato binario («pickle»), los validadores de cadenas, los índices, los grupos
de variables de las expresiones, los indicadores de cada objeto, etc. Esos datos están en el código AGPL
generado de la rama limpia (`limpio-upstream`). Este script los lee de ahí y los guarda en `perfil.json`,
SOLO como datos (nombres, números, listas): el código lo vuelve a escribir el generador con sus plantillas.

Uso:
    perfil.py <directorio-con-los-seis-archivos> <schema.json.gz> <perfil.json>
"""

import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from comun import cargar_json, guardar_json, sin_marcas  # noqa: E402


def leer(directorio, nombre):
    with open(os.path.join(directorio, nombre), encoding="utf-8") as f:
        return sin_marcas(f.read())


def bloques(texto, cabecera):
    """Devuelve [(m, cuerpo)] para cada bloque de nivel superior que empieza por la expresión `cabecera`
    y termina en la primera línea "}" sin sangría."""
    res = []
    for m in re.finditer(cabecera, texto, re.M):
        fin = texto.find("\n}", m.end())
        res.append((m, texto[m.end():fin]))
    return res


# --------------------------------------------------------------------------------------------- enums


def extraer_enums(directorio):
    enums = leer(directorio, "enums.rs")
    impl = leer(directorio, "enums_impl.rs")
    res = {}
    for m, cuerpo in bloques(enums, r"^pub enum (\w+) \{"):
        ids = {}
        for v, i in re.findall(r"^    (\w+) = (\d+),", cuerpo, re.M):
            ids[v] = int(i)
        res[m.group(1)] = {"ids": ids}
    for m, cuerpo in bloques(impl, r"^impl EnumImpl for (\w+) \{"):
        if m.group(1) in res:
            e = res[m.group(1)]
            e["count"] = int(re.search(r"const COUNT: usize = (\d+);", cuerpo).group(1))
            # Brazos de from_id que no son el id de una variante (ids antiguos que se siguen aceptando).
            desde = cuerpo[cuerpo.find("fn from_id"):]
            alias = []
            for patron, v in re.findall(r"^            (\S+) => Some\(\w+::(\w+)\),", desde, re.M):
                if not (patron.isdigit() and e["ids"].get(v) == int(patron)):
                    alias.append([patron, v])
            if alias:
                e["alias"] = alias
    estaticos = {}
    for m in re.finditer(r"^pub static (\w+): &\[(\w+)\] =\s*&\[(.*?)\];", enums, re.M | re.S):
        estaticos[m.group(1)] = re.findall(r"\w+::(\w+)", m.group(3))
    todos = re.findall(r"^impl (\w+) \{\n    pub const ALL:", impl, re.M)
    return res, estaticos, todos


# ---------------------------------------------------------------------------------------- properties


def extraer_propiedades(directorio):
    props = leer(directorio, "properties.rs")
    impl = leer(directorio, "properties_impl.rs")
    res = {}
    for nombre in ("ObjectType", "Property"):
        cuerpo = props.split(f"pub enum {nombre} {{")[1].split("\n}")[0]
        ids = {v: int(i) for v, i in re.findall(r"^    (\w+) = (\d+),", cuerpo, re.M)}
        bloque = impl.split(f"impl EnumImpl for {nombre} {{")[1].split("\n}")[0]
        json = dict((v, s) for s, v in re.findall(r'b"([^"]*)" => \w+::(\w+),', bloque))
        count = int(re.search(r"const COUNT: usize = (\d+);", bloque).group(1))
        res[nombre] = {"ids": ids, "count": count, "json": json}
    return res


# ----------------------------------------------------------------------------------------- objetos


def extraer_objetos(directorio):
    """Indicadores (FLAGS), VERSION e índices declarados de cada tipo de objeto."""
    impl = leer(directorio, "structs_impl.rs")
    props = leer(directorio, "properties_impl.rs")
    res = {}
    for m, cuerpo in bloques(impl, r"^impl ObjectImpl for (\w+) \{"):
        flags = re.search(r"const FLAGS: u64 = ([^;]+);", cuerpo).group(1)
        version = int(re.search(r"const VERSION: u8 = (\d+);", cuerpo).group(1))
        res[m.group(1)] = {
            "flags": [] if flags == "0" else [f.strip() for f in flags.split("|")],
            "version": version,
        }
    cuerpo = props[props.index("pub fn indexes(&self)"):]
    cuerpo = cuerpo[: cuerpo.index("\n    }\n")]
    for m in re.finditer(r"ObjectType::(\w+) => vec!\[(.*?)\],?\n            (?=ObjectType|_ =>)", cuerpo, re.S):
        res[m.group(1)]["indices"] = [
            list(x) for x in re.findall(
                r"IndexSchema::new\(\s*Property::(\w+),\s*IndexSchemaType::(\w+),\s*IndexSchemaValueType::(\w+),?\s*\)",
                m.group(2))
        ]
    return res


# ----------------------------------------------------------------------------------------- structs

PATRONES_INDICE = [
    (r"i\.(unique|search|unique_global|text)\(Property::(\w+), &self\.(\w+)\);", lambda g: (g[2], [g[0], g[1]])),
    (r"if let Some\(value\) = &self\.(\w+) \{ i\.(unique|search|unique_global|text)\(Property::(\w+), value\); \}",
     lambda g: (g[0], [g[1], g[2]])),
    (r"for value in self\.(\w+)\.iter\(\) \{ i\.(unique|search|unique_global|text)\(Property::(\w+), value\); \}",
     lambda g: (g[0], [g[1], g[2]])),
    (r"i\.foreign_key\(ObjectType::(\w+), self\.(\w+)(?:\.into\(\))?, None\);", lambda g: (g[1], ["fk", g[0]])),
    (r"for id in self\.(\w+)\.iter\(\) \{ i\.foreign_key\( ?ObjectType::(\w+), Some\(\*id\), (None|Some\((\w+::\w+)\.to_id\(\)\)),? ?\); \}",
     lambda g: (g[0], ["fk", g[1]] + ([g[3]] if g[3] else []))),
    (r"self\.(\w+)\.index\(i\);", lambda g: (g[0], ["anidado"])),
    (r"if let Some\(value\) = &self\.(\w+) \{ value\.index\(i\); \}", lambda g: (g[0], ["anidado"])),
    (r"for item in self\.(\w+)\.values\(\) \{ item\.index\(i\); \}", lambda g: (g[0], ["anidado"])),
    (r"i\.unique_global_composite\( ?Property::(\w+), &self\.(\w+), (.*?),? ?\);", lambda g: (g[1], ["ugc", g[0], g[2]])),
]


def extraer_indice(cuerpo):
    """Operaciones de índice de un struct: [[campo, op...]] en orden."""
    ops = []
    resto = cuerpo.strip()
    while resto:
        for patron, f in PATRONES_INDICE:
            r = re.match(patron, resto)
            if r:
                campo, op = f(r.groups())
                ops.append([campo] + op)
                resto = resto[r.end():].strip()
                break
        else:
            raise ValueError(f"operación de índice no reconocida: {resto[:200]}")
    return ops



def tipo_esperado(t):
    """Tipo Rust que corresponde a un tipo del esquema (para guardar en el perfil solo las excepciones)."""
    from modelo import tipo_rust
    return tipo_rust(t)


def extraer_structs(directorio, esquema):
    src = leer(directorio, "structs.rs")
    impl = leer(directorio, "structs_impl.rs")
    structs, multiples = {}, {}
    for m in re.finditer(r"^pub (struct|enum) (\w+) \{(.*?)^\}", src, re.M | re.S):
        clase, nombre, cuerpo = m.groups()
        if clase == "enum":
            multiples[nombre] = {"variantes": re.findall(r"^    (\w+)", cuerpo, re.M)}
            continue
        campos = []
        renombre = None
        for linea in cuerpo.split("\n"):
            linea = linea.strip()
            r = re.match(r'#\[serde\(rename = "([^"]+)"\)\]', linea)
            if r:
                renombre = r.group(1)
                continue
            r = re.match(r"pub (\w+): (.+),$", linea)
            if r:
                campos.append({"campo": r.group(1), "json": renombre or r.group(1), "tipo": r.group(2)})
                renombre = None
        structs[nombre] = {"campos": campos}
    # Posición de cada campo en el formato binario y su bit en el mapa de bits.
    for m, cuerpo in bloques(impl, r"^impl Pickle for (\w+) \{"):
        nombre = m.group(1)
        if nombre in multiples:
            ids = re.findall(r"^            (\d+) => (?:Pickle::unpickle\(stream\)\.map\(\w+::(\w+)\)|Some\(\w+::(\w+)\)),",
                             cuerpo, re.M)
            multiples[nombre]["pickle"] = {a or b: int(i) for i, a, b in ids}
            antiguos = re.findall(r"^            (\d+) => \{\n\s*let _: (\w+) = Pickle::unpickle", cuerpo, re.M)
            if antiguos:
                multiples[nombre]["antiguos"] = [[int(i), t] for i, t in antiguos]
            continue
        s = structs[nombre]
        b = re.search(r"begin_block\(out, (\d+), (\d+), (\w+)\)", cuerpo)
        s["nfields"], s["nflags"] = int(b.group(1)), int(b.group(2))
        indice = {}
        for c, n in re.findall(r"\b(\w+): field(\d+)\b", cuerpo):
            indice[c] = int(n)
        rellenos = [int(n) for n in re.findall(r"#\[cfg\(not\(feature = \"enterprise\"\)\)\]\n\s*if block\.has\((\d+)\)", cuerpo)]
        if rellenos:
            s["rellenos"] = rellenos
        bits = {}
        for n, decl in re.findall(r"let field(\d+): .*? = (.*?);\n", cuerpo, re.S):
            f = re.search(r"block\.flag\((\d+)\)", decl)
            if f:
                bits[int(n)] = int(f.group(1))
        for c in s["campos"]:
            c["idx"] = indice[c["campo"]]
            if c["idx"] in bits:
                c["bit"] = bits[c["idx"]]
    # Orden de las propiedades en los parches, validadores de cadenas y comprobaciones extra.
    jsonp = {}
    for m, cuerpo in bloques(impl, r"^impl RegistryJsonPropertyPatch for (\w+) \{"):
        nombre = m.group(1)
        brazos = re.split(r"\n            (?=Some\(|_ =>)", cuerpo.split("match pointer.next_property() {")[1])
        orden = []
        for b in brazos:
            b = " ".join(b.split())
            r = re.match(r"Some\((?:property @ )?Property::(\w+)\)", b)
            if not r or r.group(1) == "Type":
                continue
            d = {"prop": r.group(1)}
            v = re.search(r"with_validators\(&\[([^\]]*)\]\)", b)
            if v:
                d["validadores"] = re.findall(r"StringValidator::(\w+)", v.group(1))
            if "assert_can_set_account" in b:
                d["cuenta"] = True
            orden.append(d)
        structs[nombre]["parche"] = orden
    # Operaciones de índice por campo.
    for m in re.finditer(r"^impl (?:ObjectImpl for )?(\w+) \{\n(.*?)^\}", impl, re.M | re.S):
        nombre = m.group(1)
        i = re.search(r"fn index<'x>\(&'x self, i: &mut IndexBuilder<'x>\) \{(.*?)\n    \}\n", m.group(2), re.S)
        if not i or nombre not in structs:
            continue
        structs[nombre]["indice"] = extraer_indice(" ".join(i.group(1).split()))
    # Contextos de las expresiones.
    for m in re.finditer(r"pub fn ctx_\w+\(&self\) -> ExpressionContext<'_> \{(.*?)\n    \}", impl, re.S):
        pass
    for m in re.finditer(r"^impl (\w+) \{\n    pub fn ctx_(.*?)^\}", impl, re.M | re.S):
        nombre = m.group(1)
        ctx = {}
        for c in re.finditer(r"expr: &self\.(\w+),.*?allowed_variables: ([^,]+),\s*allowed_constants: ([^,]+),", m.group(0), re.S):
            ctx[c.group(1)] = [c.group(2).strip(), c.group(3).strip()]
        structs[nombre]["ctx"] = ctx
    # Comprobaciones de máximo antes que de mínimo y literales numéricos con sufijo (estilo del original).
    for m, cuerpo in bloques(impl, r"^impl (?:ObjectImpl for )?(\w+) \{"):
        nombre = m.group(1)
        if nombre not in structs:
            continue
        eo = re.findall(r"let value = &self\.(\w+);\n\s*if !value\.match_\.is_empty\(\)", cuerpo)
        if eo:
            structs[nombre]["expr_opcional"] = eo
        mp = re.findall(r"if \*value > [^{]*\{\s*errors\.push\(ValidationError::max_value\(\s*Property::(\w+),[^;]*;\s*\}\s*if \*value < ", cuerpo)
        if mp:
            structs[nombre]["max_primero"] = mp
    for m, cuerpo in bloques(impl, r"^impl Default for (\w+) \{"):
        nombre = m.group(1)
        if nombre not in structs:
            continue
        sufijo = [c for c, v in re.findall(r"^            (\w+): (\d+)u64,$", cuerpo, re.M)]
        if sufijo:
            structs[nombre]["u64"] = sufijo
        # Orden de los elementos de los conjuntos por defecto (el esquema los da ordenados alfabéticamente).
        conj = {}
        for c, items in re.findall(r"^            (\w+): Map::new\(vec!\[(.*?)\]\),$", cuerpo, re.M | re.S):
            vals = [x.strip() for x in items.split(",") if x.strip()]
            if vals != sorted(vals):
                conj[c] = vals
        if conj:
            structs[nombre]["conjuntos"] = conj
    # Propiedades que se ocultan al leer (MASKED_PASSWORD).
    for m, cuerpo in bloques(impl, r"^impl IntoValue for (\w+) \{"):
        if m.group(1) in structs:
            sec = re.findall(r"Property::(\w+),\s*if opts\.include_secrets", cuerpo)
            if sec:
                structs[m.group(1)]["secretos"] = sec
    # Solo se guardan los tipos que no son los que da el esquema.
    for nombre, s in structs.items():
        props = esquema["fields"]["x:" + nombre]["properties"]
        for c in s["campos"]:
            if tipo_esperado(props[c["json"]]["type"]) == c["tipo"]:
                del c["tipo"]
    return structs, multiples


def extraer_huecos(directorio):
    """Bloques `impl` que terminan en línea en blanco: ahí había código de pago desconocido (no es del esquema),
    del que solo queda el hueco. Se guardan para reproducir el formato en la prueba de fidelidad."""
    impl = leer(directorio, "structs_impl.rs")
    res, vistas = [], {}
    for m in re.finditer(r"^(impl[^\n]*\{)\n(.*?)^\}\n", impl, re.M | re.S):
        vistas[m.group(1)] = vistas.get(m.group(1), 0) + 1
        if m.group(2).endswith("\n\n"):
            res.append(f"{vistas[m.group(1)]}:{m.group(1)}")
    return res


def main():
    if len(sys.argv) != 4:
        sys.exit(__doc__)
    directorio, ruta_esquema, salida = sys.argv[1:]
    esquema = cargar_json(ruta_esquema)
    perfil = {}
    perfil["enums"], perfil["estaticos"], perfil["enums_all"] = extraer_enums(directorio)
    perfil.update(extraer_propiedades(directorio))
    perfil["objetos"] = extraer_objetos(directorio)
    perfil["structs"], perfil["multiples"] = extraer_structs(directorio, esquema)
    perfil["huecos"] = extraer_huecos(directorio)
    guardar_json(salida, perfil)
    print(f"perfil escrito en {salida}")


if __name__ == "__main__":
    main()
