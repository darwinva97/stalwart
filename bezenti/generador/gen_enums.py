# SPDX-FileCopyrightText: 2026 bezenti
#
# SPDX-License-Identifier: AGPL-3.0-only
"""Genera `enums.rs` y `enums_impl.rs`: un enum Rust por cada enum del esquema."""

from comun import cadena_rust


def enums_rs(m, w):
    for enum, pago_enum in m.enums():
        vs = m.variantes(enum)
        with w.pago(pago_enum):
            w.l("#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]")
            w.l("#[repr(u16)]")
            w.l(f"pub enum {enum} {{")
            # La variante predeterminada es la primera del esquema; si es de pago, sin la feature lo es la
            # primera libre.
            primera_pago = vs[0][3] and not pago_enum
            primera_libre = next((rv for _, rv, _, p in vs if not p), None)
            for n, (_, rv, i, p) in enumerate(vs):
                with w.pago(p and not pago_enum):
                    if n == 0:
                        w.l("#[default]")
                    elif primera_pago and rv == primera_libre:
                        w.l(f'#[cfg_attr(not(feature = "{m.feature}"), default)]')
                    w.l(f"{rv} = {i},")
            w.l("}")
            w.l("")
    for nombre, valores in m.p["estaticos"].items():
        tipo = "ExpressionVariable" if nombre.endswith("_VARIABLE") else "ExpressionConstant"
        w.l(f"pub static {nombre}: &[{tipo}] = &[")
        for v in valores:
            w.l(f"{tipo}::{v},")
        w.l("];")
        w.l("")


def parse_fn(w, enum, vs):
    w.l("fn parse(value: &str) -> Option<Self> {")
    w.l("        hashify::map! {")
    w.l("            value.as_bytes(),")
    w.l(f"            {enum},")
    for nombre, rv, _, _ in vs:
        w.l(f"            b{cadena_rust(nombre)} => {enum}::{rv},")
    w.l("        }")
    w.l("        .copied()")
    w.l("    }")


def enums_impl_rs(m, w):
    w.l("use crate::schema::prelude::*;")
    w.l("")
    for enum, pago_enum in m.enums():
        emitir_impl(m, w, enum, m.variantes(enum), pago_enum, enum in m.p["enums_all"], m.count_enum)


def emitir_impl(m, w, enum, vs, pago_enum, con_all, contar):
    """EnumImpl (+ ALL) + Serialize + Deserialize de un enum. `vs`: [(json, Variante, id, de_pago)]."""
    hay_pago = any(v[3] for v in vs) and not pago_enum
    with w.pago(pago_enum, attr=None):
        w.l(m.cfg) if pago_enum and m.bezenti else None
        w.l(f"impl EnumImpl for {enum} {{")
        if hay_pago:
            # hashify::map! no admite cfg por entrada: dos versiones de parse.
            with w.pago(True, attr=None):
                w.l(m.cfg)
                parse_fn(w, enum, vs)
                w.l("")
            w.l(m.cfg_no)
            parse_fn(w, enum, [v for v in vs if not v[3]])
        else:
            parse_fn(w, enum, vs)
        w.l("")
        w.l("fn as_str(&self) -> &'static str {")
        w.l("match self {")
        for nombre, rv, _, p in vs:
            with w.pago(p and not pago_enum):
                w.l(f"{enum}::{rv} => {cadena_rust(nombre)},")
        w.l("}")
        w.l("}")
        w.l("")
        w.l("fn to_id(&self) -> u16 {")
        w.l("*self as u16")
        w.l("}")
        w.l("")
        w.l("fn from_id(id: u16) -> Option<Self> {")
        w.l("match id {")
        for nombre, rv, i, p in vs:
            with w.pago(p and not pago_enum):
                w.l(f"{i} => Some({enum}::{rv}),")
        for patron, rv in m.p["enums"].get(enum, {}).get("alias", []):
            w.l(f"{patron} => Some({enum}::{rv}),")
        w.l("_ => None,")
        w.l("}")
        w.l("}")
        w.l("")
        w.l(f"const COUNT: usize = {contar(enum, vs)};")
        w.l("}")
        w.l("")
        if con_all:
            w.l(m.cfg) if pago_enum and m.bezenti else None
            w.l(f"impl {enum} {{")
            w.l(f"pub const ALL: &[{enum}] = &[")
            for nombre, rv, i, p in sorted(vs, key=lambda v: v[2]):
                with w.pago(p and not pago_enum):
                    w.l(f"{enum}::{rv},")
            w.l("];")
            w.l("}")
            w.l("")
        w.l(m.cfg) if pago_enum and m.bezenti else None
        w.l(f"impl serde::Serialize for {enum} {{")
        w.l("fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>")
        w.l("where")
        w.l("S: serde::Serializer,")
        w.l("{")
        w.l("serializer.serialize_str(self.as_str())")
        w.l("}")
        w.l("}")
        w.l("")
        w.l(m.cfg) if pago_enum and m.bezenti else None
        w.l(f"impl<'de> serde::Deserialize<'de> for {enum} {{")
        w.l("fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>")
        w.l("where")
        w.l("D: serde::Deserializer<'de>,")
        w.l("{")
        w.l("let s = Cow::<str>::deserialize(deserializer)?;")
        w.l("Self::parse(&s).ok_or_else(|| serde::de::Error::unknown_variant(&s, &[]))")
        w.l("}")
        w.l("}")
        w.l("")
