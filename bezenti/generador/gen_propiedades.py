# SPDX-FileCopyrightText: 2026 bezenti
#
# SPDX-License-Identifier: AGPL-3.0-only
"""Genera `properties.rs` y `properties_impl.rs`: `ObjectInner`, `ObjectType`, `Property` y el despacho
por tipo de objeto."""

import gen_enums


def properties_rs(m, w):
    w.l("use crate::schema::prelude::*;")
    objs = m.objetos()
    w.l("#[derive(Debug, Clone, Serialize, PartialEq, Eq)]")
    w.l("#[serde(untagged)]")
    w.l("pub enum ObjectInner {")
    for _, r, _, p in objs:
        with w.pago(p):
            w.l(f"{r}(Box<{r}>),")
    w.l("}")
    w.l("")
    for nombre, vs in (("ObjectType", objs), ("Property", m.propiedades())):
        w.l("#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]")
        w.l("#[repr(u16)]")
        w.l(f"pub enum {nombre} {{")
        for _, r, i, p in vs:
            with w.pago(p):
                w.l(f"{r} = {i},")
        w.l("}")
        w.l("")


def properties_impl_rs(m, w):
    w.l("use crate::schema::prelude::*;")
    w.l("")
    objs = [(r, r, i, p) for _, r, i, p in m.objetos()]
    gen_enums.emitir_impl(m, w, "ObjectType", objs, False, False, m.count_de("ObjectType"))
    gen_enums.emitir_impl(m, w, "Property", m.propiedades(), False, False, m.count_de("Property"))
    impl_object_type(m, w)
    impl_object_inner(m, w)
    impl_object(m, w)
    impl_conversiones(m, w)


def brazo(m, w, pago, texto):
    """Escribe un brazo de `match` (o elemento) con su cfg si es de pago."""
    with w.pago(pago):
        w.l(texto)


def impl_object_type(m, w):
    objs = m.objetos()
    w.l("impl ObjectType {")
    w.l("pub fn flags(&self) -> u64 {")
    w.l("match self {")
    for _, r, _, p in objs:
        brazo(m, w, p, f"ObjectType::{r} => {r}::FLAGS,")
    w.l("}")
    w.l("}")
    w.l("")
    w.l("pub fn indexes(&self) -> Vec<IndexSchema> {")
    w.l("match self {")
    for _, r, _, p in objs:
        indices = m.indices_objeto(r)
        if not indices:
            continue
        with w.pago(p):
            w.l(f"ObjectType::{r} => vec![")
            for prop, tipo, valor, pi in indices:
                brazo(m, w, pi and not p,
                      f"IndexSchema::new(Property::{prop}, IndexSchemaType::{tipo}, IndexSchemaValueType::{valor}),")
            w.l("],")
    w.l("_ => vec![],")
    w.l("}")
    w.l("}")
    w.l("")
    for accion in ("Get", "Query", "Create", "Update", "Destroy"):
        w.l(f"pub fn {accion.lower()}_permission(&self) -> Permission {{")
        w.l("match self {")
        for k, r, _, p in objs:
            permiso = m.permiso(k, accion)
            if accion == "Query":
                if permiso:
                    brazo(m, w, p, f"ObjectType::{r} => Permission::{permiso},")
            else:
                brazo(m, w, p, f"ObjectType::{r} => " + (f"Permission::{permiso}," if permiso else "unreachable!(),"))
        if accion == "Query":
            w.l("_ => unreachable!(),")
        w.l("}")
        w.l("}")
        w.l("")
    w.lineas.pop()
    w.l("}")
    w.l("")


def impl_object_inner(m, w):
    objs = m.objetos()
    w.l("impl ObjectInner {")
    for nombre, acceso, mut in (("account_id", "as_ref", False), ("set_account_id", "as_mut", True)):
        if mut:
            w.l("pub fn set_account_id(&mut self, id: Id) {")
        else:
            w.l("pub fn account_id(&self) -> Option<Id> {")
        w.l("match self {")
        for k, r, _, p in objs:
            esq = m.e["schemas"][k]
            if esq["type"] == "single":
                op = m.campo_cuenta(esq["schemaName"])
                if op is None:
                    continue
                brazo(m, w, p, f"ObjectInner::{r}(obj) => {cuenta(op, mut)},")
            else:
                brazos = []
                total = 0
                for v in esq["variants"]:
                    total += 1
                    if "schemaName" not in v:
                        continue
                    vp = m.variante_pago(k, v)
                    op = m.campo_cuenta(v["schemaName"])
                    if op is not None:
                        brazos.append((vp and not p, f"{r}::{v['name']}(obj) => {cuenta(op, mut)},"))
                if not brazos:
                    continue
                with w.pago(p):
                    w.l(f"ObjectInner::{r}(inner) => match inner.{acceso}() {{")
                    for vp, t in brazos:
                        brazo(m, w, vp, t)
                    if len(brazos) < total:
                        w.l("_ => {}" if mut else "_ => None,")
                    w.l("},")
        w.l("_ => {}" if mut else "_ => None,")
        w.l("}")
        w.l("}")
        w.l("")
    w.l("pub fn to_pickled_vec(&self) -> Vec<u8> {")
    w.l("match &self {")
    for k, r, _, p in objs:
        if m.no_almacenado(r):
            brazo(m, w, p, f'ObjectInner::{r}(_) => unimplemented!("{r} is not stored"),')
        else:
            brazo(m, w, p, f"ObjectInner::{r}(obj) => obj.to_pickled_vec(),")
    w.l("}")
    w.l("}")
    w.l("")
    w.l("pub fn unpickle(object: ObjectType, stream: &mut crate::pickle::PickledStream<'_>) -> Option<Self> {")
    w.l("match object {")
    for k, r, _, p in objs:
        if m.no_almacenado(r):
            brazo(m, w, p, f'ObjectType::{r} => unimplemented!("{r} is not stored"),')
        else:
            brazo(m, w, p, f"ObjectType::{r} => Pickle::unpickle(stream).map(|obj| ObjectInner::{r}(Box::new(obj))),")
    w.l("}")
    w.l("}")
    w.l("")
    w.l("pub fn deserialize<'de, D>(object: ObjectType, deserializer: D) -> Result<Self, D::Error>")
    w.l("where")
    w.l("D: serde::Deserializer<'de>,")
    w.l("{")
    w.l("match object {")
    for k, r, _, p in objs:
        brazo(m, w, p, f"ObjectType::{r} => {r}::deserialize(deserializer).map(|obj| ObjectInner::{r}(Box::new(obj))),")
    w.l("}")
    w.l("}")
    w.l("")
    w.l("pub fn expression_ctxs(&self) -> Option<Vec<ExpressionContext<'_>>> {")
    w.l("match &self {")
    for k, r, _, p in objs:
        if m.tiene_expresiones(k):
            brazo(m, w, p, f"ObjectInner::{r}(obj) => Some(obj.expression_ctxs()),")
    w.l("_ => None,")
    w.l("}")
    w.l("}")
    w.l("}")
    w.l("")


def cuenta(op, mut):
    campo, nulable = op
    if mut:
        return f"obj.{campo} = Some(id)" if nulable else f"obj.{campo} = id"
    return f"obj.{campo}" if nulable else f"Some(obj.{campo})"


def impl_object(m, w):
    objs = m.objetos()
    w.l("impl Object {")
    w.l("pub fn flags(&self) -> u64 {")
    w.l("match &self.inner {")
    for _, r, _, p in objs:
        brazo(m, w, p, f"ObjectInner::{r}(_) => {r}::FLAGS,")
    w.l("}")
    w.l("}")
    w.l("")
    w.l("pub fn object_type(&self) -> ObjectType {")
    w.l("match &self.inner {")
    for _, r, _, p in objs:
        brazo(m, w, p, f"ObjectInner::{r}(_) => ObjectType::{r},")
    w.l("}")
    w.l("}")
    w.l("")
    w.l("pub fn object_variant(&self) -> Option<&'static str> {")
    w.l("match &self.inner {")
    for k, r, _, p in objs:
        if m.e["schemas"][k]["type"] == "multiple":
            brazo(m, w, p, f"ObjectInner::{r}(obj) => Some(obj.object_type().as_str()),")
    w.l("_ => None,")
    w.l("}")
    w.l("}")
    w.l("")
    w.l("pub fn validate(&self, errors: &mut Vec<ValidationError>) -> bool {")
    w.l("match &self.inner {")
    for _, r, _, p in objs:
        brazo(m, w, p, f"ObjectInner::{r}(obj) => obj.validate(errors),")
    w.l("}")
    w.l("}")
    w.l("")
    w.l("pub fn index<'x>(&'x self, i: &mut IndexBuilder<'x>) {")
    w.l("match &self.inner {")
    for _, r, _, p in objs:
        brazo(m, w, p, f"ObjectInner::{r}(obj) => obj.index(i),")
    w.l("}")
    w.l("}")
    w.l("")
    w.l("pub fn patch<'x>(&mut self, pointer: JsonPointerPatch<'_>, value: JmapValue<'x>) -> PatchResult<'x> {")
    w.l("match &mut self.inner {")
    for _, r, _, p in objs:
        brazo(m, w, p, f"ObjectInner::{r}(obj) => obj.patch(pointer, value),")
    w.l("}")
    w.l("}")
    w.l("}")
    w.l("")
    w.l("impl IntoValue for Object {")
    w.l("fn into_value(self) -> JmapValue<'static> {")
    w.l("self.into_value_with(ValueOptions::default())")
    w.l("}")
    w.l("")
    w.l("fn into_value_with(self, opts: ValueOptions) -> JmapValue<'static> {")
    w.l("match self.inner {")
    for _, r, _, p in objs:
        brazo(m, w, p, f"ObjectInner::{r}(obj) => (*obj).into_value_with(opts),")
    w.l("}")
    w.l("}")
    w.l("}")
    w.l("")


def impl_conversiones(m, w):
    objs = m.objetos()
    w.l("impl From<ObjectType> for ObjectInner {")
    w.l("fn from(obj: ObjectType) -> Self {")
    w.l("match obj {")
    for _, r, _, p in objs:
        brazo(m, w, p, f"ObjectType::{r} => ObjectInner::{r}(Box::default()),")
    w.l("}")
    w.l("}")
    w.l("}")
    w.l("")
    for _, r, _, p in objs:
        with w.pago(p, attr=None):
            w.l(m.cfg) if p and m.bezenti else None
            w.l(f"impl From<{r}> for ObjectInner {{")
            w.l(f"fn from(value: {r}) -> Self {{")
            w.l(f"ObjectInner::{r}(Box::new(value))")
            w.l("}")
            w.l("}")
            w.l("")
            w.l(m.cfg) if p and m.bezenti else None
            w.l(f"impl From<Object> for {r} {{")
            w.l("fn from(obj: Object) -> Self {")
            w.l("match obj.inner {")
            w.l(f"ObjectInner::{r}(obj) => *obj,")
            w.l("_ => unreachable!(),")
            w.l("}")
            w.l("}")
            w.l("}")
            w.l("")
