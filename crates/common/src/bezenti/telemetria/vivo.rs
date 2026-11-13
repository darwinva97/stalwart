/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Telemetría en vivo (`/api/live/tracing`, `/api/live/metrics`). Especificación `telemetria.md`, §7.
//!
//! Cada flujo de eventos registra un suscriptor temporal del colector con los mismos eventos que ya graba algún
//! trazador, webhook o el historial (el flujo no puede ver lo que nadie graba) y lo quita al cerrarse. Aquí está
//! lo que no depende de HTTP: los filtros (`filter=` y `<clave>=`), el seguimiento de sesiones y el aislamiento
//! por inquilino.

use crate::{Server, bezenti::inquilino::TenantScope};
use registry::types::datetime::UTCDateTime;
use serde_json::{Value as Json, json};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use store::ahash::{AHashMap, AHashSet};
use tokio::sync::mpsc;
use trc::{
    Collector, DeliveryEvent, Event, EventDetails, EventType, Key, SmtpEvent, Value,
    ipc::subscriber::{EventBatch, Interests, SubscriberBuilder},
};

/// Prefijo de los suscriptores en vivo (la recarga de la configuración no los quita).
pub const PREFIJO: &str = "bezenti_vivo_";
/// Sesiones seguidas a la vez por un flujo.
const MAX_SESIONES: usize = 8192;
/// Eventos que se guardan de una sesión de la que aún no se sabe si es del inquilino.
const MAX_PENDIENTES: usize = 64;

static SIGUIENTE: AtomicU64 = AtomicU64::new(0);

/// Un suscriptor temporal: se da de baja al soltarlo.
pub struct SuscripcionVivo {
    id: String,
    pub rx: mpsc::Receiver<EventBatch>,
}

impl SuscripcionVivo {
    /// Se suscribe a todos los eventos que graba hoy algún trazador, webhook o el historial.
    pub fn nueva() -> Self {
        let id = format!("{PREFIJO}{}", SIGUIENTE.fetch_add(1, Ordering::Relaxed));
        let mut intereses = Interests::default();
        for evento in EventType::variants() {
            if Collector::has_interest(*evento) {
                intereses.set(*evento);
            }
        }
        let (_, rx, _) = SubscriberBuilder::new(id.clone())
            .with_interests(intereses)
            .with_lossy(true)
            .register();
        SuscripcionVivo { id, rx }
    }
}

impl Drop for SuscripcionVivo {
    fn drop(&mut self) {
        Collector::remove_subscriber(std::mem::take(&mut self.id));
    }
}

/// Filtros de la URL: `filter=<texto>` y `<clave>=<texto>` (los demás parámetros se ignoran).
#[derive(Debug, Default, Clone)]
pub struct FiltroVivo {
    texto: Option<String>,
    claves: Vec<(Key, String)>,
}

impl FiltroVivo {
    pub fn parse(query: &str) -> Self {
        let mut filtro = FiltroVivo::default();
        for (k, v) in form_urlencoded_pairs(query) {
            if v.is_empty() {
                continue;
            }
            if k == "filter" {
                filtro.texto = Some(v.to_lowercase());
            } else if let Some(key) = Key::parse(&k) {
                filtro.claves.push((key, v.to_lowercase()));
            }
        }
        filtro
    }

    pub fn es_vacio(&self) -> bool {
        self.texto.is_none() && self.claves.is_empty()
    }

    fn coincide(&self, evento: &Event<EventDetails>) -> bool {
        if let Some(texto) = &self.texto
            && !evento
                .keys
                .iter()
                .any(|(_, v)| valor_coincide(v, texto, true))
        {
            return false;
        }
        self.claves.iter().all(|(key, texto)| {
            evento
                .keys
                .iter()
                .any(|(k, v)| k == key && valor_coincide(v, texto, false))
        })
    }
}

fn valor_coincide(valor: &Value, texto: &str, con_fechas: bool) -> bool {
    match valor {
        Value::String(s) => s.to_lowercase().contains(texto),
        Value::UInt(n) | Value::Duration(n) => texto.parse::<u64>().is_ok_and(|t| t == *n),
        Value::Int(n) => texto.parse::<i64>().is_ok_and(|t| t == *n),
        Value::Float(n) => texto.parse::<f64>().is_ok_and(|t| t == *n),
        Value::Timestamp(t) => {
            con_fechas
                && UTCDateTime::from_timestamp(*t as i64)
                    .to_string()
                    .to_lowercase()
                    .contains(texto)
        }
        Value::Ipv4(ip) => ip.to_string().contains(texto),
        Value::Ipv6(ip) => ip.to_string().contains(texto),
        Value::Array(v) => v.iter().any(|v| valor_coincide(v, texto, con_fechas)),
        Value::Bytes(_) | Value::Bool(_) | Value::Event(_) | Value::None => false,
    }
}

fn form_urlencoded_pairs(query: &str) -> impl Iterator<Item = (String, String)> + '_ {
    query.split('&').filter(|p| !p.is_empty()).map(|p| {
        let (k, v) = p.split_once('=').unwrap_or((p, ""));
        (decodificar(k), decodificar(v))
    })
}

fn decodificar(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    Some(b) => {
                        out.push(b);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn es_fin(typ: EventType) -> bool {
    matches!(
        typ,
        EventType::Smtp(SmtpEvent::ConnectionEnd)
            | EventType::Delivery(DeliveryEvent::AttemptEnd)
            | EventType::Imap(trc::ImapEvent::ConnectionEnd)
            | EventType::Pop3(trc::Pop3Event::ConnectionEnd)
            | EventType::Http(trc::HttpEvent::ConnectionEnd)
            | EventType::ManageSieve(trc::ManageSieveEvent::ConnectionEnd)
    )
}

enum EstadoSesion {
    Pendiente(Vec<Arc<Event<EventDetails>>>),
    Visible,
}

/// Selección de eventos de un flujo: filtros del usuario con seguimiento de sesión y, para un usuario de
/// inquilino, solo las sesiones de su ámbito.
pub struct SelectorVivo {
    filtro: FiltroVivo,
    ambito: Option<TenantScope>,
    /// Sesiones que ya coincidieron con el filtro.
    seguidas: AHashSet<u64>,
    /// Estado de cada sesión respecto al inquilino.
    sesiones: AHashMap<u64, EstadoSesion>,
    dominios: AHashMap<String, Option<u32>>,
}

impl SelectorVivo {
    pub fn new(filtro: FiltroVivo, ambito: Option<TenantScope>) -> Self {
        SelectorVivo {
            filtro,
            ambito,
            seguidas: AHashSet::new(),
            sesiones: AHashMap::new(),
            dominios: AHashMap::new(),
        }
    }

    /// Eventos de un lote que se envían al cliente.
    pub async fn seleccionar(&mut self, server: &Server, lote: EventBatch) -> Vec<Json> {
        let mut salida = Vec::new();
        for evento in lote {
            let visibles = if self.ambito.is_some() {
                self.por_inquilino(server, evento).await
            } else {
                vec![evento]
            };
            for evento in visibles {
                if self.por_filtro(&evento) {
                    salida.push(a_api(&evento));
                }
            }
        }
        salida
    }

    fn por_filtro(&mut self, evento: &Event<EventDetails>) -> bool {
        if self.filtro.es_vacio() {
            return true;
        }
        let span = evento.span_id();
        let seguida = span.is_some_and(|s| self.seguidas.contains(&s));
        let pasa = seguida || self.filtro.coincide(evento);
        if let Some(span) = span {
            if es_fin(evento.inner.typ) {
                self.seguidas.remove(&span);
            } else if pasa && !seguida && self.seguidas.len() < MAX_SESIONES {
                self.seguidas.insert(span);
            }
        }
        pasa
    }

    /// Para un usuario de inquilino: los eventos de sesiones de su ámbito (y los que esperaban a saberlo).
    async fn por_inquilino(
        &mut self,
        server: &Server,
        evento: Arc<Event<EventDetails>>,
    ) -> Vec<Arc<Event<EventDetails>>> {
        let Some(span) = evento.span_id() else {
            return Vec::new();
        };
        let fin = es_fin(evento.inner.typ);
        let es_suyo = self.es_del_ambito(server, &evento).await;
        let estado = match self.sesiones.get_mut(&span) {
            Some(estado) => estado,
            None => {
                if self.sesiones.len() >= MAX_SESIONES {
                    return Vec::new();
                }
                self.sesiones
                    .entry(span)
                    .or_insert(EstadoSesion::Pendiente(Vec::new()))
            }
        };
        let salida = match estado {
            EstadoSesion::Visible => vec![evento],
            EstadoSesion::Pendiente(pendientes) => {
                if es_suyo {
                    let mut salida = std::mem::take(pendientes);
                    salida.push(evento);
                    *estado = EstadoSesion::Visible;
                    salida
                } else {
                    if pendientes.len() < MAX_PENDIENTES {
                        pendientes.push(evento);
                    }
                    Vec::new()
                }
            }
        };
        if fin {
            self.sesiones.remove(&span);
        }
        salida
    }

    async fn es_del_ambito(&mut self, server: &Server, evento: &Event<EventDetails>) -> bool {
        let Some(ambito) = &self.ambito else {
            return true;
        };
        let mut direcciones = Vec::new();
        let mut cuentas = Vec::new();
        for (key, valor) in &evento.keys {
            match (key, valor) {
                (Key::From | Key::To | Key::AccountName | Key::Domain, v) => {
                    recoger_textos(v, &mut direcciones)
                }
                (Key::AccountId, Value::UInt(id)) => cuentas.push(*id as u32),
                _ => {}
            }
        }
        for d in direcciones {
            let dominio = d
                .rsplit_once('@')
                .map(|(_, d)| d)
                .unwrap_or(d.as_str())
                .trim_end_matches('>')
                .to_lowercase();
            let tenant = match self.dominios.get(&dominio) {
                Some(t) => *t,
                None => {
                    let t = match server.domain(&dominio).await {
                        Ok(Some(d)) => d.tenant_id,
                        _ => None,
                    };
                    if self.dominios.len() < MAX_SESIONES {
                        self.dominios.insert(dominio, t);
                    }
                    t
                }
            };
            if tenant.is_some_and(|t| ambito.contains(t)) {
                return true;
            }
        }
        for c in cuentas {
            if let Ok(Some(c)) = server.try_account(c).await
                && c.tenant_id.is_some_and(|t| ambito.contains(t))
            {
                return true;
            }
        }
        false
    }
}

fn recoger_textos(valor: &Value, salida: &mut Vec<String>) {
    match valor {
        Value::String(s) => salida.push(s.to_string()),
        Value::Array(v) => {
            for v in v.iter().take(64) {
                recoger_textos(v, salida);
            }
        }
        _ => {}
    }
}

/// Un evento con el formato de la documentación de la telemetría en vivo: `event`, `timestamp` y `keyValues` (una
/// lista de `{key, value}` con el valor tipado por `@type`, igual que en `x:TraceEvent`).
pub fn a_api(evento: &Event<EventDetails>) -> Json {
    json!({
        "event": evento.inner.typ.as_str(),
        "timestamp": UTCDateTime::from_timestamp(evento.inner.timestamp as i64).to_string(),
        "keyValues": claves_json(&evento.keys, 0),
    })
}

fn claves_json(claves: &[(Key, Value)], profundidad: usize) -> Json {
    Json::Array(
        claves
            .iter()
            .filter(|(k, _)| *k != Key::SpanId)
            .map(|(k, v)| json!({"key": k.as_str(), "value": valor_json(v, profundidad)}))
            .collect(),
    )
}

fn valor_json(valor: &Value, profundidad: usize) -> Json {
    let (tipo, v) = match valor {
        Value::String(s) => ("String", json!(s.as_str())),
        Value::UInt(n) => ("UnsignedInt", json!(n)),
        Value::Int(n) => ("Integer", json!(n)),
        Value::Float(n) => ("Float", json!(n)),
        Value::Timestamp(t) => (
            "UTCDateTime",
            json!(UTCDateTime::from_timestamp(*t as i64).to_string()),
        ),
        Value::Duration(d) => ("Duration", json!(d)),
        Value::Bytes(b) => ("String", json!(String::from_utf8_lossy(b))),
        Value::Bool(b) => ("Boolean", json!(b)),
        Value::Ipv4(ip) => ("IpAddr", json!(ip.to_string())),
        Value::Ipv6(ip) => ("IpAddr", json!(ip.to_string())),
        Value::Array(l) if profundidad < 8 => (
            "List",
            Json::Array(l.iter().map(|v| valor_json(v, profundidad + 1)).collect()),
        ),
        Value::Event(e) if profundidad < 8 => {
            return json!({
                "@type": "Event",
                "event": e.as_ref().as_str(),
                "value": claves_json(e.keys(), profundidad + 1),
            });
        }
        Value::Array(_) | Value::Event(_) | Value::None => return json!({"@type": "Null"}),
    };
    json!({"@type": tipo, "value": v})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(typ: EventType, keys: Vec<(Key, Value)>) -> Event<EventDetails> {
        Event {
            inner: EventDetails {
                typ,
                timestamp: 1_000,
                level: trc::Level::Info,
                span: None,
            },
            keys,
        }
    }

    #[test]
    fn filtros() {
        let f = FiltroVivo::parse("filter=ANA%40a.test&token=x&interval=3");
        assert!(f.coincide(&ev(
            EventType::Smtp(SmtpEvent::MailFrom),
            vec![(Key::From, Value::String("ana@a.test".into()))]
        )));
        assert!(!f.coincide(&ev(
            EventType::Smtp(SmtpEvent::MailFrom),
            vec![(Key::From, Value::String("beto@b.test".into()))]
        )));
        let f = FiltroVivo::parse("remoteIp=10.0.0&remoteip=nada");
        assert_eq!(f.claves.len(), 1);
        assert!(f.coincide(&ev(
            EventType::Smtp(SmtpEvent::ConnectionStart),
            vec![(Key::RemoteIp, Value::Ipv4([10, 0, 0, 9].into()))]
        )));
        let f = FiltroVivo::parse("size=42");
        assert!(f.coincide(&ev(
            EventType::Smtp(SmtpEvent::MailFrom),
            vec![(Key::Size, Value::UInt(42))]
        )));
        assert!(!f.coincide(&ev(
            EventType::Smtp(SmtpEvent::MailFrom),
            vec![(Key::Size, Value::UInt(421))]
        )));
    }

    #[test]
    fn seguimiento_de_sesion() {
        let mut s = SelectorVivo::new(FiltroVivo::parse("filter=ana"), None);
        let a = ev(
            EventType::Smtp(SmtpEvent::MailFrom),
            vec![
                (Key::SpanId, Value::UInt(1)),
                (Key::From, Value::String("ana@a.test".into())),
            ],
        );
        let b = ev(
            EventType::Smtp(SmtpEvent::RcptTo),
            vec![(Key::SpanId, Value::UInt(1))],
        );
        let c = ev(
            EventType::Smtp(SmtpEvent::RcptTo),
            vec![(Key::SpanId, Value::UInt(2))],
        );
        let fin = ev(
            EventType::Smtp(SmtpEvent::ConnectionEnd),
            vec![(Key::SpanId, Value::UInt(1))],
        );
        assert!(!s.por_filtro(&b));
        assert!(s.por_filtro(&a));
        assert!(s.por_filtro(&b));
        assert!(!s.por_filtro(&c));
        assert!(s.por_filtro(&fin));
        assert!(!s.por_filtro(&b));
    }
}
