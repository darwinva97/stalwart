/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Historial de entregas (`x:Trace`, `x:Trace/InboundDelivery`, `x:Trace/OutboundDelivery`).
//!
//! Especificación `bezenti/especificaciones/telemetria.md`, §2–§3. Cómo funciona:
//!
//! 1. El suscriptor [`Historial`] recibe del colector (en su propio hilo, por lotes) solo los eventos de
//!    [`interesa`]. Como cualquier otro trazador, si se queda atrás el colector descarta eventos en vez de
//!    esperar: el correo nunca espera al historial.
//! 2. Agrupa los eventos por sesión (`spanId`): sesiones SMTP de entrada (`smtp.connection-start` …
//!    `smtp.connection-end`) e intentos de entrega (`delivery.attempt-start` … `delivery.attempt-end`).
//! 3. Al cerrar la sesión, si es de entrada y no encoló ningún mensaje se descarta; si no, se codifica.
//! 4. Cada [`volcado`](ENV_VOLCADO_TRAZAS) (5 s) o cada 128 trazas se resuelven los inquilinos (dominios de
//!    remitente y destinatarios, cuenta autenticada) y se escribe todo en **un solo lote** del almacén.
//!
//! Claves en el subespacio `TelemetrySpan` (todas con el id de la traza en big-endian, que ordena por tiempo):
//! `[0] id` → la traza; `[1] id` → su resumen (para filtrar sin leer los eventos); `[2] cola id` y
//! `[3] inquilino id` → índices vacíos.

use super::{
    ENV_VOLCADO_TRAZAS,
    formato::{Escritor, Lector, comprimir, descomprimir},
    metricas, segundos_env, servidor,
};
use crate::{Server, bezenti::inquilino::TenantScope};
use registry::{
    schema::structs::Trace,
    types::{datetime::UTCDateTime, list::List},
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use store::{
    IterateParams, Store, Subspace, ValueKey,
    ahash::{AHashMap, AHashSet},
    write::{AnyClass, AnyKey, BatchBuilder, ValueClass, serialize::RawValue},
};
use trc::{
    AddContext, AuthEvent, DeliveryEvent, Event, EventDetails, EventType, Key, QueueEvent,
    SmtpEvent, Value,
    ipc::subscriber::{EventBatch, Interests, SubscriberBuilder},
};
use utils::snowflake::SnowflakeIdGenerator;

/// Máximo de eventos por traza (documentación: hasta 2048).
pub const MAX_EVENTOS: usize = 2048;
/// Máximo de sesiones abiertas a la vez (por encima, las nuevas no se registran).
const MAX_ABIERTAS: usize = 16_384;
/// Una sesión sin cierre se abandona al cabo de este tiempo.
const MAX_DURACION_SESION: u64 = 3600;
/// Volcado al almacén: cada este tiempo o cada `LOTE` trazas, lo que llegue antes.
const VOLCADO: Duration = Duration::from_secs(5);
const LOTE: usize = 128;
/// Máximo de resultados que examina una consulta (los más recientes primero).
pub const MAX_RESULTADOS: usize = 10_000;

const P_TRAZA: u8 = 0;
const P_RESUMEN: u8 = 1;
const P_COLA: u8 = 2;
const P_INQUILINO: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Tipo {
    /// Sesión SMTP de entrada (`smtp.connection-start`).
    Entrada = 0,
    /// Intento de entrega (`delivery.attempt-start`).
    Salida = 1,
}

impl Tipo {
    fn de(v: u8) -> Self {
        if v == Tipo::Salida as u8 {
            Tipo::Salida
        } else {
            Tipo::Entrada
        }
    }

    /// El evento que abre las trazas de este tipo (`filtersStatic.event` de las vistas del esquema).
    pub fn evento(&self) -> EventType {
        match self {
            Tipo::Entrada => EventType::Smtp(SmtpEvent::ConnectionStart),
            Tipo::Salida => EventType::Delivery(DeliveryEvent::AttemptStart),
        }
    }

    pub fn por_evento(evento: EventType) -> Option<Self> {
        match evento {
            EventType::Smtp(SmtpEvent::ConnectionStart) => Some(Tipo::Entrada),
            EventType::Delivery(DeliveryEvent::AttemptStart) => Some(Tipo::Salida),
            _ => None,
        }
    }
}

/// ¿Guarda el historial este evento? Los de la sesión SMTP, autenticación, filtros y entrega, sin los datos del
/// protocolo (`raw-input`/`raw-output`) ni los de nivel `trace`.
pub fn interesa(evento: EventType) -> bool {
    use trc::Level;
    if evento.level() == Level::Trace {
        return false;
    }
    match evento {
        EventType::Smtp(SmtpEvent::RawInput | SmtpEvent::RawOutput)
        | EventType::Delivery(DeliveryEvent::RawInput | DeliveryEvent::RawOutput) => false,
        EventType::Smtp(_)
        | EventType::Delivery(_)
        | EventType::Queue(_)
        | EventType::Auth(_)
        | EventType::Spf(_)
        | EventType::Dkim(_)
        | EventType::Dmarc(_)
        | EventType::Arc(_)
        | EventType::Iprev(_)
        | EventType::MailAuth(_)
        | EventType::Spam(_)
        | EventType::Sieve(_)
        | EventType::MessageIngest(_)
        | EventType::MtaSts(_)
        | EventType::Dane(_)
        | EventType::Milter(_)
        | EventType::MtaHook(_)
        | EventType::Security(_)
        | EventType::Limit(_)
        | EventType::Tls(_) => true,
        _ => false,
    }
}

/// Eventos que se suscriben para el historial.
pub fn intereses() -> Interests {
    let mut intereses = Interests::default();
    for evento in EventType::variants() {
        if interesa(*evento) {
            intereses.set(*evento);
        }
    }
    intereses
}

/// Lo que se guarda aparte de los eventos para filtrar sin leerlos.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resumen {
    pub tipo: u8,
    pub timestamp: u64,
    pub size: u64,
    pub from: String,
    pub to: Vec<String>,
    pub colas: Vec<u64>,
    pub inquilinos: Vec<u32>,
    /// Texto en minúsculas en el que busca el filtro `text` (direcciones, IP remota, EHLO, id de mensaje).
    pub palabras: String,
}

impl Resumen {
    fn serializar(&self) -> Vec<u8> {
        let mut w = Escritor::with_capacity(64 + self.palabras.len());
        w.u8(self.tipo);
        w.num(self.timestamp);
        w.num(self.size);
        w.texto(&self.from);
        w.num(self.to.len() as u64);
        for to in &self.to {
            w.texto(to);
        }
        w.num(self.colas.len() as u64);
        for c in &self.colas {
            w.num(*c);
        }
        w.num(self.inquilinos.len() as u64);
        for t in &self.inquilinos {
            w.num(*t as u64);
        }
        w.texto(&self.palabras);
        w.buf
    }

    fn deserializar(datos: &[u8]) -> Option<Self> {
        let mut r = Lector::new(datos);
        let tipo = r.u8()?;
        let timestamp = r.num()?;
        let size = r.num()?;
        let from = r.texto()?;
        let to = (0..r.num()?)
            .map(|_| r.texto())
            .collect::<Option<Vec<_>>>()?;
        let colas = (0..r.num()?).map(|_| r.num()).collect::<Option<Vec<_>>>()?;
        let inquilinos = (0..r.num()?)
            .map(|_| r.num().map(|t| t as u32))
            .collect::<Option<Vec<_>>>()?;
        let palabras = r.texto()?;
        Some(Resumen {
            tipo,
            timestamp,
            size,
            from,
            to,
            colas,
            inquilinos,
            palabras,
        })
    }

    pub fn tipo(&self) -> Tipo {
        Tipo::de(self.tipo)
    }
}

fn clave(prefijo: u8, partes: &[&[u8]]) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + partes.iter().map(|p| p.len()).sum::<usize>());
    k.push(prefijo);
    for p in partes {
        k.extend_from_slice(p);
    }
    k
}

fn clase(key: Vec<u8>) -> ValueClass {
    ValueClass::Any(AnyClass {
        subspace: Subspace::TelemetrySpan,
        key,
    })
}

fn rango(desde: Vec<u8>, hasta: Vec<u8>) -> IterateParams<AnyKey<Vec<u8>>> {
    IterateParams::new(
        AnyKey {
            subspace: Subspace::TelemetrySpan,
            key: desde,
        },
        AnyKey {
            subspace: Subspace::TelemetrySpan,
            key: hasta,
        },
    )
}

fn id_de_clave(key: &[u8], desde: usize) -> Option<u64> {
    key.get(desde..desde + 8)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_be_bytes)
}

/// Época de los ids «snowflake» (`utils::snowflake`): 2021-09-22.
const EPOCA_SNOWFLAKE: u64 = 1_632_280_000;

/// Id de traza más bajo para un instante (segundos UNIX), también en el futuro.
pub fn id_desde(segundos: u64) -> u64 {
    segundos
        .saturating_sub(EPOCA_SNOWFLAKE)
        .saturating_mul(1000)
        .checked_shl(21)
        .filter(|id| *id >> 21 == segundos.saturating_sub(EPOCA_SNOWFLAKE) * 1000)
        .unwrap_or(u64::MAX)
}

/// Segundos UNIX de un id de traza.
pub fn segundos_de_id(id: u64) -> u64 {
    SnowflakeIdGenerator::to_timestamp(id)
}

// ------------------------------------------------------------------------------------------------------------
// Suscriptor
// ------------------------------------------------------------------------------------------------------------

/// El trazador del historial para la lista de trazadores (`None` si `x:TracingStore` es `Disabled`).
pub async fn suscriptor(
    bp: &mut store::registry::bootstrap::Bootstrap,
    storage: &crate::config::storage::Storage,
) -> Option<crate::config::telemetry::TelemetrySubscriber> {
    let (store, huella) = super::almacen_trazas(bp, &storage.data).await?;
    Some(crate::config::telemetry::TelemetrySubscriber {
        id: format!("bezenti_historial_{huella:x}"),
        interests: intereses(),
        typ: crate::config::telemetry::TelemetrySubscriberType::Historial(Historial { store }),
        // Nunca frena al colector: si el historial se queda atrás, se pierden eventos (como en cualquier
        // trazador con pérdida), no se retrasa el correo.
        lossy: true,
    })
}

/// El trazador del historial (un `TelemetrySubscriberType` más).
#[derive(Debug, Clone)]
pub struct Historial {
    pub store: Store,
}

struct EnCurso {
    tipo: Tipo,
    /// Cuándo llegó el evento de cierre: la traza se cierra [`GRACIA`] después, por si llegan eventos tardíos.
    fin: Option<Instant>,
    inicio: u64,
    eventos: Vec<Arc<Event<EventDetails>>>,
    encolado: bool,
    from: Option<String>,
    to: Vec<String>,
    size: u64,
    colas: Vec<u64>,
    cuentas: Vec<u32>,
    palabras: Vec<String>,
}

/// Una traza cerrada, codificada y pendiente de escribir.
struct Lista {
    id: u64,
    resumen: Resumen,
    /// Carga útil ya codificada (sin la cabecera de inquilinos).
    comprimida: bool,
    cuerpo: Vec<u8>,
    /// Ids de los eventos, para los contadores por inquilino.
    eventos: Vec<EventType>,
    cuentas: Vec<u32>,
}

impl Historial {
    pub fn spawn(self, builder: SubscriberBuilder) {
        let (_, mut rx, _) = builder.register();
        tokio::spawn(async move {
            let volcado = segundos_env(ENV_VOLCADO_TRAZAS).unwrap_or(VOLCADO);
            let mut agrupador = Agrupador::default();
            let mut listas: Vec<Lista> = Vec::new();
            let mut proximo = Instant::now() + volcado;
            let mut proxima_limpieza = Instant::now() + Duration::from_secs(60);
            loop {
                let espera = proximo.saturating_duration_since(Instant::now());
                let abierto = tokio::select! {
                    lote = rx.recv() => match lote {
                        Some(lote) => {
                            agrupador.procesar(lote, &mut listas);
                            true
                        }
                        None => false,
                    },
                    _ = tokio::time::sleep(espera) => true,
                };
                let ahora = Instant::now();
                // Al cerrar el suscriptor (recarga o parada) se guardan ya las sesiones terminadas.
                agrupador.cerrar_vencidas(
                    if abierto { ahora } else { ahora + GRACIA },
                    &mut listas,
                );
                if !listas.is_empty() && (listas.len() >= LOTE || ahora >= proximo || !abierto) {
                    volcar(&self.store, std::mem::take(&mut listas)).await;
                }
                if ahora >= proximo {
                    proximo = ahora + volcado;
                }
                if ahora >= proxima_limpieza {
                    proxima_limpieza = ahora + Duration::from_secs(60);
                    agrupador.limpiar(store::write::now());
                }
                if !abierto {
                    break;
                }
            }
        });
    }
}

/// Estado del agrupador de sesiones.
///
/// - Las sesiones SMTP y los intentos de entrega sacan sus `spanId` de generadores distintos, así que pueden
///   coincidir: las sesiones abiertas se guardan por `(spanId, tipo)`.
/// - El colector vacía por turnos las colas de cada hilo, así que los eventos de una sesión que cambia de hilo
///   pueden llegar desordenados (incluso antes que su `connection-start`). Los que llegan sin sesión abierta se
///   guardan un rato como «huérfanos» y se adoptan cuando llega el inicio.
#[derive(Default)]
struct Agrupador {
    abiertas: AHashMap<(u64, u8), EnCurso>,
    huerfanos: AHashMap<u64, (u64, Vec<Arc<Event<EventDetails>>>)>,
}

/// Huérfanos como mucho: sesiones distintas, eventos por sesión y segundos de espera.
const MAX_HUERFANOS: usize = 4096;
const MAX_EVENTOS_HUERFANOS: usize = 256;
const ESPERA_HUERFANOS: u64 = 60;
/// Espera tras el cierre de una sesión antes de guardarla (eventos de otros hilos que llegan tarde).
const GRACIA: Duration = Duration::from_secs(2);

/// Tipo de sesión al que pertenece un evento si hay dos sesiones abiertas con el mismo `spanId`.
fn tipo_probable(typ: EventType) -> Tipo {
    match typ {
        EventType::Delivery(_)
        | EventType::MessageIngest(_)
        | EventType::MtaSts(_)
        | EventType::Dane(_)
        | EventType::Queue(QueueEvent::DsnQueued) => Tipo::Salida,
        _ => Tipo::Entrada,
    }
}

fn tipo_fin(typ: EventType) -> Option<Tipo> {
    match typ {
        EventType::Smtp(SmtpEvent::ConnectionEnd) => Some(Tipo::Entrada),
        EventType::Delivery(DeliveryEvent::AttemptEnd) => Some(Tipo::Salida),
        _ => None,
    }
}

impl Agrupador {
    fn procesar(&mut self, lote: EventBatch, listas: &mut Vec<Lista>) {
        for evento in lote {
            self.evento(evento, listas);
        }
    }

    /// Cierra las trazas cuyo evento de cierre llegó hace más de [`GRACIA`].
    fn cerrar_vencidas(&mut self, ahora: Instant, listas: &mut Vec<Lista>) {
        let vencidas = self
            .abiertas
            .iter()
            .filter(|(_, t)| {
                t.fin
                    .is_some_and(|f| ahora.saturating_duration_since(f) >= GRACIA)
            })
            .map(|(k, _)| *k)
            .collect::<Vec<_>>();
        for clave in vencidas {
            if let Some(t) = self.abiertas.remove(&clave)
                && (t.tipo == Tipo::Salida || t.encolado)
            {
                listas.push(t.cerrar(clave.0));
            }
        }
    }

    /// Olvida las sesiones sin cierre y los huérfanos que nadie adoptó.
    fn limpiar(&mut self, ahora: u64) {
        let limite = ahora.saturating_sub(MAX_DURACION_SESION);
        self.abiertas.retain(|_, t| t.inicio >= limite);
        let limite = ahora.saturating_sub(ESPERA_HUERFANOS);
        self.huerfanos.retain(|_, (llegada, _)| *llegada >= limite);
    }

    fn evento(&mut self, evento: Arc<Event<EventDetails>>, listas: &mut Vec<Lista>) {
        let Some(span) = evento.span_id() else {
            return;
        };
        let typ = evento.inner.typ;
        if let Some(tipo) = Tipo::por_evento(typ) {
            if self.abiertas.len() >= MAX_ABIERTAS {
                return;
            }
            let mut t = EnCurso {
                tipo,
                fin: None,
                inicio: evento.inner.timestamp,
                eventos: Vec::with_capacity(32),
                encolado: false,
                from: None,
                to: Vec::new(),
                size: 0,
                colas: Vec::new(),
                cuentas: Vec::new(),
                palabras: Vec::new(),
            };
            t.anotar(&evento);
            t.eventos.push(evento);
            self.abiertas.insert((span, tipo as u8), t);

            // Adopta los eventos de esta sesión que llegaron antes que su inicio.
            if let Some((llegada, previos)) = self.huerfanos.remove(&span) {
                let (mios, ajenos): (Vec<_>, Vec<_>) = previos.into_iter().partition(|e| {
                    tipo_fin(e.inner.typ).unwrap_or_else(|| tipo_probable(e.inner.typ)) == tipo
                });
                if !ajenos.is_empty() {
                    self.huerfanos.insert(span, (llegada, ajenos));
                }
                for e in mios {
                    self.evento(e, listas);
                }
            }
            return;
        }
        if let Some(tipo) = tipo_fin(typ) {
            match self.abiertas.get_mut(&(span, tipo as u8)) {
                Some(t) => {
                    t.eventos.push(evento);
                    t.fin = Some(Instant::now());
                }
                None => self.huerfano(span, evento),
            }
            return;
        }
        let entrada = (span, Tipo::Entrada as u8);
        let salida = (span, Tipo::Salida as u8);
        let clave = match (
            self.abiertas.contains_key(&entrada),
            self.abiertas.contains_key(&salida),
        ) {
            (true, false) => entrada,
            (false, true) => salida,
            (true, true) => (span, tipo_probable(typ) as u8),
            (false, false) => {
                self.huerfano(span, evento);
                return;
            }
        };
        if let Some(t) = self.abiertas.get_mut(&clave) {
            t.anotar(&evento);
            if t.eventos.len() < MAX_EVENTOS {
                t.eventos.push(evento);
            }
        }
    }

    fn huerfano(&mut self, span: u64, evento: Arc<Event<EventDetails>>) {
        let llegada = evento.inner.timestamp;
        if let Some((_, eventos)) = self.huerfanos.get_mut(&span) {
            if eventos.len() < MAX_EVENTOS_HUERFANOS {
                eventos.push(evento);
            }
        } else if self.huerfanos.len() < MAX_HUERFANOS {
            self.huerfanos.insert(span, (llegada, vec![evento]));
        }
    }
}

fn texto(valor: &Value) -> Option<&str> {
    match valor {
        Value::String(s) => Some(s.as_str()),
        _ => None,
    }
}

fn textos(valor: &Value) -> Vec<String> {
    match valor {
        Value::String(s) => vec![s.to_string()],
        Value::Array(v) => v.iter().filter_map(texto).map(str::to_string).collect(),
        _ => Vec::new(),
    }
}

impl EnCurso {
    /// Recoge de cada evento lo que va al resumen.
    fn anotar(&mut self, evento: &Event<EventDetails>) {
        let typ = evento.inner.typ;
        let es_cola = matches!(
            typ,
            EventType::Queue(
                QueueEvent::MessageQueued
                    | QueueEvent::AuthenticatedMessageQueued
                    | QueueEvent::DsnQueued
                    | QueueEvent::ReportQueued
                    | QueueEvent::AutogeneratedQueued
            )
        );
        let es_inicio_salida = typ == EventType::Delivery(DeliveryEvent::AttemptStart);
        if es_cola {
            self.encolado = true;
        }
        for (key, valor) in &evento.keys {
            match key {
                Key::From if es_cola || es_inicio_salida => {
                    if self.from.is_none()
                        && let Some(from) = texto(valor)
                    {
                        self.from = Some(from.to_string());
                    }
                }
                Key::To if es_cola || es_inicio_salida => {
                    for to in textos(valor) {
                        if !self.to.contains(&to) {
                            self.to.push(to);
                        }
                    }
                }
                Key::Size if es_cola || es_inicio_salida => {
                    if let Value::UInt(size) = valor {
                        if es_cola {
                            self.size += size;
                        } else {
                            self.size = *size;
                        }
                    }
                }
                Key::QueueId => {
                    if let Value::UInt(id) = valor
                        && !self.colas.contains(id)
                    {
                        self.colas.push(*id);
                    }
                }
                Key::AccountId if typ == EventType::Auth(AuthEvent::Success) => {
                    if let Value::UInt(id) = valor {
                        self.cuentas.push(*id as u32);
                    }
                }
                Key::RemoteIp | Key::Hostname | Key::MessageId if self.palabras.len() < 16 => {
                    let s = match valor {
                        Value::Ipv4(ip) => ip.to_string(),
                        Value::Ipv6(ip) => ip.to_string(),
                        Value::String(s) => s.to_string(),
                        _ => continue,
                    };
                    if !self.palabras.contains(&s) {
                        self.palabras.push(s);
                    }
                }
                _ => {}
            }
        }
        if typ == EventType::Smtp(SmtpEvent::Ehlo)
            && let Some(Value::String(dominio)) = evento
                .keys
                .iter()
                .find(|(k, _)| *k == Key::Domain)
                .map(|(_, v)| v)
            && self.palabras.len() < 16
        {
            self.palabras.push(dominio.to_string());
        }
    }

    fn cerrar(mut self, span: u64) -> Lista {
        // Los eventos adoptados (llegados desordenados) se recolocan por fecha; a igual segundo, en su orden.
        self.eventos.sort_by_key(|e| {
            let typ = e.inner.typ;
            let rango = if Tipo::por_evento(typ).is_some() {
                0
            } else if tipo_fin(typ).is_some() {
                2
            } else {
                1
            };
            (e.inner.timestamp, rango)
        });
        let id = (span & !1) | self.tipo as u64;
        let from = self.from.unwrap_or_default();
        let mut w = Escritor::with_capacity(256 + self.eventos.len() * 48);
        w.u8(self.tipo as u8);
        w.num(self.inicio);
        w.texto(&from);
        w.num(self.to.len() as u64);
        for to in &self.to {
            w.texto(to);
        }
        w.num(self.size);
        w.num(self.eventos.len() as u64);
        for evento in &self.eventos {
            w.evento(evento, self.inicio);
        }
        let (comprimida, cuerpo) = comprimir(w.buf);
        let mut palabras = String::with_capacity(128);
        for p in std::iter::once(&from)
            .chain(self.to.iter())
            .chain(self.palabras.iter())
        {
            if !p.is_empty() {
                palabras.push_str(&p.to_lowercase());
                palabras.push(' ');
            }
        }
        for c in &self.colas {
            palabras.push_str(&c.to_string());
            palabras.push(' ');
        }
        Lista {
            id,
            resumen: Resumen {
                tipo: self.tipo as u8,
                timestamp: self.inicio,
                size: self.size,
                from,
                to: self.to,
                colas: self.colas,
                inquilinos: Vec::new(),
                palabras,
            },
            comprimida,
            cuerpo,
            eventos: self.eventos.iter().map(|e| e.inner.typ).collect(),
            cuentas: self.cuentas,
        }
    }
}

/// Inquilinos de una traza: los de los dominios de remitente y destinatarios y el de la cuenta autenticada.
async fn inquilinos(
    server: &Server,
    lista: &Lista,
    cache: &mut AHashMap<String, Option<u32>>,
) -> Vec<u32> {
    let mut inquilinos = Vec::new();
    for direccion in std::iter::once(&lista.resumen.from).chain(lista.resumen.to.iter()) {
        let Some((_, dominio)) = direccion.rsplit_once('@') else {
            continue;
        };
        let dominio = dominio.trim_end_matches('>').to_lowercase();
        let tenant = match cache.get(&dominio) {
            Some(t) => *t,
            None => {
                let t = match server.domain(&dominio).await {
                    Ok(Some(d)) => d.tenant_id,
                    _ => None,
                };
                cache.insert(dominio, t);
                t
            }
        };
        if let Some(t) = tenant
            && !inquilinos.contains(&t)
        {
            inquilinos.push(t);
        }
    }
    for cuenta in &lista.cuentas {
        if let Ok(Some(c)) = server.try_account(*cuenta).await
            && let Some(t) = c.tenant_id
            && !inquilinos.contains(&t)
        {
            inquilinos.push(t);
        }
    }
    inquilinos.sort_unstable();
    inquilinos
}

async fn volcar(store: &Store, listas: Vec<Lista>) {
    let server = servidor();
    let mut cache = AHashMap::new();
    let mut batch = BatchBuilder::new();
    let mut por_inquilino: Vec<(u32, Vec<EventType>)> = Vec::new();
    for mut lista in listas {
        if let Some(server) = &server {
            lista.resumen.inquilinos = inquilinos(server, &lista, &mut cache).await;
        }
        let id = lista.id.to_be_bytes();
        let mut registro = Vec::with_capacity(lista.cuerpo.len() + 8);
        registro.push(super::formato::VERSION);
        registro.push(lista.comprimida as u8);
        {
            let mut w = Escritor::default();
            w.num(lista.resumen.inquilinos.len() as u64);
            for t in &lista.resumen.inquilinos {
                w.num(*t as u64);
            }
            registro.extend_from_slice(&w.buf);
        }
        registro.extend_from_slice(&lista.cuerpo);
        batch.set(clase(clave(P_TRAZA, &[&id])), registro);
        batch.set(clase(clave(P_RESUMEN, &[&id])), lista.resumen.serializar());
        for cola in &lista.resumen.colas {
            batch.set(
                clase(clave(P_COLA, &[&cola.to_be_bytes(), &id])),
                Vec::new(),
            );
        }
        for t in &lista.resumen.inquilinos {
            batch.set(
                clase(clave(P_INQUILINO, &[&t.to_be_bytes(), &id])),
                Vec::new(),
            );
            por_inquilino.push((*t, lista.eventos.clone()));
        }
    }
    if let Err(err) = store.write_batch(&mut batch).await {
        trc::error!(
            err.caused_by(trc::location!())
                .details("No se pudo guardar el historial de entregas")
        );
        return;
    }
    for (tenant, eventos) in por_inquilino {
        metricas::contar_inquilino(tenant, eventos.into_iter());
    }
}

// ------------------------------------------------------------------------------------------------------------
// Consultas
// ------------------------------------------------------------------------------------------------------------

/// Filtros de `x:Trace/query`.
#[derive(Debug, Clone, Default)]
pub struct FiltroTrazas {
    /// Segundos UNIX, ambos inclusive.
    pub desde: Option<u64>,
    pub hasta: Option<u64>,
    pub tipo: Option<Tipo>,
    pub cola: Option<u64>,
    /// En minúsculas.
    pub texto: Option<String>,
    pub ascendente: bool,
}

impl FiltroTrazas {
    fn rango_ids(&self) -> (u64, u64) {
        let desde = self.desde.map(id_desde).unwrap_or(0);
        let hasta = self
            .hasta
            .map(|h| id_desde(h.saturating_add(1)).saturating_sub(1))
            .unwrap_or(u64::MAX);
        (desde, hasta)
    }

    fn admite(&self, id: u64, r: &Resumen, ambito: Option<&TenantScope>) -> bool {
        let (desde, hasta) = self.rango_ids();
        id >= desde
            && id <= hasta
            && self.tipo.is_none_or(|t| r.tipo() == t)
            && self.cola.is_none_or(|c| r.colas.contains(&c))
            && self
                .texto
                .as_ref()
                .is_none_or(|t| r.palabras.contains(t.as_str()))
            && ambito.is_none_or(|a| r.inquilinos.iter().any(|t| a.contains(*t)))
    }
}

impl Server {
    fn almacen_trazas(&self) -> trc::Result<&Store> {
        self.core.telemetria.trazas.as_ref().ok_or_else(|| {
            trc::JmapEvent::Forbidden
                .into_err()
                .details("Tracing history is disabled (x:TracingStore)")
        })
    }

    /// Resumen de una traza.
    pub async fn resumen_traza(&self, id: u64) -> trc::Result<Option<Resumen>> {
        let store = self.almacen_trazas()?;
        Ok(store
            .get_value::<RawValue>(ValueKey::from(clase(clave(
                P_RESUMEN,
                &[&id.to_be_bytes()],
            ))))
            .await
            .caused_by(trc::location!())?
            .and_then(|v| Resumen::deserializar(&v.0)))
    }

    /// Una traza con el formato de la API y sus inquilinos. `None` si no existe.
    pub async fn traza(&self, id: u64) -> trc::Result<Option<(Vec<u32>, Trace)>> {
        let store = self.almacen_trazas()?;
        let Some(registro) = store
            .get_value::<RawValue>(ValueKey::from(clase(clave(P_TRAZA, &[&id.to_be_bytes()]))))
            .await
            .caused_by(trc::location!())?
        else {
            return Ok(None);
        };
        Ok(decodificar(&registro.0))
    }

    /// Ids de las trazas que cumplen el filtro y que ve el ámbito (`None` = operador), en el orden pedido.
    pub async fn consultar_trazas(
        &self,
        filtro: &FiltroTrazas,
        ambito: Option<&TenantScope>,
    ) -> trc::Result<Vec<u64>> {
        let store = self.almacen_trazas()?;
        let (desde, hasta) = filtro.rango_ids();
        if desde > hasta {
            return Ok(Vec::new());
        }

        // Candidatos por índice cuando hay cola o ámbito de inquilino.
        let candidatos = if let Some(cola) = filtro.cola {
            let mut ids = Vec::new();
            let c = cola.to_be_bytes();
            store
                .iterate(
                    rango(
                        clave(P_COLA, &[&c, &0u64.to_be_bytes()]),
                        clave(P_COLA, &[&c, &u64::MAX.to_be_bytes()]),
                    )
                    .no_values(),
                    |key, _| {
                        if let Some(id) = id_de_clave(key, 9) {
                            ids.push(id);
                        }
                        Ok(true)
                    },
                )
                .await
                .caused_by(trc::location!())?;
            Some(ids)
        } else if let Some(ambito) = ambito {
            let mut ids = AHashSet::new();
            for t in ambito.ids() {
                let t = t.to_be_bytes();
                store
                    .iterate(
                        rango(
                            clave(P_INQUILINO, &[&t, &desde.to_be_bytes()]),
                            clave(P_INQUILINO, &[&t, &hasta.to_be_bytes()]),
                        )
                        .no_values(),
                        |key, _| {
                            if let Some(id) = id_de_clave(key, 5) {
                                ids.insert(id);
                            }
                            Ok(true)
                        },
                    )
                    .await
                    .caused_by(trc::location!())?;
            }
            Some(ids.into_iter().collect())
        } else {
            None
        };

        let mut resultado = Vec::new();
        match candidatos {
            Some(mut ids) => {
                ids.sort_unstable();
                if !filtro.ascendente {
                    ids.reverse();
                }
                let solo_ambito =
                    filtro.cola.is_none() && filtro.tipo.is_none() && filtro.texto.is_none();
                for id in ids {
                    if resultado.len() >= MAX_RESULTADOS {
                        break;
                    }
                    if solo_ambito {
                        resultado.push(id);
                    } else if let Some(r) = self.resumen_traza(id).await?
                        && filtro.admite(id, &r, ambito)
                    {
                        resultado.push(id);
                    }
                }
            }
            None => {
                store
                    .iterate(
                        rango(
                            clave(P_RESUMEN, &[&desde.to_be_bytes()]),
                            clave(P_RESUMEN, &[&hasta.to_be_bytes()]),
                        )
                        .set_ascending(filtro.ascendente),
                        |key, value| {
                            if let Some(id) = id_de_clave(key, 1)
                                && let Some(r) = Resumen::deserializar(value)
                                && filtro.admite(id, &r, None)
                            {
                                resultado.push(id);
                            }
                            Ok(resultado.len() < MAX_RESULTADOS)
                        },
                    )
                    .await
                    .caused_by(trc::location!())?;
            }
        }
        Ok(resultado)
    }

    /// Borra las trazas anteriores a `holdTracesFor`. Devuelve cuántas.
    pub async fn purgar_trazas(&self) -> trc::Result<u64> {
        let (Some(store), Some(retencion)) = (
            self.core.telemetria.trazas.as_ref(),
            self.core.telemetria.retencion_trazas,
        ) else {
            return Ok(0);
        };
        let corte = id_desde(store::write::now().saturating_sub(retencion));
        let mut borrar = Vec::new();
        store
            .iterate(
                rango(
                    clave(P_RESUMEN, &[&0u64.to_be_bytes()]),
                    clave(P_RESUMEN, &[&corte.to_be_bytes()]),
                ),
                |key, value| {
                    if let Some(id) = id_de_clave(key, 1)
                        && id < corte
                    {
                        borrar.push((id, Resumen::deserializar(value).unwrap_or_default()));
                    }
                    Ok(true)
                },
            )
            .await
            .caused_by(trc::location!())?;
        let total = borrar.len() as u64;
        for trozo in borrar.chunks(500) {
            let mut batch = BatchBuilder::new();
            for (id, r) in trozo {
                let id = id.to_be_bytes();
                batch.clear(clase(clave(P_TRAZA, &[&id])));
                batch.clear(clase(clave(P_RESUMEN, &[&id])));
                for c in &r.colas {
                    batch.clear(clase(clave(P_COLA, &[&c.to_be_bytes(), &id])));
                }
                for t in &r.inquilinos {
                    batch.clear(clase(clave(P_INQUILINO, &[&t.to_be_bytes(), &id])));
                }
            }
            store
                .write_batch(&mut batch)
                .await
                .caused_by(trc::location!())?;
        }
        Ok(total)
    }
}

/// Decodifica un registro de traza: (inquilinos, traza).
fn decodificar(registro: &[u8]) -> Option<(Vec<u32>, Trace)> {
    let (&version, resto) = registro.split_first()?;
    if version != super::formato::VERSION {
        return None;
    }
    let (&flags, resto) = resto.split_first()?;
    let mut r = Lector::new(resto);
    let inquilinos = (0..r.num()?)
        .map(|_| r.num().map(|t| t as u32))
        .collect::<Option<Vec<_>>>()?;
    let datos = descomprimir(flags & 1 != 0, r.buf)?;
    let mut r = Lector::new(&datos);
    let _tipo = r.u8()?;
    let inicio = r.num()?;
    let from = r.texto()?;
    let to = (0..r.num()?)
        .map(|_| r.texto())
        .collect::<Option<Vec<_>>>()?;
    let size = r.num()?;
    let n = r.num()? as usize;
    let mut eventos = Vec::with_capacity(n.min(MAX_EVENTOS));
    for _ in 0..n {
        eventos.push(r.evento(inicio)?);
    }
    Some((
        inquilinos,
        Trace {
            events: List::from_iter(eventos),
            timestamp: UTCDateTime::from_timestamp(inicio as i64),
            from,
            to: to.join(", "),
            size,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_por_fecha() {
        let id = SnowflakeIdGenerator::new().generate();
        let ts = segundos_de_id(id);
        assert!(id_desde(ts) <= id && id < id_desde(ts + 1));
        assert!(id_desde(4_000_000_000) > id);
        assert_eq!(id_desde(0), 0);
    }

    #[test]
    fn resumen_ida_y_vuelta() {
        let r = Resumen {
            tipo: 1,
            timestamp: 1_700_000_000,
            size: 1234,
            from: "a@a.test".into(),
            to: vec!["b@b.test".into(), "c@c.test".into()],
            colas: vec![7, 99],
            inquilinos: vec![3, 4],
            palabras: "a@a.test b@b.test 10.0.0.1 ".into(),
        };
        assert_eq!(Resumen::deserializar(&r.serializar()).unwrap(), r);
    }

    #[test]
    fn intereses_sin_datos_del_protocolo() {
        assert!(interesa(EventType::Smtp(SmtpEvent::MailFrom)));
        assert!(interesa(EventType::Delivery(DeliveryEvent::Completed)));
        assert!(!interesa(EventType::Smtp(SmtpEvent::RawInput)));
        assert!(!interesa(EventType::Imap(trc::ImapEvent::ConnectionStart)));
    }

    #[test]
    fn eventos_desordenados() {
        let ev = |typ, keys: Vec<(Key, Value)>| {
            Arc::new(Event {
                inner: EventDetails {
                    typ,
                    timestamp: 50,
                    level: trc::Level::Info,
                    span: None,
                },
                keys,
            })
        };
        let k = || vec![(Key::SpanId, Value::UInt(9))];
        let mut agrupador = Agrupador::default();
        let mut listas = Vec::new();
        // El encolado y el cierre llegan antes que el inicio (hilos distintos).
        agrupador.procesar(
            vec![
                ev(EventType::Queue(QueueEvent::MessageQueued), k()),
                ev(EventType::Smtp(SmtpEvent::ConnectionEnd), k()),
                ev(EventType::Smtp(SmtpEvent::ConnectionStart), k()),
            ],
            &mut listas,
        );
        assert!(listas.is_empty(), "se espera la gracia antes de cerrar");
        // Un evento tardío (de otro hilo) tras el cierre todavía entra.
        agrupador.procesar(vec![ev(EventType::Smtp(SmtpEvent::Ehlo), k())], &mut listas);
        agrupador.cerrar_vencidas(Instant::now() + GRACIA, &mut listas);
        assert_eq!(listas.len(), 1);
        assert_eq!(listas[0].eventos.len(), 4);
        assert_eq!(
            listas[0].eventos[0],
            EventType::Smtp(SmtpEvent::ConnectionStart)
        );
        assert_eq!(
            listas[0].eventos[3],
            EventType::Smtp(SmtpEvent::ConnectionEnd)
        );
        assert!(agrupador.abiertas.is_empty() && agrupador.huerfanos.is_empty());
        // Los huérfanos que nadie adopta se olvidan.
        agrupador.procesar(
            vec![ev(EventType::Smtp(SmtpEvent::MailFrom), k())],
            &mut listas,
        );
        assert_eq!(agrupador.huerfanos.len(), 1);
        agrupador.limpiar(50 + ESPERA_HUERFANOS + 1);
        assert!(agrupador.huerfanos.is_empty());
    }

    #[test]
    fn traza_completa() {
        let ev = |typ, ts, keys| {
            Arc::new(Event {
                inner: EventDetails {
                    typ,
                    timestamp: ts,
                    level: trc::Level::Info,
                    span: None,
                },
                keys,
            })
        };
        let span = 0x1234_5678_9abc_def0u64;
        let mut agrupador = Agrupador::default();
        let mut listas = Vec::new();
        agrupador.procesar(
            vec![
                ev(
                    EventType::Smtp(SmtpEvent::ConnectionStart),
                    100,
                    vec![(Key::SpanId, Value::UInt(span))],
                ),
                ev(
                    EventType::Queue(QueueEvent::MessageQueued),
                    101,
                    vec![
                        (Key::SpanId, Value::UInt(span)),
                        (Key::QueueId, Value::UInt(42)),
                        (Key::From, Value::String("x@ext.test".into())),
                        (
                            Key::To,
                            Value::Array(vec![Value::String("ana@a.test".into())]),
                        ),
                        (Key::Size, Value::UInt(321)),
                    ],
                ),
                // Un intento de entrega con el mismo spanId que la sesión abierta (generadores distintos).
                ev(
                    EventType::Delivery(DeliveryEvent::AttemptStart),
                    101,
                    vec![(Key::SpanId, Value::UInt(span))],
                ),
                ev(
                    EventType::Delivery(DeliveryEvent::Completed),
                    101,
                    vec![(Key::SpanId, Value::UInt(span))],
                ),
                ev(
                    EventType::Smtp(SmtpEvent::Ehlo),
                    101,
                    vec![(Key::SpanId, Value::UInt(span))],
                ),
                ev(
                    EventType::Delivery(DeliveryEvent::AttemptEnd),
                    101,
                    vec![(Key::SpanId, Value::UInt(span))],
                ),
                ev(
                    EventType::Smtp(SmtpEvent::ConnectionEnd),
                    102,
                    vec![(Key::SpanId, Value::UInt(span))],
                ),
                // Sesión sin mensajes: no se guarda.
                ev(
                    EventType::Smtp(SmtpEvent::ConnectionStart),
                    103,
                    vec![(Key::SpanId, Value::UInt(5))],
                ),
                ev(
                    EventType::Smtp(SmtpEvent::ConnectionEnd),
                    104,
                    vec![(Key::SpanId, Value::UInt(5))],
                ),
            ],
            &mut listas,
        );
        agrupador.cerrar_vencidas(Instant::now() + GRACIA, &mut listas);
        assert!(agrupador.abiertas.is_empty());
        assert_eq!(listas.len(), 2);
        let salida = listas
            .iter()
            .find(|l| l.resumen.tipo() == Tipo::Salida)
            .unwrap();
        assert_eq!(salida.eventos.len(), 3);
        let l = listas
            .iter()
            .find(|l| l.resumen.tipo() == Tipo::Entrada)
            .unwrap();
        assert_eq!(l.resumen.colas, vec![42]);
        assert_eq!(l.resumen.to, vec!["ana@a.test".to_string()]);
        let mut registro = vec![super::super::formato::VERSION, l.comprimida as u8, 0];
        registro.extend_from_slice(&l.cuerpo);
        let (inq, traza) = decodificar(&registro).unwrap();
        assert!(inq.is_empty());
        assert_eq!(traza.from, "x@ext.test");
        assert_eq!(traza.size, 321);
        assert_eq!(traza.events.len(), 4);
    }
}
