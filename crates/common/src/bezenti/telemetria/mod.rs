/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Telemetría con historial y alertas (edición bezenti, fase 5).
//!
//! Especificación: `bezenti/especificaciones/telemetria.md`. Piezas:
//!
//! - [`trazas`]: historial de entregas (`x:Trace`): un suscriptor del colector de eventos agrupa por sesión los
//!   eventos de cada sesión SMTP que encola un mensaje y de cada intento de entrega, y los escribe **por lotes**
//!   en el almacén de trazas, fuera del camino del correo.
//! - [`metricas`]: historial de métricas (`x:Metric`): un registro compacto por nodo y por intervalo.
//! - [`vivo`]: filtros de la telemetría en vivo (`/api/live/tracing`).
//! - [`formato`]: el formato binario de los eventos guardados.
//!
//! La configuración ([`ConfigTelemetria`]) se lee con el resto del `Core`; los bucles periódicos (recoger métricas
//! y evaluar alertas) viven en el crate `services`.

pub mod formato;
pub mod metricas;
pub mod trazas;
pub mod vivo;

use crate::{
    BuildServer, Inner, Server,
    expr::if_block::{BootstrapExprExt, IfBlock},
};
use parking_lot::Mutex;
use registry::schema::{
    prelude::ObjectType,
    structs::{
        Alert, AlertEmail, AlertEvent, DataRetention, DataStore, MetricsStore, TracingStore,
    },
};
use std::{
    sync::{Arc, OnceLock, Weak},
    time::Duration,
};
use store::{Store, registry::bootstrap::Bootstrap};
use utils::cron::SimpleCron;

/// Intervalo de evaluación de las alertas por defecto (documentación: cada cinco minutos).
pub const INTERVALO_ALERTAS: Duration = Duration::from_secs(5 * 60);

/// Variables de entorno para pruebas y ajuste fino (no forman parte de la API).
pub const ENV_INTERVALO_METRICAS: &str = "BEZENTI_METRICAS_INTERVALO";
pub const ENV_INTERVALO_ALERTAS: &str = "BEZENTI_ALERTAS_INTERVALO";
pub const ENV_VOLCADO_TRAZAS: &str = "BEZENTI_TRAZAS_VOLCADO";

/// Configuración de la telemetría con historial (parte del `Core`).
#[derive(Clone, Default)]
pub struct ConfigTelemetria {
    /// Almacén de las trazas (`None`: `x:TracingStore` es `Disabled`).
    pub trazas: Option<Store>,
    /// Almacén de las métricas (`None`: `x:MetricsStore` es `Disabled`).
    pub metricas: Option<Store>,
    /// `holdTracesFor` en segundos (`None`: no se borran).
    pub retencion_trazas: Option<u64>,
    /// `holdMetricsFor` en segundos (`None`: no se borran).
    pub retencion_metricas: Option<u64>,
    /// `metricsCollectionInterval`.
    pub intervalo_metricas: Option<SimpleCron>,
    /// Alertas activas, ya compiladas.
    pub alertas: Arc<Vec<Alerta>>,
}

/// Una alerta (`x:Alert`) lista para evaluar.
#[derive(Debug, Clone)]
pub struct Alerta {
    pub id: u64,
    pub condicion: IfBlock,
    pub correo: Option<AlertaCorreo>,
    /// `eventAlert` activado, con su mensaje (puede ser vacío).
    pub evento: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AlertaCorreo {
    pub de: String,
    pub nombre: Option<String>,
    pub para: Vec<String>,
    pub asunto: String,
    pub cuerpo: String,
}

impl ConfigTelemetria {
    pub async fn parse(bp: &mut Bootstrap, datos: &Store) -> Self {
        if bp.registry.is_recovery_mode() {
            return ConfigTelemetria::default();
        }
        let dr = bp.setting_infallible::<DataRetention>().await;
        let metricas = match bp.setting_infallible::<MetricsStore>().await {
            MetricsStore::Disabled => Opcion::Ninguno,
            MetricsStore::Default => Opcion::Datos,
            MetricsStore::FoundationDb(s) => Opcion::Aparte(Box::new(DataStore::FoundationDb(s))),
            MetricsStore::PostgreSql(s) => Opcion::Aparte(Box::new(DataStore::PostgreSql(s))),
            MetricsStore::MySql(s) => Opcion::Aparte(Box::new(DataStore::MySql(s))),
        };

        let mut alertas = Vec::new();
        for alerta in bp.list_infallible::<Alert>().await {
            let id = alerta.id;
            let alerta = alerta.object;
            if !alerta.enable {
                continue;
            }
            let condicion = bp.compile_expr(id, &alerta.ctx_condition());
            if condicion.is_empty() {
                bp.build_warning(id, "Alert without a valid condition, ignored");
                continue;
            }
            let correo = match alerta.email_alert {
                AlertEmail::Enabled(c) => Some(AlertaCorreo {
                    de: c.from_address,
                    nombre: c.from_name.filter(|n| !n.is_empty()),
                    para: c.to.into_iter().collect(),
                    asunto: c.subject,
                    cuerpo: c.body,
                }),
                AlertEmail::Disabled => None,
            };
            let evento = match alerta.event_alert {
                AlertEvent::Enabled(e) => Some(e.event_message.unwrap_or_default()),
                AlertEvent::Disabled => None,
            };
            if correo.is_none() && evento.is_none() {
                bp.build_warning(id, "Alert without notifications, ignored");
                continue;
            }
            alertas.push(Alerta {
                id: id.id().id(),
                condicion,
                correo,
                evento,
            });
        }

        ConfigTelemetria {
            trazas: almacen_trazas(bp, datos).await.map(|(s, _)| s),
            metricas: almacen(bp, datos, metricas, ObjectType::MetricsStore).await,
            retencion_trazas: dr.hold_traces_for.map(|d| d.into_inner().as_secs().max(1)),
            retencion_metricas: dr.hold_metrics_for.map(|d| d.into_inner().as_secs().max(1)),
            intervalo_metricas: Some(dr.metrics_collection_interval.into()),
            alertas: Arc::new(alertas),
        }
    }
}

/// Almacén de las trazas según `x:TracingStore` y una huella de su configuración.
pub async fn almacen_trazas(bp: &mut Bootstrap, datos: &Store) -> Option<(Store, u64)> {
    let config = bp.setting_infallible::<TracingStore>().await;
    let huella = {
        use std::hash::{Hash, Hasher};
        let mut h = std::hash::DefaultHasher::new();
        serde_json::to_string(&config)
            .unwrap_or_default()
            .hash(&mut h);
        h.finish()
    };
    let opcion = match config {
        TracingStore::Disabled => Opcion::Ninguno,
        TracingStore::Default => Opcion::Datos,
        TracingStore::FoundationDb(s) => Opcion::Aparte(Box::new(DataStore::FoundationDb(s))),
        TracingStore::PostgreSql(s) => Opcion::Aparte(Box::new(DataStore::PostgreSql(s))),
        TracingStore::MySql(s) => Opcion::Aparte(Box::new(DataStore::MySql(s))),
    };
    almacen(bp, datos, opcion, ObjectType::TracingStore)
        .await
        .map(|s| (s, huella))
}

enum Opcion {
    Ninguno,
    Datos,
    Aparte(Box<DataStore>),
}

/// Almacenes aparte ya abiertos, para no abrir otra conexión en cada recarga de la configuración.
static ALMACENES: Mutex<Vec<(String, Store)>> = Mutex::new(Vec::new());

async fn almacen(
    bp: &mut Bootstrap,
    datos: &Store,
    opcion: Opcion,
    objeto: ObjectType,
) -> Option<Store> {
    match opcion {
        Opcion::Ninguno => None,
        Opcion::Datos => Some(datos.clone()),
        Opcion::Aparte(config) => {
            let clave = serde_json::to_string(&config).unwrap_or_default();
            if let Some((_, store)) = ALMACENES.lock().iter().find(|(c, _)| *c == clave) {
                return Some(store.clone());
            }
            match Store::build(*config).await {
                Ok(store) => {
                    ALMACENES.lock().push((clave, store.clone()));
                    Some(store)
                }
                Err(err) => {
                    bp.build_error(
                        objeto.singleton(),
                        format!("No se pudo abrir el almacén, se usa el de datos: {err}"),
                    );
                    Some(datos.clone())
                }
            }
        }
    }
}

/// El servidor en marcha, para las tareas de telemetría que nacen antes que él (el suscriptor del historial se
/// crea al leer la configuración, antes de construir `Inner`).
static INNER: OnceLock<Weak<Inner>> = OnceLock::new();

pub fn registrar_servidor(inner: &Arc<Inner>) {
    let _ = INNER.set(Arc::downgrade(inner));
}

pub fn servidor() -> Option<Server> {
    INNER.get()?.upgrade().map(|inner| inner.build_server())
}

/// Segundos de una variable de entorno (para pruebas).
pub fn segundos_env(nombre: &str) -> Option<Duration> {
    std::env::var(nombre)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .map(Duration::from_secs)
}

/// Id del nodo para el historial: 0 en un servidor sin clúster (documentación de `x:Metric`).
pub fn nodo(server: &Server) -> u16 {
    if server.core.storage.coordinator.is_enabled() {
        server.registry().node_id()
    } else {
        0
    }
}

/// Fase 8: clave del estado de las alertas disparadas de un nodo, en el almacén de datos (subespacio de métricas,
/// con un prefijo que la purga de métricas no toca: `[2] nodo`).
fn clave_alertas(nodo: u16) -> store::write::ValueClass {
    let mut key = Vec::with_capacity(3);
    key.push(2u8);
    key.extend_from_slice(&nodo.to_be_bytes());
    store::write::ValueClass::Any(store::write::AnyClass {
        subspace: store::Subspace::TelemetryMetric,
        key,
    })
}

impl Server {
    /// Fase 8: alertas que estaban disparadas en este nodo antes de reiniciar (para no repetir el aviso).
    pub async fn alertas_disparadas(&self) -> trc::Result<Vec<u64>> {
        use trc::AddContext;
        Ok(self
            .store()
            .get_value::<store::write::serialize::RawValue>(store::ValueKey::from(clave_alertas(
                nodo(self),
            )))
            .await
            .caused_by(trc::location!())?
            .map(|raw| {
                let (ids, _) = raw.0.as_chunks::<8>();
                ids.iter().map(|id| u64::from_be_bytes(*id)).collect()
            })
            .unwrap_or_default())
    }

    /// Fase 8: guarda las alertas disparadas de este nodo.
    pub async fn guardar_alertas_disparadas(&self, ids: &[u64]) -> trc::Result<()> {
        use trc::AddContext;
        let mut batch = store::write::BatchBuilder::new();
        if ids.is_empty() {
            batch.clear(clave_alertas(nodo(self)));
        } else {
            batch.set(
                clave_alertas(nodo(self)),
                ids.iter()
                    .flat_map(|id| id.to_be_bytes())
                    .collect::<Vec<u8>>(),
            );
        }
        self.store()
            .write_batch(&mut batch)
            .await
            .caused_by(trc::location!())
            .map(|_| ())
    }
}
