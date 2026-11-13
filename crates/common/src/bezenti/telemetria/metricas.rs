/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Historial de métricas (`x:Metric`). Especificación `bezenti/especificaciones/telemetria.md`, §4.
//!
//! En cada intervalo (`metricsCollectionInterval`) cada nodo escribe **un solo registro** con todas sus métricas:
//! los contadores e histogramas como diferencia con el registro anterior (solo los que cambiaron) y los medidores
//! con su valor. Los inquilinos tienen además su propio registro con los contadores de los eventos de sus
//! trazas (ver [`contar_inquilino`]): es lo único que ve el administrador de un inquilino.
//!
//! Claves en el subespacio `TelemetryMetric`: `[0] segundos nodo` → registro del servidor;
//! `[1] inquilino segundos nodo` → registro del inquilino. Cada punto de la API (`x:Metric`) tiene por id
//! `segundos << 22 | nodo << 12 | métrica`, así que `get` lee un solo registro.

use super::formato::{Escritor, Lector};
use crate::{Server, bezenti::inquilino::TenantScope};
use parking_lot::Mutex;
use registry::{
    schema::structs::{Metric, MetricCount, MetricSum},
    types::datetime::UTCDateTime,
};
use std::sync::LazyLock;
use store::{
    IterateParams, Store, Subspace, ValueKey,
    ahash::AHashMap,
    write::{AnyClass, AnyKey, BatchBuilder, ValueClass, serialize::RawValue},
};
use trc::{AddContext, Collector, EventType, MetricType, TOTAL_EVENT_COUNT};

const P_SERVIDOR: u8 = 0;
const P_INQUILINO: u8 = 1;
/// Máximo de puntos que devuelve una consulta.
pub const MAX_RESULTADOS: usize = 50_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Clase {
    Contador = 0,
    Medidor = 1,
    Histograma = 2,
}

/// Un punto de un registro.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entrada {
    pub metrica: MetricType,
    pub clase: Clase,
    pub count: u64,
    pub sum: u64,
}

/// Métrica de contador de cada evento (para los contadores por inquilino).
static CONTADOR_DE_EVENTO: LazyLock<Vec<Option<MetricType>>> = LazyLock::new(|| {
    let mut tabla = vec![None; TOTAL_EVENT_COUNT];
    for m in MetricType::variants() {
        let e = m.event_id();
        if e < TOTAL_EVENT_COUNT {
            tabla[e] = Some(*m);
        }
    }
    tabla
});

/// Métrica de contador de un evento, si la tiene.
pub fn metrica_de_evento(evento: EventType) -> Option<MetricType> {
    CONTADOR_DE_EVENTO
        .get(evento.to_id() as usize)
        .copied()
        .flatten()
}

/// Contadores por inquilino pendientes de guardar: inquilino → métrica → cuenta.
static INQUILINOS: Mutex<Vec<(u32, AHashMap<MetricType, u64>)>> = Mutex::new(Vec::new());

/// Suma los eventos de una traza a los contadores de un inquilino.
pub fn contar_inquilino(tenant: u32, eventos: impl Iterator<Item = EventType>) {
    let mut inquilinos = INQUILINOS.lock();
    let pos = match inquilinos.iter().position(|(t, _)| *t == tenant) {
        Some(pos) => pos,
        None => {
            inquilinos.push((tenant, AHashMap::new()));
            inquilinos.len() - 1
        }
    };
    let contadores = &mut inquilinos[pos].1;
    for evento in eventos {
        if let Some(Some(m)) = CONTADOR_DE_EVENTO.get(evento.to_id() as usize) {
            *contadores.entry(*m).or_default() += 1;
        }
    }
}

/// Saca los contadores por inquilino acumulados desde la última vez.
pub fn tomar_inquilinos() -> Vec<(u32, Vec<Entrada>)> {
    std::mem::take(&mut *INQUILINOS.lock())
        .into_iter()
        .map(|(t, c)| {
            (
                t,
                c.into_iter()
                    .map(|(metrica, count)| Entrada {
                        metrica,
                        clase: Clase::Contador,
                        count,
                        sum: 0,
                    })
                    .collect(),
            )
        })
        .collect()
}

/// Lleva los valores del muestreo anterior para guardar diferencias.
#[derive(Default)]
pub struct Recolector {
    anterior: AHashMap<MetricType, (u64, u64)>,
}

impl Recolector {
    /// Toma una muestra de todas las métricas del proceso.
    pub fn muestrear(&mut self) -> Vec<Entrada> {
        let mut entradas = Vec::with_capacity(128);
        for c in Collector::collect_counters(true) {
            let Some(Some(metrica)) = CONTADOR_DE_EVENTO.get(c.id().to_id() as usize) else {
                continue;
            };
            let anterior = self
                .anterior
                .insert(*metrica, (c.value(), 0))
                .unwrap_or_default();
            let delta = c.value().saturating_sub(anterior.0);
            if delta > 0 {
                entradas.push(Entrada {
                    metrica: *metrica,
                    clase: Clase::Contador,
                    count: delta,
                    sum: 0,
                });
            }
        }
        for g in Collector::collect_gauges(true) {
            if g.id() == MetricType::CacheSwapPending {
                continue;
            }
            entradas.push(Entrada {
                metrica: g.id(),
                clase: Clase::Medidor,
                count: g.get(),
                sum: 0,
            });
        }
        for h in Collector::collect_histograms(true) {
            let (count, sum) = (h.count(), h.sum());
            let anterior = self
                .anterior
                .insert(h.id(), (count, sum))
                .unwrap_or_default();
            let dc = count.saturating_sub(anterior.0);
            if dc > 0 {
                entradas.push(Entrada {
                    metrica: h.id(),
                    clase: Clase::Histograma,
                    count: dc,
                    sum: sum.saturating_sub(anterior.1),
                });
            }
        }
        entradas
    }
}

fn serializar(entradas: &[Entrada]) -> Vec<u8> {
    let mut w = Escritor::with_capacity(entradas.len() * 6 + 4);
    w.num(entradas.len() as u64);
    for e in entradas {
        w.num(e.metrica.to_id() as u64);
        w.u8(e.clase as u8);
        w.num(e.count);
        if e.clase == Clase::Histograma {
            w.num(e.sum);
        }
    }
    w.buf
}

fn deserializar(datos: &[u8]) -> Vec<Entrada> {
    let mut r = Lector::new(datos);
    let mut entradas = Vec::new();
    let Some(n) = r.num() else {
        return entradas;
    };
    for _ in 0..n {
        let (Some(id), Some(clase), Some(count)) = (r.num(), r.u8(), r.num()) else {
            break;
        };
        let clase = match clase {
            0 => Clase::Contador,
            1 => Clase::Medidor,
            _ => Clase::Histograma,
        };
        let sum = if clase == Clase::Histograma {
            r.num().unwrap_or_default()
        } else {
            0
        };
        if let Some(metrica) = MetricType::from_id(id as u16) {
            entradas.push(Entrada {
                metrica,
                clase,
                count,
                sum,
            });
        }
    }
    entradas
}

fn clave_servidor(ts: u64, nodo: u16) -> Vec<u8> {
    let mut k = Vec::with_capacity(11);
    k.push(P_SERVIDOR);
    k.extend_from_slice(&ts.to_be_bytes());
    k.extend_from_slice(&nodo.to_be_bytes());
    k
}

fn clave_inquilino(tenant: u32, ts: u64, nodo: u16) -> Vec<u8> {
    let mut k = Vec::with_capacity(15);
    k.push(P_INQUILINO);
    k.extend_from_slice(&tenant.to_be_bytes());
    k.extend_from_slice(&ts.to_be_bytes());
    k.extend_from_slice(&nodo.to_be_bytes());
    k
}

fn clase_valor(key: Vec<u8>) -> ValueClass {
    ValueClass::Any(AnyClass {
        subspace: Subspace::TelemetryMetric,
        key,
    })
}

fn rango(desde: Vec<u8>, hasta: Vec<u8>) -> IterateParams<AnyKey<Vec<u8>>> {
    IterateParams::new(
        AnyKey {
            subspace: Subspace::TelemetryMetric,
            key: desde,
        },
        AnyKey {
            subspace: Subspace::TelemetryMetric,
            key: hasta,
        },
    )
}

/// Id de un punto de la API.
pub fn id_punto(ts: u64, nodo: u16, metrica: MetricType) -> u64 {
    (ts << 22) | (((nodo as u64) & 0x3FF) << 12) | (metrica.to_id() as u64 & 0xFFF)
}

/// (segundos, nodo, métrica) de un id.
pub fn partes_id(id: u64) -> Option<(u64, u16, MetricType)> {
    Some((
        id >> 22,
        ((id >> 12) & 0x3FF) as u16,
        MetricType::from_id((id & 0xFFF) as u16)?,
    ))
}

fn punto(ts: u64, nodo: u16, e: &Entrada) -> Metric {
    let timestamp = UTCDateTime::from_timestamp(ts as i64);
    match e.clase {
        Clase::Contador => Metric::Counter(MetricCount {
            count: e.count,
            metric: e.metrica,
            node_id: nodo as u64,
            timestamp,
        }),
        Clase::Medidor => Metric::Gauge(MetricCount {
            count: e.count,
            metric: e.metrica,
            node_id: nodo as u64,
            timestamp,
        }),
        Clase::Histograma => Metric::Histogram(MetricSum {
            count: e.count,
            sum: e.sum,
            metric: e.metrica,
            node_id: nodo as u64,
            timestamp,
        }),
    }
}

/// Filtros de `x:Metric/query`.
#[derive(Debug, Clone, Default)]
pub struct FiltroMetricas {
    pub desde: Option<u64>,
    pub hasta: Option<u64>,
    pub nodo: Option<u16>,
    pub metricas: Vec<MetricType>,
    pub ascendente: bool,
}

impl FiltroMetricas {
    fn admite(&self, nodo: u16, metrica: MetricType) -> bool {
        self.nodo.is_none_or(|n| n == nodo)
            && (self.metricas.is_empty() || self.metricas.contains(&metrica))
    }
}

fn ts_nodo(key: &[u8], desde: usize) -> Option<(u64, u16)> {
    let ts = u64::from_be_bytes(key.get(desde..desde + 8)?.try_into().ok()?);
    let nodo = u16::from_be_bytes(key.get(desde + 8..desde + 10)?.try_into().ok()?);
    Some((ts, nodo))
}

impl Server {
    fn almacen_metricas(&self) -> trc::Result<&Store> {
        self.core.telemetria.metricas.as_ref().ok_or_else(|| {
            trc::JmapEvent::Forbidden
                .into_err()
                .details("Metrics history is disabled (x:MetricsStore)")
        })
    }

    /// Guarda una muestra del servidor y los contadores de los inquilinos en un solo lote.
    pub async fn guardar_metricas(
        &self,
        ts: u64,
        nodo: u16,
        servidor: &[Entrada],
        inquilinos: &[(u32, Vec<Entrada>)],
    ) -> trc::Result<()> {
        let store = self.almacen_metricas()?;
        let mut batch = BatchBuilder::new();
        batch.set(clase_valor(clave_servidor(ts, nodo)), serializar(servidor));
        for (t, entradas) in inquilinos {
            if !entradas.is_empty() {
                batch.set(
                    clase_valor(clave_inquilino(*t, ts, nodo)),
                    serializar(entradas),
                );
            }
        }
        store
            .write_batch(&mut batch)
            .await
            .caused_by(trc::location!())
            .map(|_| ())
    }

    /// Un punto por id; con ámbito, la suma de los inquilinos del ámbito.
    pub async fn metrica(
        &self,
        id: u64,
        ambito: Option<&TenantScope>,
    ) -> trc::Result<Option<Metric>> {
        let store = self.almacen_metricas()?;
        let Some((ts, nodo, metrica)) = partes_id(id) else {
            return Ok(None);
        };
        let claves = match ambito {
            None => vec![clave_servidor(ts, nodo)],
            Some(a) => a
                .ids()
                .iter()
                .map(|t| clave_inquilino(*t, ts, nodo))
                .collect(),
        };
        let mut total: Option<Entrada> = None;
        for k in claves {
            if let Some(v) = store
                .get_value::<RawValue>(ValueKey::from(clase_valor(k)))
                .await
                .caused_by(trc::location!())?
                && let Some(e) = deserializar(&v.0)
                    .into_iter()
                    .find(|e| e.metrica == metrica)
            {
                match &mut total {
                    Some(t) => {
                        t.count += e.count;
                        t.sum += e.sum;
                    }
                    None => total = Some(e),
                }
            }
        }
        Ok(total.map(|e| punto(ts, nodo, &e)))
    }

    /// Ids de los puntos que cumplen el filtro (con ámbito, los de los inquilinos del ámbito sumados).
    pub async fn consultar_metricas(
        &self,
        filtro: &FiltroMetricas,
        ambito: Option<&TenantScope>,
    ) -> trc::Result<Vec<u64>> {
        let store = self.almacen_metricas()?;
        let desde = filtro.desde.unwrap_or(0);
        let hasta = filtro.hasta.unwrap_or(u64::MAX >> 22);
        let mut ids = Vec::new();
        match ambito {
            None => {
                store
                    .iterate(
                        rango(clave_servidor(desde, 0), clave_servidor(hasta, u16::MAX))
                            .set_ascending(filtro.ascendente),
                        |key, value| {
                            if let Some((ts, nodo)) = ts_nodo(key, 1) {
                                for e in deserializar(value) {
                                    if filtro.admite(nodo, e.metrica) {
                                        ids.push(id_punto(ts, nodo, e.metrica));
                                    }
                                }
                            }
                            Ok(ids.len() < MAX_RESULTADOS)
                        },
                    )
                    .await
                    .caused_by(trc::location!())?;
            }
            Some(ambito) => {
                for t in ambito.ids() {
                    store
                        .iterate(
                            rango(
                                clave_inquilino(*t, desde, 0),
                                clave_inquilino(*t, hasta, u16::MAX),
                            ),
                            |key, value| {
                                if let Some((ts, nodo)) = ts_nodo(key, 5) {
                                    for e in deserializar(value) {
                                        if filtro.admite(nodo, e.metrica) {
                                            ids.push(id_punto(ts, nodo, e.metrica));
                                        }
                                    }
                                }
                                Ok(ids.len() < MAX_RESULTADOS)
                            },
                        )
                        .await
                        .caused_by(trc::location!())?;
                }
                ids.sort_unstable();
                ids.dedup();
                if !filtro.ascendente {
                    ids.reverse();
                }
            }
        }
        Ok(ids)
    }

    /// Borra los registros anteriores a `holdMetricsFor`. Devuelve cuántos.
    pub async fn purgar_metricas(&self) -> trc::Result<u64> {
        let (Some(store), Some(retencion)) = (
            self.core.telemetria.metricas.as_ref(),
            self.core.telemetria.retencion_metricas,
        ) else {
            return Ok(0);
        };
        let corte = store::write::now().saturating_sub(retencion);
        let mut borrar = Vec::new();
        store
            .iterate(
                rango(
                    clave_servidor(0, 0),
                    clave_inquilino(u32::MAX, u64::MAX, u16::MAX),
                )
                .no_values(),
                |key, _| {
                    let desde = if key.first() == Some(&P_SERVIDOR) {
                        1
                    } else {
                        5
                    };
                    if let Some((ts, _)) = ts_nodo(key, desde)
                        && ts < corte
                    {
                        borrar.push(key.to_vec());
                    }
                    Ok(true)
                },
            )
            .await
            .caused_by(trc::location!())?;
        let total = borrar.len() as u64;
        for trozo in borrar.chunks(1000) {
            let mut batch = BatchBuilder::new();
            for k in trozo {
                batch.clear(clase_valor(k.clone()));
            }
            store
                .write_batch(&mut batch)
                .await
                .caused_by(trc::location!())?;
        }
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registro_ida_y_vuelta() {
        let entradas = vec![
            Entrada {
                metrica: MetricType::QueueCount,
                clase: Clase::Medidor,
                count: 12,
                sum: 0,
            },
            Entrada {
                metrica: MetricType::DeliveryAttemptTime,
                clase: Clase::Histograma,
                count: 3,
                sum: 900,
            },
        ];
        assert_eq!(deserializar(&serializar(&entradas)), entradas);
    }

    #[test]
    fn ids_de_puntos() {
        let id = id_punto(1_763_000_000, 3, MetricType::QueueCount);
        assert_eq!(
            partes_id(id),
            Some((1_763_000_000, 3, MetricType::QueueCount))
        );
        assert!(id < (1u64 << 54));
    }

    #[test]
    fn contadores_por_inquilino() {
        contar_inquilino(
            77,
            [
                EventType::Delivery(trc::DeliveryEvent::Completed),
                EventType::Delivery(trc::DeliveryEvent::Completed),
            ]
            .into_iter(),
        );
        let tomados = tomar_inquilinos();
        let (_, e) = tomados.iter().find(|(t, _)| *t == 77).unwrap();
        assert!(e.iter().any(|e| e.count == 2 && e.clase == Clase::Contador));
    }
}
