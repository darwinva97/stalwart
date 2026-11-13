/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Filtros SCIM (RFC 7644 §3.4.2.2) y rutas de PATCH (RFC 7644 §3.5.2), escritos desde el RFC.
//!
//! - [`Filtro::parse`]: gramática completa: `eq ne co sw ew pr gt ge lt le`, `and`, `or`, `not`, paréntesis y
//!   filtros de valor `atributo[filtro]`. Palabras clave sin distinguir mayúsculas.
//! - [`Filtro::evalua`]: evaluación sobre la representación JSON de un recurso. Los nombres de atributo no
//!   distinguen mayúsculas (RFC 7643 §2.1); las cadenas se comparan sin distinguir mayúsculas salvo los atributos
//!   `caseExact` (ver [`es_case_exact`]).
//! - [`RutaPatch::parse`]: `atributo`, `atributo.sub`, `atributo[filtro]`, `atributo[filtro].sub`, con o sin el
//!   prefijo URI del esquema.
//!
//! Especificación: `bezenti/especificaciones/scim.md`, §6 y §8.

use serde_json::Value;
use std::{fmt, str::FromStr};

/// Ruta de un atributo: `[uri:]nombre[.sub]`. `nombre` y `sub` van en minúsculas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RutaAtributo {
    pub uri: Option<String>,
    pub nombre: String,
    pub sub: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Co,
    Sw,
    Ew,
    Gt,
    Ge,
    Lt,
    Le,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Filtro {
    Comparacion {
        ruta: RutaAtributo,
        op: Op,
        valor: Value,
    },
    Presente(RutaAtributo),
    Y(Box<Filtro>, Box<Filtro>),
    O(Box<Filtro>, Box<Filtro>),
    No(Box<Filtro>),
    /// `atributo[filtro]`: algún elemento del atributo multivalor cumple el filtro (relativo al elemento).
    Valor {
        atributo: RutaAtributo,
        filtro: Box<Filtro>,
    },
}

/// Ruta de una operación PATCH.
#[derive(Debug, Clone, PartialEq)]
pub struct RutaPatch {
    /// Atributo de primer nivel (`sub` siempre `None` aquí).
    pub atributo: RutaAtributo,
    pub filtro: Option<Filtro>,
    /// Subatributo (`atributo.sub` o `atributo[filtro].sub`).
    pub sub: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorFiltro(pub String);

impl fmt::Display for ErrorFiltro {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl RutaAtributo {
    /// Interpreta `urn:…:Esquema:nombre.sub` o `nombre.sub`.
    pub fn parse(texto: &str) -> Result<Self, ErrorFiltro> {
        let (uri, resto) = match texto.rfind(':') {
            Some(pos) => (Some(texto[..pos].to_string()), &texto[pos + 1..]),
            None => (None, texto),
        };
        let (nombre, sub) = match resto.split_once('.') {
            Some((nombre, sub)) => (nombre, Some(sub)),
            None => (resto, None),
        };
        let valido = |s: &str| {
            !s.is_empty()
                && s.chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '$')
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '$')
        };
        if !valido(nombre) || sub.is_some_and(|s| !valido(s)) {
            return Err(ErrorFiltro(format!("Invalid attribute path '{texto}'")));
        }
        if uri
            .as_deref()
            .is_some_and(|u| !u.to_ascii_lowercase().starts_with("urn:"))
        {
            return Err(ErrorFiltro(format!("Invalid schema URI in '{texto}'")));
        }
        Ok(RutaAtributo {
            uri,
            nombre: nombre.to_ascii_lowercase(),
            sub: sub.map(|s| s.to_ascii_lowercase()),
        })
    }

    pub fn simple(nombre: &str) -> Self {
        RutaAtributo {
            uri: None,
            nombre: nombre.to_ascii_lowercase(),
            sub: None,
        }
    }

    /// `nombre` o `nombre.sub`, en minúsculas, sin URI.
    pub fn clave(&self) -> String {
        match &self.sub {
            Some(sub) => format!("{}.{}", self.nombre, sub),
            None => self.nombre.clone(),
        }
    }
}

impl Op {
    fn parse(texto: &str) -> Option<Op> {
        Some(match texto.to_ascii_lowercase().as_str() {
            "eq" => Op::Eq,
            "ne" => Op::Ne,
            "co" => Op::Co,
            "sw" => Op::Sw,
            "ew" => Op::Ew,
            "gt" => Op::Gt,
            "ge" => Op::Ge,
            "lt" => Op::Lt,
            "le" => Op::Le,
            _ => return None,
        })
    }
}

/// ¿Distingue mayúsculas el atributo (`caseExact` en RFC 7643 y en nuestro esquema)?
pub fn es_case_exact(clave: &str) -> bool {
    matches!(
        clave,
        "id" | "externalid" | "members.value" | "groups.value" | "meta.version"
    )
}

// ---------------------------------------------------------------------------------------------------------------
// Analizador

struct Analizador<'x> {
    texto: &'x str,
    pos: usize,
}

impl<'x> Analizador<'x> {
    fn error<T>(&self, que: &str) -> Result<T, ErrorFiltro> {
        Err(ErrorFiltro(format!("{que} at position {}", self.pos + 1)))
    }

    fn salta_espacios(&mut self) {
        while self.texto[self.pos..].starts_with([' ', '\t', '\r', '\n']) {
            self.pos += 1;
        }
    }

    fn fin(&self) -> bool {
        self.pos >= self.texto.len()
    }

    fn mira(&self) -> Option<char> {
        self.texto[self.pos..].chars().next()
    }

    /// Palabra: caracteres de una ruta de atributo o de una palabra clave.
    fn palabra(&mut self) -> &'x str {
        let inicio = self.pos;
        while let Some(c) = self.mira() {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '$' | ':' | '.') {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
        &self.texto[inicio..self.pos]
    }

    /// ¿Viene la palabra clave `kw` (seguida de un separador)? Si es así, la consume.
    fn palabra_clave(&mut self, kw: &str) -> bool {
        let resto = &self.texto[self.pos..];
        if resto.len() >= kw.len()
            && resto[..kw.len()].eq_ignore_ascii_case(kw)
            && resto[kw.len()..]
                .chars()
                .next()
                .is_none_or(|c| matches!(c, ' ' | '\t' | '(' | '\r' | '\n'))
        {
            self.pos += kw.len();
            true
        } else {
            false
        }
    }

    fn filtro_o(&mut self) -> Result<Filtro, ErrorFiltro> {
        let mut izquierda = self.filtro_y()?;
        loop {
            self.salta_espacios();
            let guardado = self.pos;
            if self.palabra_clave("or") {
                self.salta_espacios();
                let derecha = self.filtro_y()?;
                izquierda = Filtro::O(Box::new(izquierda), Box::new(derecha));
            } else {
                self.pos = guardado;
                return Ok(izquierda);
            }
        }
    }

    fn filtro_y(&mut self) -> Result<Filtro, ErrorFiltro> {
        let mut izquierda = self.filtro_unario()?;
        loop {
            self.salta_espacios();
            let guardado = self.pos;
            if self.palabra_clave("and") {
                self.salta_espacios();
                let derecha = self.filtro_unario()?;
                izquierda = Filtro::Y(Box::new(izquierda), Box::new(derecha));
            } else {
                self.pos = guardado;
                return Ok(izquierda);
            }
        }
    }

    fn filtro_unario(&mut self) -> Result<Filtro, ErrorFiltro> {
        self.salta_espacios();
        if self.palabra_clave("not") {
            self.salta_espacios();
            if self.mira() != Some('(') {
                return self.error("Expected '(' after 'not'");
            }
            let interno = self.parentesis()?;
            return Ok(Filtro::No(Box::new(interno)));
        }
        if self.mira() == Some('(') {
            return self.parentesis();
        }
        self.atomo()
    }

    fn parentesis(&mut self) -> Result<Filtro, ErrorFiltro> {
        self.pos += 1; // '('
        let interno = self.filtro_o()?;
        self.salta_espacios();
        if self.mira() != Some(')') {
            return self.error("Expected ')'");
        }
        self.pos += 1;
        Ok(interno)
    }

    fn atomo(&mut self) -> Result<Filtro, ErrorFiltro> {
        let inicio = self.pos;
        let texto_ruta = self.palabra();
        if texto_ruta.is_empty() {
            return self.error("Expected an attribute name");
        }
        let ruta = RutaAtributo::parse(texto_ruta)
            .map_err(|e| ErrorFiltro(format!("{} at position {}", e.0, inicio + 1)))?;

        if self.mira() == Some('[') {
            if ruta.sub.is_some() {
                return self.error("A value filter cannot follow a sub-attribute");
            }
            self.pos += 1;
            let interno = self.filtro_o()?;
            self.salta_espacios();
            if self.mira() != Some(']') {
                return self.error("Expected ']'");
            }
            self.pos += 1;
            return Ok(Filtro::Valor {
                atributo: ruta,
                filtro: Box::new(interno),
            });
        }

        self.salta_espacios();
        let inicio_op = self.pos;
        let op = self.palabra();
        if op.eq_ignore_ascii_case("pr") {
            return Ok(Filtro::Presente(ruta));
        }
        let Some(op) = Op::parse(op) else {
            self.pos = inicio_op;
            return self
                .error("Expected a comparison operator (eq, ne, co, sw, ew, gt, ge, lt, le, pr)");
        };
        self.salta_espacios();
        let valor = self.valor()?;
        Ok(Filtro::Comparacion { ruta, op, valor })
    }

    fn valor(&mut self) -> Result<Value, ErrorFiltro> {
        match self.mira() {
            Some('"') => {
                let inicio = self.pos;
                self.pos += 1;
                let mut escapado = false;
                loop {
                    let Some(c) = self.mira() else {
                        return self.error("Unterminated string");
                    };
                    self.pos += c.len_utf8();
                    if escapado {
                        escapado = false;
                    } else if c == '\\' {
                        escapado = true;
                    } else if c == '"' {
                        break;
                    }
                }
                serde_json::from_str::<Value>(&self.texto[inicio..self.pos])
                    .or_else(|_| self.error("Invalid string"))
            }
            Some(c) if c == '-' || c.is_ascii_digit() => {
                let inicio = self.pos;
                while let Some(c) = self.mira() {
                    if c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E') {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                serde_json::from_str::<Value>(&self.texto[inicio..self.pos])
                    .or_else(|_| self.error("Invalid number"))
            }
            _ => {
                let palabra = self.palabra();
                match palabra.to_ascii_lowercase().as_str() {
                    "true" => Ok(Value::Bool(true)),
                    "false" => Ok(Value::Bool(false)),
                    "null" => Ok(Value::Null),
                    _ => self.error("Expected a value (string, number, true, false or null)"),
                }
            }
        }
    }
}

impl Filtro {
    pub fn parse(texto: &str) -> Result<Filtro, ErrorFiltro> {
        let mut analizador = Analizador { texto, pos: 0 };
        analizador.salta_espacios();
        if analizador.fin() {
            return Err(ErrorFiltro("Empty filter".to_string()));
        }
        let filtro = analizador.filtro_o()?;
        analizador.salta_espacios();
        if !analizador.fin() {
            return analizador.error("Unexpected text");
        }
        Ok(filtro)
    }

    /// Todas las rutas que usa el filtro, como `nombre` o `nombre.sub` (los filtros de valor dan `atributo.sub`).
    pub fn rutas(&self) -> Vec<RutaAtributo> {
        let mut rutas = Vec::new();
        self.recoge_rutas(None, &mut rutas);
        rutas
    }

    fn recoge_rutas(&self, padre: Option<&RutaAtributo>, rutas: &mut Vec<RutaAtributo>) {
        let ajusta = |ruta: &RutaAtributo| match padre {
            Some(padre) => RutaAtributo {
                uri: padre.uri.clone(),
                nombre: padre.nombre.clone(),
                sub: Some(ruta.nombre.clone()),
            },
            None => ruta.clone(),
        };
        match self {
            Filtro::Comparacion { ruta, .. } | Filtro::Presente(ruta) => rutas.push(ajusta(ruta)),
            Filtro::Y(a, b) | Filtro::O(a, b) => {
                a.recoge_rutas(padre, rutas);
                b.recoge_rutas(padre, rutas);
            }
            Filtro::No(a) => a.recoge_rutas(padre, rutas),
            Filtro::Valor { atributo, filtro } => {
                let atributo = ajusta(atributo);
                filtro.recoge_rutas(Some(&atributo), rutas);
            }
        }
    }

    /// ¿Cumple el recurso (u objeto) el filtro?
    pub fn evalua(&self, recurso: &Value) -> bool {
        self.evalua_en(recurso, None)
    }

    fn evalua_en(&self, objeto: &Value, padre: Option<&str>) -> bool {
        match self {
            Filtro::Y(a, b) => a.evalua_en(objeto, padre) && b.evalua_en(objeto, padre),
            Filtro::O(a, b) => a.evalua_en(objeto, padre) || b.evalua_en(objeto, padre),
            Filtro::No(a) => !a.evalua_en(objeto, padre),
            Filtro::Presente(ruta) => valores(objeto, ruta).iter().any(|v| presente(v)),
            Filtro::Comparacion { ruta, op, valor } => {
                let clave = match padre {
                    Some(padre) => format!("{padre}.{}", ruta.nombre),
                    None => ruta.clave(),
                };
                let exacto = es_case_exact(&clave);
                valores(objeto, ruta)
                    .iter()
                    .any(|v| compara(v, *op, valor, exacto))
            }
            Filtro::Valor { atributo, filtro } => match busca(objeto, atributo) {
                Some(Value::Array(elementos)) => elementos
                    .iter()
                    .any(|e| filtro.evalua_en(e, Some(&atributo.nombre))),
                Some(elemento @ Value::Object(_)) => {
                    filtro.evalua_en(elemento, Some(&atributo.nombre))
                }
                _ => false,
            },
        }
    }

    /// Términos `eq` de la conjunción de primer nivel (para buscar candidatos por índice).
    pub fn igualdades(&self) -> Vec<(String, &Value)> {
        let mut out = Vec::new();
        self.recoge_igualdades(&mut out);
        out
    }

    fn recoge_igualdades<'x>(&'x self, out: &mut Vec<(String, &'x Value)>) {
        match self {
            Filtro::Y(a, b) => {
                a.recoge_igualdades(out);
                b.recoge_igualdades(out);
            }
            Filtro::Comparacion {
                ruta,
                op: Op::Eq,
                valor,
            } => out.push((ruta.clave(), valor)),
            Filtro::Valor { atributo, filtro } => {
                // `emails[value eq "x"]` equivale a `emails.value eq "x"`.
                let mut internas = Vec::new();
                filtro.recoge_igualdades(&mut internas);
                for (clave, valor) in internas {
                    if !clave.contains('.') {
                        out.push((format!("{}.{clave}", atributo.nombre), valor));
                    }
                }
            }
            _ => {}
        }
    }
}

impl FromStr for Filtro {
    type Err = ErrorFiltro;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Filtro::parse(s)
    }
}

impl RutaPatch {
    pub fn parse(texto: &str) -> Result<RutaPatch, ErrorFiltro> {
        let texto = texto.trim();
        let Some(corchete) = texto.find('[') else {
            let ruta = RutaAtributo::parse(texto)?;
            return Ok(RutaPatch {
                atributo: RutaAtributo {
                    uri: ruta.uri,
                    nombre: ruta.nombre,
                    sub: None,
                },
                filtro: None,
                sub: ruta.sub,
            });
        };
        let atributo = RutaAtributo::parse(&texto[..corchete])?;
        if atributo.sub.is_some() {
            return Err(ErrorFiltro(format!(
                "A value filter cannot follow a sub-attribute in '{texto}'"
            )));
        }
        let cierre = texto
            .rfind(']')
            .filter(|cierre| *cierre > corchete)
            .ok_or_else(|| ErrorFiltro(format!("Missing ']' in '{texto}'")))?;
        let filtro = Filtro::parse(&texto[corchete + 1..cierre])?;
        let resto = &texto[cierre + 1..];
        let sub = if resto.is_empty() {
            None
        } else if let Some(sub) = resto.strip_prefix('.') {
            let sub_ruta = RutaAtributo::parse(sub)?;
            if sub_ruta.sub.is_some() || sub_ruta.uri.is_some() {
                return Err(ErrorFiltro(format!("Invalid sub-attribute in '{texto}'")));
            }
            Some(sub_ruta.nombre)
        } else {
            return Err(ErrorFiltro(format!(
                "Unexpected text after ']' in '{texto}'"
            )));
        };
        Ok(RutaPatch {
            atributo,
            filtro: Some(filtro),
            sub,
        })
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Evaluación

/// Busca un atributo de primer nivel sin distinguir mayúsculas (con o sin URI de extensión).
pub fn busca<'x>(objeto: &'x Value, ruta: &RutaAtributo) -> Option<&'x Value> {
    let mapa = objeto.as_object()?;
    if let Some(uri) = &ruta.uri
        && let Some((_, extension)) = mapa.iter().find(|(k, _)| k.eq_ignore_ascii_case(uri))
        && let Some((_, v)) = extension
            .as_object()
            .and_then(|e| e.iter().find(|(k, _)| k.eq_ignore_ascii_case(&ruta.nombre)))
    {
        return Some(v);
    }
    mapa.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(&ruta.nombre))
        .map(|(_, v)| v)
}

/// Busca un subatributo sin distinguir mayúsculas.
pub fn busca_sub<'x>(objeto: &'x Value, sub: &str) -> Option<&'x Value> {
    objeto
        .as_object()?
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(sub))
        .map(|(_, v)| v)
}

/// Valores de una ruta: para atributos multivalor, el de cada elemento (o su `value` si no hay subatributo).
fn valores<'x>(objeto: &'x Value, ruta: &RutaAtributo) -> Vec<&'x Value> {
    let Some(v) = busca(objeto, ruta) else {
        return Vec::new();
    };
    let elementos: Vec<&Value> = match v {
        Value::Array(lista) => lista.iter().collect(),
        v => vec![v],
    };
    elementos
        .into_iter()
        .filter_map(|e| match (&ruta.sub, e) {
            (Some(sub), Value::Object(_)) => busca_sub(e, sub),
            (Some(_), _) => None,
            (None, Value::Object(_)) => busca_sub(e, "value"),
            (None, e) => Some(e),
        })
        .collect()
}

fn presente(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        _ => true,
    }
}

/// Instante de una fecha RFC 3339 (`2026-11-13T10:00:00Z`, con fracción de segundo o desplazamiento).
fn instante(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.len() < 19
        || s.as_bytes()
            .get(10)
            .is_none_or(|b| *b != b'T' && *b != b't')
    {
        return None;
    }
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (
        num(0, 4)?,
        num(5, 7)?,
        num(8, 10)?,
        num(11, 13)?,
        num(14, 16)?,
        num(17, 19)?,
    );
    let mut resto = &s[19..];
    if let Some(fraccion) = resto.strip_prefix('.') {
        let fin = fraccion
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(fraccion.len());
        resto = &fraccion[fin..];
    }
    let desplazamiento = match resto {
        "Z" | "z" | "" => 0,
        r if r.len() == 6 && (r.starts_with('+') || r.starts_with('-')) => {
            let h: i64 = r[1..3].parse().ok()?;
            let m: i64 = r[4..6].parse().ok()?;
            let total = h * 3600 + m * 60;
            if r.starts_with('+') { total } else { -total }
        }
        _ => return None,
    };
    // Días desde 1970 (algoritmo de Howard Hinnant).
    let (y, mo) = if mo <= 2 { (y - 1, mo + 12) } else { (y, mo) };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (mo - 3) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let dias = era * 146097 + doe - 719468;
    Some(dias * 86400 + h * 3600 + mi * 60 + se - desplazamiento)
}

fn compara(actual: &Value, op: Op, esperado: &Value, exacto: bool) -> bool {
    use std::cmp::Ordering;
    let orden: Option<Ordering> = match (actual, esperado) {
        (Value::String(a), Value::String(b)) => {
            if let (Some(a), Some(b)) = (instante(a), instante(b))
                && matches!(op, Op::Gt | Op::Ge | Op::Lt | Op::Le | Op::Eq | Op::Ne)
            {
                Some(a.cmp(&b))
            } else {
                let (a, b) = if exacto {
                    (a.clone(), b.clone())
                } else {
                    (a.to_lowercase(), b.to_lowercase())
                };
                return match op {
                    Op::Eq => a == b,
                    Op::Ne => a != b,
                    Op::Co => a.contains(&b),
                    Op::Sw => a.starts_with(&b),
                    Op::Ew => a.ends_with(&b),
                    Op::Gt => a > b,
                    Op::Ge => a >= b,
                    Op::Lt => a < b,
                    Op::Le => a <= b,
                };
            }
        }
        (Value::Number(a), Value::Number(b)) => a
            .as_f64()
            .zip(b.as_f64())
            .and_then(|(a, b)| a.partial_cmp(&b)),
        (Value::Bool(a), Value::Bool(b)) => {
            return match op {
                Op::Eq => a == b,
                Op::Ne => a != b,
                _ => false,
            };
        }
        // Booleanos escritos como cadena ("True"), como hace Entra en algunos sitios.
        (Value::Bool(a), Value::String(b)) | (Value::String(b), Value::Bool(a)) => {
            let b = b.eq_ignore_ascii_case("true");
            return match op {
                Op::Eq => *a == b,
                Op::Ne => *a != b,
                _ => false,
            };
        }
        (_, Value::Null) => {
            return match op {
                Op::Eq => actual.is_null(),
                Op::Ne => !actual.is_null(),
                _ => false,
            };
        }
        _ => None,
    };
    match (orden, op) {
        (Some(o), Op::Eq) => o == Ordering::Equal,
        (Some(o), Op::Ne) => o != Ordering::Equal,
        (Some(o), Op::Gt) => o == Ordering::Greater,
        (Some(o), Op::Ge) => o != Ordering::Less,
        (Some(o), Op::Lt) => o == Ordering::Less,
        (Some(o), Op::Le) => o != Ordering::Greater,
        (None, Op::Ne) => true,
        _ => false,
    }
}

#[cfg(test)]
mod pruebas {
    use super::*;
    use serde_json::json;

    fn usuario() -> Value {
        json!({
            "schemas": ["urn:ietf:params:scim:schemas:core:2.0:User"],
            "id": "b",
            "externalId": "AbC-1",
            "userName": "Ana@Empresa.test",
            "displayName": "Ana Pérez",
            "active": true,
            "emails": [
                {"value": "ana@empresa.test", "type": "work", "primary": true},
                {"value": "ventas@empresa.test", "type": "work"}
            ],
            "groups": [{"value": "c", "display": "Ventas"}],
            "meta": {"created": "2026-11-13T10:00:00Z", "resourceType": "User"}
        })
    }

    fn cumple(filtro: &str) -> bool {
        Filtro::parse(filtro)
            .unwrap_or_else(|e| panic!("{filtro}: {e}"))
            .evalua(&usuario())
    }

    #[test]
    fn operadores_basicos() {
        assert!(cumple(r#"userName eq "ana@empresa.test""#));
        assert!(cumple(r#"USERNAME EQ "ANA@EMPRESA.TEST""#));
        assert!(
            !cumple(r#"externalId eq "abc-1""#),
            "externalId es caseExact"
        );
        assert!(cumple(r#"externalId eq "AbC-1""#));
        assert!(cumple(r#"displayName co "pér""#));
        assert!(cumple(r#"userName sw "ana@""#));
        assert!(cumple(r#"userName ew ".test""#));
        assert!(cumple(r#"userName ne "otro@empresa.test""#));
        assert!(cumple("displayName pr"));
        assert!(!cumple("title pr"));
        assert!(cumple("active eq true"));
        assert!(!cumple("active eq false"));
        assert!(cumple(r#"meta.created gt "2026-01-01T00:00:00Z""#));
        assert!(cumple(r#"meta.created lt "2026-11-13T12:00:00.000+01:00""#));
        assert!(!cumple(
            r#"meta.created lt "2026-11-13T10:00:00.000+01:00""#
        ));
        assert!(cumple(r#"meta.created le "2026-11-13T11:00:00+01:00""#));
    }

    #[test]
    fn logicos_y_parentesis() {
        assert!(cumple(r#"userName eq "x" or displayName sw "ana""#));
        assert!(!cumple(r#"userName eq "x" and displayName sw "ana""#));
        assert!(cumple(r#"not (userName eq "x")"#));
        assert!(cumple(
            r#"not(userName eq "x") and (active eq true or title pr)"#
        ));
        // `and` antes que `or`.
        assert!(cumple(r#"userName eq "x" and title pr or active eq true"#));
    }

    #[test]
    fn multivalor_y_filtros_de_valor() {
        assert!(cumple(r#"emails eq "ventas@empresa.test""#));
        assert!(cumple(r#"emails.value eq "VENTAS@empresa.test""#));
        assert!(cumple(r#"emails[type eq "work" and value co "ventas"]"#));
        assert!(!cumple(r#"emails[type eq "home"]"#));
        assert!(cumple(r#"groups.value eq "c""#));
        assert!(cumple(
            r#"urn:ietf:params:scim:schemas:core:2.0:User:userName eq "ana@empresa.test""#
        ));
    }

    #[test]
    fn errores_de_sintaxis() {
        for malo in [
            "",
            "userName",
            r#"userName xx "a""#,
            r#"userName eq"#,
            r#"(userName eq "a""#,
            r#"userName eq "a" and"#,
            r#"userName eq "a" garbage"#,
            r#"emails[type eq "work""#,
            r#"not userName eq "a""#,
            r#"userName eq "sin cerrar"#,
            r#"1abc eq "a""#,
        ] {
            assert!(Filtro::parse(malo).is_err(), "debería fallar: {malo}");
        }
    }

    #[test]
    fn igualdades_y_rutas() {
        let f =
            Filtro::parse(r#"userName eq "a" and (externalId eq "b" or active eq true)"#).unwrap();
        let ig: Vec<String> = f.igualdades().into_iter().map(|(k, _)| k).collect();
        assert_eq!(ig, vec!["username".to_string()]);
        let f = Filtro::parse(r#"emails[value eq "x@y"] and members.value eq "z""#).unwrap();
        let ig: Vec<String> = f.igualdades().into_iter().map(|(k, _)| k).collect();
        assert_eq!(
            ig,
            vec!["emails.value".to_string(), "members.value".to_string()]
        );
        let rutas: Vec<String> = Filtro::parse(r#"emails[type eq "work"] or meta.created gt "x""#)
            .unwrap()
            .rutas()
            .iter()
            .map(|r| r.clave())
            .collect();
        assert_eq!(rutas, vec!["emails.type", "meta.created"]);
    }

    #[test]
    fn rutas_de_patch() {
        let r = RutaPatch::parse(r#"emails[type eq "work"].value"#).unwrap();
        assert_eq!(r.atributo.nombre, "emails");
        assert_eq!(r.sub.as_deref(), Some("value"));
        assert!(r.filtro.is_some());
        let r = RutaPatch::parse("name.givenName").unwrap();
        assert_eq!(
            (r.atributo.nombre.as_str(), r.sub.as_deref()),
            ("name", Some("givenname"))
        );
        let r =
            RutaPatch::parse("urn:ietf:params:scim:schemas:extension:enterprise:2.0:User:manager")
                .unwrap();
        assert_eq!(
            r.atributo.uri.as_deref(),
            Some("urn:ietf:params:scim:schemas:extension:enterprise:2.0:User")
        );
        assert_eq!(r.atributo.nombre, "manager");
        let r = RutaPatch::parse(r#"members[value eq "2819c223"]"#).unwrap();
        assert!(r.filtro.is_some() && r.sub.is_none());
        for malo in [
            "",
            "emails[",
            "emails[type eq \"work\"]x",
            "a.b.c",
            "emails.value[type eq \"x\"]",
        ] {
            assert!(RutaPatch::parse(malo).is_err(), "debería fallar: {malo}");
        }
    }

    #[test]
    fn fechas() {
        assert_eq!(instante("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(instante("2026-11-13T10:00:00Z"), Some(1_794_564_000));
        assert_eq!(instante("2026-11-13T11:00:00+01:00"), Some(1_794_564_000));
        assert_eq!(instante("ayer"), None);
    }
}
