# SPDX-FileCopyrightText: 2026 bezenti
#
# SPDX-License-Identifier: AGPL-3.0-only
"""Genera `structs.rs` (definiciones) y `structs_impl.rs` (validación, índices, formato binario, valores por
defecto, conversión a JMAP y parches)."""


def structs_rs(m, w):
    w.l("use crate::schema::prelude::*;")
    for k, r, pago in m.esquemas_struct():
        with w.pago(pago, attr=None):
            if m.es_multiple(k):
                w.l(m.cfg) if pago and m.bezenti else None
                w.l("#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]")
                w.l('#[serde(tag = "@type")]')
                w.l(f"pub enum {r} {{")
                for nombre, sub, _, p in m.variantes_multiple(k):
                    with w.pago(p and not pago):
                        w.l(f"{nombre}({sub})," if sub else f"{nombre},")
                w.l("}")
            else:
                w.l(m.cfg) if pago and m.bezenti else None
                w.l("#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]")
                w.l("#[serde(default)]")
                w.l(f"pub struct {r} {{")
                for c in m.campos(k):
                    with w.pago(c["pago"] and not pago):
                        if c["campo"] != c["json"]:
                            w.l(f'#[serde(rename = "{c["json"]}")]')
                        w.l(f"pub {c['campo']}: {c['tipo']},")
                w.l("}")
            w.l("")


# ------------------------------------------------------------------------------------------ structs_impl

from comun import cadena_rust, propiedad  # noqa: E402
from reglas import opcional  # noqa: E402


def structs_impl_rs(m, w):
    w.l("use crate::schema::prelude::*;")
    w.l("")
    for k, r, pago in m.esquemas_struct():
        with w.pago(pago, attr=None):
            g = GenStruct(m, w, k, r, pago)
            if m.es_multiple(k):
                g.multiple()
            else:
                g.simple()


class GenStruct:
    def __init__(self, m, w, k, r, pago):
        self.m, self.w, self.k, self.r, self.pago = m, w, k, r, pago
        self.objeto = m.es_objeto(r)
        self.almacenado = m.almacenado(k)

    # Atributo para un bloque `impl` completo de un struct de pago.
    def cab(self, texto):
        if self.pago and self.m.bezenti:
            self.w.l(self.m.cfg)
        self.w.l(texto)

    def pago_campo(self, c):
        return self.w.pago(c["pago"] and not self.pago)

    # ------------------------------------------------------------------------------------- múltiple

    def multiple(self):
        m, w, k, r = self.m, self.w, self.k, self.r
        vs = m.variantes_multiple(k)
        def brazos(f):
            for nombre, sub, i, p in vs:
                with w.pago(p and not self.pago):
                    f(nombre, sub, i)
        if self.objeto:
            self.cab(f"impl ObjectImpl for {r} {{")
            self.cabecera_objeto()
        else:
            self.cab(f"impl {r} {{")
        hay_sub = any(s for _, s, _, _ in vs)
        w.l("fn validate(&self, errors: &mut Vec<ValidationError>) -> bool {" if hay_sub
            else "fn validate(&self, _: &mut Vec<ValidationError>) -> bool {")
        w.l("match self {")
        brazos(lambda n, s, i: w.l(f"{r}::{n}(inner) => inner.validate(errors)," if s else f"{r}::{n} => true,"))
        w.l("}")
        w.l("}")
        indexables = [(n, s, i, p) for n, s, i, p in vs if s and m.tiene_indice(s)]
        con_tipo = self.objeto and any(x[0] == "Type" for x in m.indices_objeto(r))
        if not self.objeto and not m.referido_por_indice(r):
            indexables = []
        if self.objeto or indexables:
            w.l("")
            if indexables:
                w.l("fn index<'x>(&'x self, i: &mut IndexBuilder<'x>) {")
                w.l("match self {")
                for n, s, i, p in vs:
                    with w.pago(p and not self.pago):
                        if s and m.tiene_indice(s):
                            w.l(f"{r}::{n}(object) => {{")
                            if con_tipo:
                                w.l(f"i.typ({i});")
                            w.l("object.index(i);")
                            w.l("}")
                        else:
                            w.l(f"{r}::{n}" + ("(_)" if s else "") + " => {}")
                w.l("}")
                w.l("}")
            else:
                w.l("fn index<'x>(&'x self, _: &mut IndexBuilder<'x>) {}")
        w.l("}")
        w.l("")
        # Valor por defecto: la primera variante.
        n0, s0, _, _ = vs[0]
        self.cab(f"impl Default for {r} {{")
        w.l("fn default() -> Self {")
        w.l(f"{r}::{n0}(Default::default())" if s0 else f"{r}::{n0}")
        w.l("}")
        w.l("}")
        w.l("")
        if self.almacenado:
            self.cab(f"impl Pickle for {r} {{")
            w.l("fn pickle(&self, out: &mut Vec<u8>) {")
            w.l("match self {")
            def pk(n, s, i):
                w.l(f"{r}::{n}" + ("(inner)" if s else "") + " => {")
                w.l(f"{i}u16.pickle(out);")
                if s:
                    w.l("inner.pickle(out);")
                w.l("}")
            brazos(pk)
            w.l("}")
            w.l("}")
            w.l("")
            w.l("fn unpickle(stream: &mut crate::pickle::PickledStream<'_>) -> Option<Self> {")
            if self.objeto:
                w.l("if stream.version() > Self::VERSION {")
                w.l("return None;")
                w.l("}")
            w.l("match u16::unpickle(stream)? {")
            brazos(lambda n, s, i: w.l(f"{i} => Pickle::unpickle(stream).map({r}::{n}),"
                                       if s else f"{i} => Some({r}::{n}),"))
            for i, t in m.p["multiples"].get(r, {}).get("antiguos", []):
                # Variantes antiguas: se leen y se descartan.
                w.l(f"{i} => {{")
                w.l(f"let _: {t} = Pickle::unpickle(stream)?;")
                w.l("Some(Self::default())")
                w.l("}")
            w.l("_ => None,")
            w.l("}")
            w.l("}")
            w.l("}")
            w.l("")
        self.cab(f"impl IntoValue for {r} {{")
        w.l("fn into_value(self) -> JmapValue<'static> {")
        w.l("self.into_value_with(ValueOptions::default())")
        w.l("}")
        w.l("")
        w.l("fn into_value_with(self, opts: ValueOptions) -> JmapValue<'static> {" if hay_sub
            else "fn into_value_with(self, _: ValueOptions) -> JmapValue<'static> {")
        w.l("match self {")
        def iv(n, s, i):
            if s:
                w.l(f"{r}::{n}(obj) => {{")
                w.l("let mut obj = obj.into_value_with(opts);")
                w.l(f"obj.as_object_mut().unwrap().insert_unchecked(Property::Type, JmapValue::Str({cadena_rust(n)}.into()));")
                w.l("obj")
                w.l("}")
            else:
                w.l(f"{r}::{n} => {{")
                w.l("let mut obj = jmap_tools::Map::new();")
                w.l(f"obj.insert_unchecked(Property::Type, JmapValue::Str({cadena_rust(n)}.into()));")
                w.l("JmapValue::Object(obj)")
                w.l("}")
        brazos(iv)
        w.l("}")
        w.l("}")
        w.l("}")
        w.l("")
        self.cab(f"impl RegistryJsonPatch for {r} {{")
        w.l("fn patch<'x>(&mut self, pointer: JsonPointerPatch<'_>, value: JmapValue<'x>) -> PatchResult<'x> {")
        w.l("if !pointer.has_next() {")
        w.l("match object_type(&pointer, &value)? {")
        et = m.enum_tipo(k)
        brazos(lambda n, s, i: w.l(f"{et}::{n} => *self = {r}::{n}" + ("(Default::default())," if s else ",")))
        w.l("}")
        w.l("}")
        w.l("match self {")
        brazos(lambda n, s, i: w.l(f"{r}::{n}(inner) => inner.patch(pointer, value)," if s
                                   else f"{r}::{n} => pointer.assert_eof(),"))
        w.l("}")
        w.l("}")
        w.l("}")
        w.l("")
        self.cab(f"impl {r} {{")
        w.l(f"pub fn object_type(&self) -> {et} {{")
        w.l("match self {")
        brazos(lambda n, s, i: w.l(f"{r}::{n}" + ("(_)" if s else "") + f" => {et}::{n},"))
        w.l("}")
        w.l("}")
        if m.tiene_expresiones(k):
            w.l("")
            w.l("pub fn expression_ctxs(&self) -> Vec<ExpressionContext<'_>> {")
            w.l("match self {")
            con = [(n, s, i, p) for n, s, i, p in vs if s and m.tiene_expresiones("x:" + s)]
            for n, s, i, p in con:
                with w.pago(p and not self.pago):
                    w.l(f"{r}::{n}(obj) => obj.expression_ctxs(),")
            if len(con) < len(vs):
                w.l("_ => vec![],")
            w.l("}")
            w.l("}")
        w.l("}")
        w.l("")

    def cabecera_objeto(self):
        w, r = self.w, self.r
        info = self.m.info_objeto(r)
        flags = " | ".join(info["flags"]) or "0"
        w.l(f"const FLAGS: u64 = {flags};")
        w.l(f"const VERSION: u8 = {info['version']};")
        w.l(f"const OBJECT: ObjectType = ObjectType::{r};")
        w.l("")

    # --------------------------------------------------------------------------------------- simple

    def simple(self):
        m, w, k, r = self.m, self.w, self.k, self.r
        campos = m.campos(k)
        self.campos = campos
        indice = m.indice_struct(k, campos)
        if self.objeto:
            self.cab(f"impl ObjectImpl for {r} {{")
            self.cabecera_objeto()
            self.validate(campos)
            w.l("")
            self.index(indice, True)
            w.l("}")
            w.l("")
        else:
            self.cab(f"impl {r} {{")
            self.validate(campos)
            if indice and m.referido_por_indice(r):
                w.l("")
                self.index(indice, False)
            w.l("}")
            w.l("")
        ctx = m.contextos(k, campos)
        if ctx:
            self.cab(f"impl {r} {{")
            for c, (var, cons) in ctx:
                self.contexto(c, var, cons)
                w.l("")
            w.l("pub fn expression_ctxs(&self) -> Vec<ExpressionContext<'_>> {")
            w.l("vec![")
            for c, _ in ctx:
                with self.pago_campo(c):
                    w.l(f"self.ctx_{c['campo']}(),")
            w.l("]")
            w.l("}")
            w.l("}")
            w.l("")
        if self.almacenado:
            self.pickle(campos)
        self.cab(f"impl Default for {r} {{")
        w.l("fn default() -> Self {")
        w.l("Self {")
        for c in campos:
            with self.pago_campo(c):
                w.l(f"{c['campo']}: {m.valor_defecto(k, c)},")
        w.l("}")
        w.l("}")
        w.l("}")
        w.l("")
        self.into_value(campos)
        self.patch(campos)

    def validate(self, campos):
        w = self.w
        cuerpo = []
        for c in campos:
            lineas = self.m.validacion(self.k, c)
            if lineas:
                cuerpo.append((c, lineas))
        if not cuerpo:
            w.l("fn validate(&self, _: &mut Vec<ValidationError>) -> bool {")
            w.l("true")
            w.l("}")
            return
        w.l("fn validate(&self, errors: &mut Vec<ValidationError>) -> bool {")
        w.l("let neb = errors.len();")
        for c, lineas in cuerpo:
            with self.w.pago(c["pago"] and not self.pago, bloque=len([l for l in lineas if l.endswith(";") or l.endswith("{")]) > 1):
                for l in lineas:
                    w.l(l)
        w.l("errors.len() == neb")
        w.l("}")

    def index(self, indice, objeto):
        w = self.w
        if not indice:
            w.l("fn index<'x>(&'x self, _: &mut IndexBuilder<'x>) {}")
            return
        if all(p for p, _ in indice) and not self.pago:
            w.l(f'#[cfg_attr(not(feature = "{self.m.feature}"), allow(unused_variables))]')
        w.l("fn index<'x>(&'x self, i: &mut IndexBuilder<'x>) {")
        for p, lineas in indice:
            with self.w.pago(p and not self.pago, bloque=True):
                for l in lineas:
                    w.l(l)
        w.l("}")

    def contexto(self, c, var, cons):
        w, m = self.w, self.m
        with self.pago_campo(c):
            w.l(f"pub fn ctx_{c['campo']}(&self) -> ExpressionContext<'_> {{")
            w.l("ExpressionContext {")
            w.l(f"expr: &self.{c['campo']},")
            d = m.defecto_esquema(self.k, c)
            w.l(f"default: Some({m.valor_rust(c['prop']['type'], d, c['tipo'])}),"
                if d is not None else "default: None,")
            w.l(f"property: Property::{propiedad(c['json'])},")
            w.l(f"allowed_variables: {var},")
            w.l(f"allowed_constants: {cons},")
            w.l("}")
            w.l("}")

    def pickle(self, campos):
        m, w, r = self.m, self.w, self.r
        orden = sorted([c for c in campos if "idx" in c], key=lambda c: c["idx"])
        nf, nb = m.nfields(self.k, campos), m.nflags(self.k, campos)
        self.cab(f"impl Pickle for {r} {{")
        w.l("fn pickle(&self, out: &mut Vec<u8>) {")
        con_bits = [c for c in campos if "bit" in c]
        if nb:
            if con_bits and all(c["pago"] for c in con_bits) and not self.pago:
                w.l(f'#[cfg_attr(not(feature = "{m.feature}"), allow(unused_mut))]')
            w.l("let mut bitmap = 0u64;")
            for c in sorted(con_bits, key=lambda c: c["bit"]):
                with self.pago_campo(c):
                    cond = f"self.{c['campo']}" if c["tipo"] == "bool" else f"self.{c['campo']}.is_some()"
                    w.l(f"if {cond} {{")
                    w.l(f"bitmap |= 1 << {c['bit']};")
                    w.l("}")
            w.l(f"let block = crate::pickle::begin_block(out, {nf}, {nb}, bitmap);")
        else:
            w.l(f"let block = crate::pickle::begin_block(out, {nf}, 0, 0);")
        for c in orden:
            if c["tipo"] == "bool":
                continue
            relleno = c["pago"] and not self.pago and not opcional(c["tipo"])
            with self.pago_campo(c):
                if opcional(c["tipo"]):
                    w.l(f"if let Some(value) = &self.{c['campo']} {{")
                    w.l("value.pickle(out);")
                    w.l("}")
                elif relleno:
                    # Campo de pago obligatorio: va en su propio bloque, para que sin la feature se pueda saltar.
                    w.l("{")
                    w.l("let block = crate::pickle::begin_block(out, 0, 0, 0);")
                    w.l(f"self.{c['campo']}.pickle(out);")
                    w.l("crate::pickle::end_block(out, block);")
                    w.l("}")
                else:
                    w.l(f"self.{c['campo']}.pickle(out);")
            if relleno:
                w.l(self.m.cfg_no)
                w.l("{")
                w.l("let block = crate::pickle::begin_block(out, 0, 0, 0);")
                w.l("crate::pickle::end_block(out, block);")
                w.l("}")
        w.l("crate::pickle::end_block(out, block);")
        w.l("}")
        w.l("")
        w.l("fn unpickle(stream: &mut crate::pickle::PickledStream<'_>) -> Option<Self> {")
        if self.objeto:
            w.l("if stream.version() > Self::VERSION {")
            w.l("return None;")
            w.l("}")
        w.l("let block = stream.open_block()?;")
        for c in orden:
            n = c["idx"]
            defecto = m.valor_defecto(self.k, c)
            relleno = c["pago"] and not self.pago and not opcional(c["tipo"]) and c["tipo"] != "bool"
            with self.pago_campo(c):
                if c["tipo"] == "bool":
                    w.l(f"let field{n}: bool = if block.has({n}) {{ block.flag({c['bit']}) }} else {{ {defecto} }};")
                elif opcional(c["tipo"]):
                    w.l(f"let field{n}: {c['tipo']} = if block.has({n}) {{")
                    w.l(f"if block.flag({c['bit']}) {{")
                    w.l("Some(Pickle::unpickle(stream)?)")
                    w.l("} else {")
                    w.l("None")
                    w.l("}")
                    w.l("} else {")
                    w.l(f"{defecto}")
                    w.l("};")
                elif relleno:
                    w.l(f"let field{n}: {c['tipo']} = if block.has({n}) {{")
                    w.l("let block = stream.open_block()?;")
                    w.l("let value = Pickle::unpickle(stream)?;")
                    w.l("stream.close_block(block);")
                    w.l("value")
                    w.l("} else {")
                    w.l(f"{defecto}")
                    w.l("};")
                else:
                    w.l(f"let field{n}: {c['tipo']} = if block.has({n}) {{")
                    w.l("Pickle::unpickle(stream)?")
                    w.l("} else {")
                    w.l(f"{defecto}")
                    w.l("};")
            if relleno:
                w.l(self.m.cfg_no)
                w.l(f"if block.has({n}) {{")
                w.l("let block = stream.open_block()?;")
                w.l("stream.close_block(block);")
                w.l("}")
        w.l("stream.close_block(block);")
        w.l("Some(Self {")
        for c in orden:
            with self.pago_campo(c):
                w.l(f"{c['campo']}: field{c['idx']},")
        w.l("})")
        w.l("}")
        w.l("}")
        w.l("")

    def into_value(self, campos):
        w, r = self.w, self.r
        self.cab(f"impl IntoValue for {r} {{")
        w.l("fn into_value(self) -> JmapValue<'static> {")
        w.l("self.into_value_with(ValueOptions::default())")
        w.l("}")
        w.l("")
        w.l("fn into_value_with(self, opts: ValueOptions) -> JmapValue<'static> {")
        w.l(f"let mut map = jmap_tools::Map::with_capacity({len(campos) + 2});")
        for c in campos:
            p = propiedad(c["json"])
            with self.pago_campo(c):
                if self.m.es_secreto(c, self.k):
                    if opcional(c["tipo"]):
                        w.l(f"if self.{c['campo']}.is_some() {{")
                    w.l(f"map.insert_unchecked(Property::{p}, if opts.include_secrets {{ "
                        f"self.{c['campo']}.into_value_with(opts) }} else {{ JmapValue::Str(MASKED_PASSWORD.into()) }});")
                    if opcional(c["tipo"]):
                        w.l("}")
                else:
                    w.l(f"map.insert_unchecked(Property::{p}, self.{c['campo']}.into_value_with(opts));")
        w.l("JmapValue::Object(map)")
        w.l("}")
        w.l("}")
        w.l("")

    def patch(self, campos):
        m, w, r = self.m, self.w, self.r
        self.cab(f"impl RegistryJsonPropertyPatch for {r} {{")
        w.l("fn patch_property<'x>(&mut self, mut pointer: JsonPointerPatch<'_>, value: JmapValue<'x>) -> PatchResult<'x> {")
        w.l("match pointer.next_property() {")
        for d in m.orden_parche(self.k, campos):
            with self.w.pago(d["pago"] and not self.pago):
                w.l(d["codigo"])
        w.l("Some(Property::Type) => Ok(MaybeUnpatched::Unpatched { property: Property::Type, value, }),")
        w.l('_ => Err(PatchError::new(pointer, "Invalid property")),')
        w.l("}")
        w.l("}")
        w.l("}")
        w.l("")
