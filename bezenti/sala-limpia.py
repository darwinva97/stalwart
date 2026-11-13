#!/usr/bin/env python3
"""
Sala limpia de la edición bezenti de Stalwart.

Stalwart mezcla en el mismo código la parte libre (AGPL-3.0) y la de pago (SEL, "Stalwart Enterprise
License"). La edición bezenti reimplementa funciones equivalentes a las de pago con código propio,
escrito DESDE CERO. Para que eso sea defendible, quien escribe el código nuevo no debe ver el de pago.

Este script genera una copia del árbol de Stalwart SIN código SEL:
  - archivos 100 % SEL (cabecera "SPDX-License-Identifier: LicenseRef-SEL" sin "AGPL-3.0-only OR"):
    se sustituyen por un aviso con su número de líneas;
  - fragmentos SEL dentro de archivos libres (SPDX-SnippetBegin … LicenseRef-SEL … SPDX-SnippetEnd):
    se sustituyen por una marca "[fragmento SEL eliminado: N líneas]" en el mismo sitio.
Así se sabe DÓNDE había código de pago (para poner ahí los enganches propios) pero no QUÉ hacía.

Uso:
  bezenti/sala-limpia.py <árbol-de-stalwart> <destino>        → copia limpia
  bezenti/sala-limpia.py --inventario <árbol-de-stalwart>      → inventario (solo ubicaciones) en Markdown
  bezenti/sala-limpia.py --comprobar <árbol>                   → falla si queda algún resto de SEL

Reglas de la sala limpia (bezenti/README.md): implementar SOLO a partir de la copia limpia, la documentación
PÚBLICA de Stalwart y las especificaciones de bezenti/especificaciones/. Nunca abrir los archivos SEL originales.
"""
import os, re, shutil, sys, collections

EXCLUIR_DIRS = {".git", "target", "node_modules"}
SEL_SOLO = re.compile(r"SPDX-License-Identifier:\s*LicenseRef-SEL\s*$", re.M)
DUAL = re.compile(r"AGPL-3\.0-only\s+OR\s+LicenseRef-SEL")
INICIO = re.compile(r"^(\s*)(//|#|/\*|\*|<!--)?\s*SPDX-SnippetBegin")
FIN = re.compile(r"SPDX-SnippetEnd")


def es_texto(ruta):
    try:
        with open(ruta, "rb") as f:
            return b"\0" not in f.read(4096)
    except OSError:
        return False


def es_sel_entero(texto):
    cabecera = "\n".join(texto.splitlines()[:12])
    return bool(SEL_SOLO.search(cabecera)) and not DUAL.search(cabecera)


def limpiar(texto):
    """Devuelve (texto_limpio, [(línea_inicio, n_líneas)]) quitando los fragmentos SEL."""
    lineas = texto.split("\n")
    salida, quitados, i = [], [], 0
    while i < len(lineas):
        m = INICIO.match(lineas[i])
        if m:
            j = i + 1
            while j < len(lineas) and not FIN.search(lineas[j]):
                j += 1
            bloque = lineas[i:j + 1]
            if any("LicenseRef-SEL" in l for l in bloque[:4]):
                sangria = m.group(1)
                salida.append(f"{sangria}// [fragmento SEL eliminado: {len(bloque)} líneas]")
                quitados.append((i + 1, len(bloque)))
                i = j + 1
                continue
        salida.append(lineas[i])
        i += 1
    return "\n".join(salida), quitados


def recorrer(raiz, binarios=False):
    """Archivos de texto (o, con binarios=True, solo los binarios, que se copian tal cual)."""
    for d, dirs, archivos in os.walk(raiz):
        dirs[:] = [x for x in dirs if x not in EXCLUIR_DIRS]
        for a in archivos:
            ruta = os.path.join(d, a)
            if os.path.islink(ruta):
                continue
            if es_texto(ruta) != binarios:
                yield ruta


def copia_limpia(origen, destino):
    if os.path.exists(destino):
        sys.exit(f"el destino ya existe: {destino}")
    n_arch = n_frag = 0
    for ruta in recorrer(origen):
        rel = os.path.relpath(ruta, origen)
        dst = os.path.join(destino, rel)
        os.makedirs(os.path.dirname(dst), exist_ok=True)
        with open(ruta, encoding="utf-8", errors="surrogateescape") as f:
            texto = f.read()
        if es_sel_entero(texto):
            n = texto.count("\n") + 1
            with open(dst, "w", encoding="utf-8") as f:
                f.write(f"// [archivo SEL eliminado: {n} líneas]\n")
            n_arch += 1
            continue
        limpio, quitados = limpiar(texto)
        n_frag += len(quitados)
        with open(dst, "w", encoding="utf-8", errors="surrogateescape") as f:
            f.write(limpio)
        shutil.copymode(ruta, dst)
    n_bin = 0
    for ruta in recorrer(origen, binarios=True):
        dst = os.path.join(destino, os.path.relpath(ruta, origen))
        os.makedirs(os.path.dirname(dst), exist_ok=True)
        shutil.copy2(ruta, dst)
        n_bin += 1
    print(f"copia limpia en {destino}: {n_arch} archivos SEL y {n_frag} fragmentos SEL eliminados; {n_bin} binarios copiados")


def inventario(origen):
    archivos, fragmentos = [], collections.defaultdict(list)
    for ruta in recorrer(origen):
        rel = os.path.relpath(ruta, origen)
        with open(ruta, encoding="utf-8", errors="surrogateescape") as f:
            texto = f.read()
        if es_sel_entero(texto):
            archivos.append((rel, texto.count("\n") + 1))
            continue
        for linea, n in limpiar(texto)[1]:
            fragmentos[rel].append((linea, n))
    print("# Inventario de código SEL (solo ubicaciones, sin contenido)\n")
    print(f"- Archivos 100 % SEL: **{len(archivos)}** ({sum(n for _, n in archivos)} líneas)")
    print(f"- Fragmentos SEL en archivos libres: **{sum(len(v) for v in fragmentos.values())}** en **{len(fragmentos)}** archivos\n")
    por_crate = collections.defaultdict(lambda: [0, 0])
    for rel, _ in archivos:
        por_crate[rel.split("/")[1] if rel.startswith("crates/") else rel.split("/")[0]][0] += 1
    for rel, v in fragmentos.items():
        por_crate[rel.split("/")[1] if rel.startswith("crates/") else rel.split("/")[0]][1] += len(v)
    print("## Por crate\n\n| crate | archivos SEL | fragmentos SEL |\n|---|---|---|")
    for c, (a, fr) in sorted(por_crate.items(), key=lambda kv: -(kv[1][0] * 50 + kv[1][1])):
        print(f"| {c} | {a} | {fr} |")
    print("\n## Archivos 100 % SEL\n")
    for rel, n in sorted(archivos):
        print(f"- `{rel}` ({n} líneas)")
    print("\n## Fragmentos SEL (archivo: línea+longitud)\n")
    for rel in sorted(fragmentos):
        print(f"- `{rel}`: " + ", ".join(f"{l}+{n}" for l, n in fragmentos[rel]))


def comprobar(raiz):
    restos = []
    for ruta in recorrer(raiz):
        with open(ruta, encoding="utf-8", errors="surrogateescape") as f:
            texto = f.read()
        if es_sel_entero(texto) or limpiar(texto)[1]:
            restos.append(os.path.relpath(ruta, raiz))
    if restos:
        print("QUEDA CÓDIGO SEL en:\n  " + "\n  ".join(restos[:50]))
        sys.exit(1)
    print("sin código SEL")


if __name__ == "__main__":
    a = sys.argv[1:]
    if len(a) == 2 and a[0] == "--inventario":
        inventario(a[1])
    elif len(a) == 2 and a[0] == "--comprobar":
        comprobar(a[1])
    elif len(a) == 2:
        copia_limpia(a[0], a[1])
    else:
        sys.exit(__doc__)
