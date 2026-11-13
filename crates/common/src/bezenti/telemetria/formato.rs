/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Formato binario compacto de las trazas guardadas (especificación `telemetria.md`, §3).
//!
//! Cada evento se guarda como `id del evento` + `segundos desde el inicio de la traza` + pares clave/valor, con
//! enteros LEB128 y una etiqueta de un byte por valor. Una traza típica de entrada (unos 40 eventos) ocupa
//! 1–3 KiB sin comprimir; por encima de [`UMBRAL_COMPRESION`] se comprime con zstd (el mismo compresor que usa el
//! almacén), lo que la deja en torno a un tercio.

use registry::{
    schema::structs::{
        TraceEvent, TraceKeyValue, TraceValue, TraceValueBoolean, TraceValueDuration,
        TraceValueEvent, TraceValueFloat, TraceValueInteger, TraceValueIpAddr, TraceValueList,
        TraceValueString, TraceValueUTCDateTime, TraceValueUnsignedInt,
    },
    types::{datetime::UTCDateTime, ipaddr::IpAddr, list::List},
};
use std::net::{Ipv4Addr, Ipv6Addr};
use trc::{Event, EventDetails, EventType, Key, Value};
use utils::codec::leb128::{Leb128Reader, Leb128Writer};

/// Versión del formato de los registros.
pub const VERSION: u8 = 1;
/// A partir de este tamaño la carga útil se comprime.
pub const UMBRAL_COMPRESION: usize = 1024;
/// Máximo de niveles de anidamiento al decodificar (eventos dentro de eventos, listas…).
const MAX_PROFUNDIDAD: usize = 8;

const T_NULO: u8 = 0;
const T_TEXTO: u8 = 1;
const T_ENTERO_SIN_SIGNO: u8 = 2;
const T_ENTERO: u8 = 3;
const T_REAL: u8 = 4;
const T_FECHA: u8 = 5;
const T_DURACION: u8 = 6;
const T_BYTES: u8 = 7;
const T_FALSO: u8 = 8;
const T_VERDADERO: u8 = 9;
const T_IPV4: u8 = 10;
const T_IPV6: u8 = 11;
const T_EVENTO: u8 = 12;
const T_LISTA: u8 = 13;

/// Escritor de valores.
#[derive(Default)]
pub struct Escritor {
    pub buf: Vec<u8>,
}

impl Escritor {
    pub fn with_capacity(capacidad: usize) -> Self {
        Escritor {
            buf: Vec::with_capacity(capacidad),
        }
    }

    #[inline]
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    #[inline]
    pub fn num(&mut self, v: u64) {
        self.buf.push_leb128(v);
    }

    pub fn texto(&mut self, v: &str) {
        self.bytes(v.as_bytes());
    }

    pub fn bytes(&mut self, v: &[u8]) {
        self.num(v.len() as u64);
        self.buf.extend_from_slice(v);
    }

    /// Un evento de la traza: id, segundos desde `inicio` y sus claves (sin `spanId`, que es la propia traza).
    pub fn evento(&mut self, evento: &Event<EventDetails>, inicio: u64) {
        self.num(evento.inner.typ.to_id() as u64);
        self.num(evento.inner.timestamp.saturating_sub(inicio));
        self.claves(&evento.keys);
    }

    fn claves(&mut self, claves: &[(Key, Value)]) {
        let claves = claves
            .iter()
            .filter(|(clave, _)| *clave != Key::SpanId)
            .take(u8::MAX as usize)
            .collect::<Vec<_>>();
        self.u8(claves.len() as u8);
        for (clave, valor) in claves {
            self.num(clave.to_id() as u64);
            self.valor(valor, 0);
        }
    }

    fn valor(&mut self, valor: &Value, profundidad: usize) {
        match valor {
            Value::String(v) => {
                self.u8(T_TEXTO);
                self.texto(v);
            }
            Value::UInt(v) => {
                self.u8(T_ENTERO_SIN_SIGNO);
                self.num(*v);
            }
            Value::Int(v) => {
                self.u8(T_ENTERO);
                self.num(((*v << 1) ^ (*v >> 63)) as u64);
            }
            Value::Float(v) => {
                self.u8(T_REAL);
                self.buf.extend_from_slice(&v.to_le_bytes());
            }
            Value::Timestamp(v) => {
                self.u8(T_FECHA);
                self.num(*v);
            }
            Value::Duration(v) => {
                self.u8(T_DURACION);
                self.num(*v);
            }
            Value::Bytes(v) => {
                self.u8(T_BYTES);
                self.bytes(&v[..v.len().min(4096)]);
            }
            Value::Bool(v) => self.u8(if *v { T_VERDADERO } else { T_FALSO }),
            Value::Ipv4(v) => {
                self.u8(T_IPV4);
                self.buf.extend_from_slice(&v.octets());
            }
            Value::Ipv6(v) => {
                self.u8(T_IPV6);
                self.buf.extend_from_slice(&v.octets());
            }
            Value::Event(v) if profundidad < MAX_PROFUNDIDAD => {
                self.u8(T_EVENTO);
                self.num(v.as_ref().to_id() as u64);
                let claves = v.keys();
                self.u8(claves.len().min(u8::MAX as usize) as u8);
                for (clave, valor) in claves.iter().take(u8::MAX as usize) {
                    self.num(clave.to_id() as u64);
                    self.valor(valor, profundidad + 1);
                }
            }
            Value::Array(v) if profundidad < MAX_PROFUNDIDAD => {
                self.u8(T_LISTA);
                self.num(v.len() as u64);
                for valor in v {
                    self.valor(valor, profundidad + 1);
                }
            }
            Value::Event(_) | Value::Array(_) | Value::None => self.u8(T_NULO),
        }
    }
}

/// Lector de valores.
pub struct Lector<'x> {
    pub buf: &'x [u8],
}

impl<'x> Lector<'x> {
    pub fn new(buf: &'x [u8]) -> Self {
        Lector { buf }
    }

    pub fn u8(&mut self) -> Option<u8> {
        let (&v, resto) = self.buf.split_first()?;
        self.buf = resto;
        Some(v)
    }

    pub fn num(&mut self) -> Option<u64> {
        self.buf.read_leb128::<u64>()
    }

    pub fn bytes(&mut self) -> Option<&'x [u8]> {
        let len = self.num()? as usize;
        let (v, resto) = self.buf.split_at_checked(len)?;
        self.buf = resto;
        Some(v)
    }

    pub fn texto(&mut self) -> Option<String> {
        self.bytes()
            .map(|v| String::from_utf8_lossy(v).into_owned())
    }

    fn fijo<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (v, resto) = self.buf.split_at_checked(N)?;
        self.buf = resto;
        v.try_into().ok()
    }

    /// Un evento con el formato de la API (`x:TraceEvent`).
    pub fn evento(&mut self, inicio: u64) -> Option<TraceEvent> {
        let event = EventType::from_id(self.num()? as u16)?;
        let timestamp = inicio + self.num()?;
        let key_values = self.claves(0)?;
        Some(TraceEvent {
            event,
            timestamp: UTCDateTime::from_timestamp(timestamp as i64),
            key_values,
        })
    }

    fn claves(&mut self, profundidad: usize) -> Option<List<TraceKeyValue>> {
        let n = self.u8()? as usize;
        let mut claves = Vec::with_capacity(n);
        for _ in 0..n {
            let clave = Key::from_id(self.num()? as u16);
            let valor = self.valor(profundidad)?;
            if let Some(key) = clave {
                claves.push(TraceKeyValue { key, value: valor });
            }
        }
        Some(List::from_iter(claves))
    }

    fn valor(&mut self, profundidad: usize) -> Option<TraceValue> {
        if profundidad > MAX_PROFUNDIDAD {
            return None;
        }
        Some(match self.u8()? {
            T_NULO => TraceValue::Null,
            T_TEXTO => TraceValue::String(TraceValueString {
                value: self.texto()?,
            }),
            T_ENTERO_SIN_SIGNO => {
                TraceValue::UnsignedInt(TraceValueUnsignedInt { value: self.num()? })
            }
            T_ENTERO => {
                let v = self.num()?;
                TraceValue::Integer(TraceValueInteger {
                    value: ((v >> 1) as i64) ^ -((v & 1) as i64),
                })
            }
            T_REAL => TraceValue::Float(TraceValueFloat {
                value: f64::from_le_bytes(self.fijo::<8>()?).into(),
            }),
            T_FECHA => TraceValue::UTCDateTime(TraceValueUTCDateTime {
                value: UTCDateTime::from_timestamp(self.num()? as i64),
            }),
            T_DURACION => TraceValue::Duration(TraceValueDuration { value: self.num()? }),
            T_BYTES => TraceValue::String(TraceValueString {
                value: String::from_utf8_lossy(self.bytes()?).into_owned(),
            }),
            T_FALSO => TraceValue::Boolean(TraceValueBoolean { value: false }),
            T_VERDADERO => TraceValue::Boolean(TraceValueBoolean { value: true }),
            T_IPV4 => TraceValue::IpAddr(TraceValueIpAddr {
                value: IpAddr(Ipv4Addr::from(self.fijo::<4>()?).into()),
            }),
            T_IPV6 => TraceValue::IpAddr(TraceValueIpAddr {
                value: IpAddr(Ipv6Addr::from(self.fijo::<16>()?).into()),
            }),
            T_EVENTO => {
                let event = EventType::from_id(self.num()? as u16)?;
                TraceValue::Event(TraceValueEvent {
                    event,
                    value: self.claves(profundidad + 1)?,
                })
            }
            T_LISTA => {
                let n = self.num()? as usize;
                let mut valores = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    valores.push(self.valor(profundidad + 1)?);
                }
                TraceValue::List(TraceValueList {
                    value: List::from_iter(valores),
                })
            }
            _ => return None,
        })
    }
}

/// Valor de un evento en vivo con el formato de la API (`x:TraceValue`), sin pasar por el formato binario.
pub fn valor_api(valor: &Value, profundidad: usize) -> TraceValue {
    match valor {
        Value::String(v) => TraceValue::String(TraceValueString {
            value: v.to_string(),
        }),
        Value::UInt(v) => TraceValue::UnsignedInt(TraceValueUnsignedInt { value: *v }),
        Value::Int(v) => TraceValue::Integer(TraceValueInteger { value: *v }),
        Value::Float(v) => TraceValue::Float(TraceValueFloat { value: (*v).into() }),
        Value::Timestamp(v) => TraceValue::UTCDateTime(TraceValueUTCDateTime {
            value: UTCDateTime::from_timestamp(*v as i64),
        }),
        Value::Duration(v) => TraceValue::Duration(TraceValueDuration { value: *v }),
        Value::Bytes(v) => TraceValue::String(TraceValueString {
            value: String::from_utf8_lossy(v).into_owned(),
        }),
        Value::Bool(v) => TraceValue::Boolean(TraceValueBoolean { value: *v }),
        Value::Ipv4(v) => TraceValue::IpAddr(TraceValueIpAddr {
            value: IpAddr((*v).into()),
        }),
        Value::Ipv6(v) => TraceValue::IpAddr(TraceValueIpAddr {
            value: IpAddr((*v).into()),
        }),
        Value::Event(v) if profundidad < MAX_PROFUNDIDAD => TraceValue::Event(TraceValueEvent {
            event: *v.as_ref(),
            value: claves_api(v.keys(), profundidad + 1),
        }),
        Value::Array(v) if profundidad < MAX_PROFUNDIDAD => TraceValue::List(TraceValueList {
            value: List::from_iter(v.iter().map(|v| valor_api(v, profundidad + 1))),
        }),
        Value::Event(_) | Value::Array(_) | Value::None => TraceValue::Null,
    }
}

/// Claves de un evento en vivo con el formato de la API (sin `spanId`).
pub fn claves_api(claves: &[(Key, Value)], profundidad: usize) -> List<TraceKeyValue> {
    List::from_iter(
        claves
            .iter()
            .filter(|(clave, _)| *clave != Key::SpanId)
            .map(|(clave, valor)| TraceKeyValue {
                key: *clave,
                value: valor_api(valor, profundidad),
            }),
    )
}

/// Comprime la carga útil si merece la pena. Devuelve (comprimida, datos).
pub fn comprimir(datos: Vec<u8>) -> (bool, Vec<u8>) {
    if datos.len() >= UMBRAL_COMPRESION
        && let Ok(comprimido) = store::write::compress::compress(None, &datos, 0)
        && comprimido.len() < datos.len()
    {
        (true, comprimido)
    } else {
        (false, datos)
    }
}

pub fn descomprimir(comprimida: bool, datos: &[u8]) -> Option<Vec<u8>> {
    if comprimida {
        store::write::compress::decompress(datos).ok()
    } else {
        Some(datos.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trc::{DeliveryEvent, SmtpEvent};

    #[test]
    fn ida_y_vuelta() {
        let evento = Event {
            inner: EventDetails {
                typ: EventType::Smtp(SmtpEvent::MailFrom),
                timestamp: 1_000_005,
                level: trc::Level::Info,
                span: None,
            },
            keys: vec![
                (Key::SpanId, Value::UInt(7)),
                (Key::From, Value::String("ana@a.test".into())),
                (Key::Size, Value::UInt(1234)),
                (Key::Code, Value::Int(-3)),
                (Key::Elapsed, Value::Duration(15)),
                (Key::RemoteIp, Value::Ipv4(Ipv4Addr::new(10, 1, 2, 3))),
                (Key::Details, Value::Float(1.5)),
                (Key::Result, Value::Bool(true)),
                (
                    Key::To,
                    Value::Array(vec![
                        Value::String("b@b.test".into()),
                        Value::String("c@c.test".into()),
                    ]),
                ),
                (
                    Key::CausedBy,
                    Value::Event(trc::Error::new(EventType::Delivery(
                        DeliveryEvent::ConnectError,
                    ))),
                ),
            ],
        };
        let mut w = Escritor::default();
        w.evento(&evento, 1_000_000);
        let mut r = Lector::new(&w.buf);
        let ev = r.evento(1_000_000).unwrap();
        assert!(r.buf.is_empty());
        assert_eq!(ev.event, EventType::Smtp(SmtpEvent::MailFrom));
        assert_eq!(ev.timestamp, UTCDateTime::from_timestamp(1_000_005));
        let esperado = claves_api(&evento.keys, 0);
        assert_eq!(ev.key_values, esperado);
        assert!(!ev.key_values.iter().any(|kv| kv.key == Key::SpanId));
    }

    #[test]
    fn compresion() {
        let datos = b"ana@a.test ".repeat(500);
        let (c, comprimido) = comprimir(datos.clone());
        assert!(c && comprimido.len() < datos.len());
        assert_eq!(descomprimir(c, &comprimido).unwrap(), datos);
        let (c, corto) = comprimir(b"hola".to_vec());
        assert!(!c && corto == b"hola");
    }
}
