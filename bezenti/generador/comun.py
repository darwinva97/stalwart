# SPDX-FileCopyrightText: 2026 bezenti
#
# SPDX-License-Identifier: AGPL-3.0-only
"""Utilidades comunes del generador del registro (edición bezenti de Stalwart).

Solo usa la biblioteca estándar de Python 3 y, para dar formato, el `rustfmt` de la cadena de Rust.
"""

import gzip
import json
import os
import re
import shutil
import subprocess

# Los seis archivos que genera el generador, en el orden en que se escriben.
ARCHIVOS = [
    "enums.rs",
    "enums_impl.rs",
    "properties.rs",
    "properties_impl.rs",
    "structs.rs",
    "structs_impl.rs",
]

MARCA_SEL = re.compile(r"^\s*// \[(fragmento|archivo) SEL eliminado: \d+ líneas\]\s*$")


def cargar_json(ruta):
    """Lee un JSON, comprimido con gzip o no."""
    if ruta.endswith(".gz"):
        with gzip.open(ruta, "rt", encoding="utf-8") as f:
            return json.load(f)
    with open(ruta, encoding="utf-8") as f:
        return json.load(f)


def guardar_json(ruta, datos):
    with open(ruta, "w", encoding="utf-8") as f:
        json.dump(datos, f, ensure_ascii=False, indent=1, sort_keys=False)
        f.write("\n")


def sin_marcas(texto):
    """Quita las líneas de marca de la sala limpia (para comparar con la salida del generador)."""
    return "\n".join(l for l in texto.split("\n") if not MARCA_SEL.match(l))


def nombre_rust(nombre):
    """Nombre de esquema ("x:Domain", "x:Trace/InboundDelivery") → identificador de tipo Rust."""
    if nombre.startswith("x:"):
        nombre = nombre[2:]
    return nombre


def variante(cadena):
    """Valor de un enum del esquema → nombre de la variante Rust.

    Reglas (deducidas del código AGPL generado): se trocea por caracteres no alfanuméricos y se pone en
    mayúscula la primera letra de cada trozo; "+" es "Plus" y "GMT-" es "GMTMinus"
    ("Etc/GMT-3" → "EtcGMTMinus3"); si el resultado empieza por cifra se antepone "V" ("16" → "V16").
    """
    s = cadena.replace("+", "Plus")
    s = s.replace("GMT-", "GMTMinus")
    trozos = [t for t in re.split(r"[^A-Za-z0-9]+", s) if t]
    r = "".join(t[:1].upper() + t[1:] for t in trozos)
    if r[:1].isdigit():
        r = "V" + r
    return r


def propiedad(cadena):
    """Nombre JSON de una propiedad ("memberTenantId", "@type") → variante de `Property`."""
    if cadena == "@type":
        return "Type"
    return variante(cadena)


def serpiente(nombre):
    """camelCase → snake_case, tal como lo hace el generador original ("maxVCardSize" → "max_v_card_size")."""
    s = re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", nombre)
    s = re.sub(r"([A-Z])([A-Z][a-z])", r"\1_\2", s)
    s = s.lower()
    if s in PALABRAS_RESERVADAS:
        s += "_"
    return s


PALABRAS_RESERVADAS = {
    "type", "match", "if", "else", "enum", "struct", "fn", "let", "loop", "move", "mut", "ref", "return",
    "self", "static", "trait", "use", "where", "while", "for", "in", "impl", "mod", "pub", "crate", "super",
    "const", "dyn", "as", "break", "continue", "extern", "false", "true", "unsafe", "async", "await",
    "abstract", "become", "box", "do", "final", "macro", "override", "priv", "typeof", "unsized", "virtual",
    "yield", "try",
}


def constante(nombre):
    """CamelCase → CONSTANTE."""
    return serpiente(nombre).upper().rstrip("_")


def rustfmt_disponible():
    return shutil.which("rustfmt") is not None


def rustfmt(texto, edicion="2024"):
    """Da formato a código Rust con rustfmt (igual que el código generado original)."""
    r = subprocess.run(
        ["rustfmt", "--edition", edicion, "--emit", "stdout", "--quiet"],
        input=texto.encode("utf-8"),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if r.returncode != 0:
        # Guardamos el texto para poder depurar el error de sintaxis.
        ruta = os.path.join(os.environ.get("TMPDIR", "/tmp"), "generador-error.rs")
        with open(ruta, "w", encoding="utf-8") as f:
            f.write(texto)
        raise SystemExit(f"rustfmt falló (texto en {ruta}):\n{r.stderr.decode('utf-8', 'replace')[:3000]}")
    return r.stdout.decode("utf-8")


def cadena_rust(s):
    """Literal de cadena Rust."""
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n").replace("\t", "\\t").replace("\r", "\\r") + '"'
