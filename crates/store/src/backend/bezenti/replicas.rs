/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Réplicas de lectura de PostgreSQL y MySQL/MariaDB (edición bezenti, fase 6).
//!
//! Resumen (detalle en `bezenti/especificaciones/replicas-y-reparto.md`):
//!
//! - Todas las escrituras, y las lecturas que se hacen DENTRO de una escritura (aserciones, fusiones,
//!   reservas de identificadores), van al primario: las hace el propio motor en su transacción.
//! - Las lecturas sueltas (`get_value`, `key_exists`, `iterate`, `iterate_many`, `get_counter` y los blobs)
//!   se reparten por turnos entre las réplicas **sanas y frescas**; si no hay ninguna, van al primario.
//!   `get_value_fresh`, las consultas SQL libres (`sql_query`) y la búsqueda de texto van siempre al primario.
//! - **Lee lo que escribiste**: este nodo apunta, por *partición* (los 4 primeros bytes de la clave), la
//!   última escritura que la tocó. Una réplica solo sirve una lectura si ya contiene esa escritura, lo que se
//!   comprueba comparando posiciones de replicación (LSN de PostgreSQL, GTID de MySQL/MariaDB) cada
//!   [`INTERVALO`]. Así, tras escribir en una cuenta, sus lecturas van al primario unos cientos de
//!   milisegundos y las de las demás cuentas siguen en las réplicas.
//! - **Salud**: una réplica que falla una lectura se marca caída y esa lectura se repite en el primario; la
//!   comprobación periódica la vuelve a dar por buena cuando responde. Una réplica que se atrasa más de
//!   [`MAX_RETRASO`] (o cuya posición no se puede verificar) deja de usarse hasta que se pone al día.

use crate::{
    Deserialize, IndexKey, IterateParams, Key, QueryResult, Store, Value, ValueKey,
    write::{AssignedIds, Batch, Operation, PendingId, ValueClass},
};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use xxhash_rust::xxh3::xxh3_64;

/// Número de particiones del registro de escrituras (más particiones = menos falsos positivos).
pub(crate) const PARTICIONES: usize = 4096;
/// Cada cuánto se comparan las posiciones del primario y de las réplicas.
pub const INTERVALO: Duration = Duration::from_millis(200);
/// Retraso máximo tolerado: una réplica que no se ha visto al día en este tiempo deja de usarse.
pub const MAX_RETRASO: Duration = Duration::from_secs(5);
/// Tiempo máximo de cada consulta de posición.
const TIEMPO_COMPROBACION: Duration = Duration::from_secs(2);
/// Posiciones del primario que se recuerdan (64 × 200 ms ≈ 13 s, más que `MAX_RETRASO`).
const HISTORIAL: usize = 64;
/// Marca de «nunca vista al día».
const NUNCA: u64 = u64::MAX;

/// Un servidor SQL concreto (primario o réplica). Se llama al motor directamente, no a través de [`Store`],
/// para no crear recursión entre futuros asíncronos.
#[derive(Clone)]
pub enum MiembroSql {
    #[cfg(feature = "postgres")]
    Postgres(Arc<crate::backend::postgres::PostgresStore>),
    #[cfg(feature = "mysql")]
    Mysql(Arc<crate::backend::mysql::MysqlStore>),
}

/// Posición de replicación de un servidor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Posicion {
    /// PostgreSQL: LSN (en el primario `pg_current_wal_lsn()`, en la réplica `pg_last_wal_replay_lsn()`).
    Lsn(u64),
    /// MySQL con `gtid_mode = ON`: conjunto de GTID ejecutados, por servidor de origen.
    GtidMysql(Vec<(String, Vec<(u64, u64)>)>),
    /// MariaDB: último número de secuencia por dominio (`gtid_binlog_pos` / `gtid_slave_pos`).
    GtidMariadb(Vec<(u32, u64)>),
}

impl Posicion {
    /// `true` si un servidor en la posición `self` ya contiene todo lo que había en `otra`.
    pub fn contiene(&self, otra: &Posicion) -> bool {
        match (self, otra) {
            (Posicion::Lsn(a), Posicion::Lsn(b)) => a >= b,
            (Posicion::GtidMariadb(a), Posicion::GtidMariadb(b)) => b
                .iter()
                .all(|(dominio, seq)| a.iter().any(|(d, s)| d == dominio && s >= seq)),
            (Posicion::GtidMysql(a), Posicion::GtidMysql(b)) => b.iter().all(|(origen, tramos)| {
                let Some((_, propios)) = a.iter().find(|(o, _)| o.eq_ignore_ascii_case(origen))
                else {
                    return tramos.is_empty();
                };
                tramos
                    .iter()
                    .all(|(desde, hasta)| propios.iter().any(|(d, h)| d <= desde && hasta <= h))
            }),
            _ => false,
        }
    }

    /// Conjunto de GTID de MySQL (`uuid:1-5:7,uuid2:1-3`). Los tramos contiguos se funden.
    pub fn gtid_mysql(texto: &str) -> Option<Posicion> {
        let mut origenes = Vec::new();
        for parte in texto.split(',').map(|p| p.trim()).filter(|p| !p.is_empty()) {
            let mut trozos = parte.split(':');
            let origen = trozos.next()?.trim().to_ascii_lowercase();
            let mut tramos: Vec<(u64, u64)> = Vec::new();
            for tramo in trozos {
                let tramo = tramo.trim();
                // Etiquetas de MySQL 8.3+ (`uuid:etiqueta:1-5`): se tratan como un origen aparte.
                if tramo.chars().next().is_some_and(|c| !c.is_ascii_digit()) {
                    return None;
                }
                let (desde, hasta) = match tramo.split_once('-') {
                    Some((d, h)) => (d.parse().ok()?, h.parse().ok()?),
                    None => {
                        let n = tramo.parse().ok()?;
                        (n, n)
                    }
                };
                tramos.push((desde, hasta));
            }
            tramos.sort_unstable();
            let mut fundidos: Vec<(u64, u64)> = Vec::with_capacity(tramos.len());
            for (d, h) in tramos {
                match fundidos.last_mut() {
                    Some(ultimo) if d <= ultimo.1.saturating_add(1) => ultimo.1 = ultimo.1.max(h),
                    _ => fundidos.push((d, h)),
                }
            }
            origenes.push((origen, fundidos));
        }
        Some(Posicion::GtidMysql(origenes))
    }

    /// Posición GTID de MariaDB (`dominio-servidor-secuencia,...`).
    pub fn gtid_mariadb(texto: &str) -> Option<Posicion> {
        let mut dominios: Vec<(u32, u64)> = Vec::new();
        for parte in texto.split(',').map(|p| p.trim()).filter(|p| !p.is_empty()) {
            let mut trozos = parte.split('-');
            let dominio: u32 = trozos.next()?.parse().ok()?;
            let _servidor: u64 = trozos.next()?.parse().ok()?;
            let secuencia: u64 = trozos.next()?.parse().ok()?;
            match dominios.iter_mut().find(|(d, _)| *d == dominio) {
                Some((_, s)) => *s = (*s).max(secuencia),
                None => dominios.push((dominio, secuencia)),
            }
        }
        Some(Posicion::GtidMariadb(dominios))
    }
}

/// Qué escrituras tiene que contener una réplica para poder servir una lectura.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requisito {
    /// Las que tocaron esta partición (y las que no se pudieron clasificar).
    Particion(usize),
    /// Todas las escrituras de este nodo.
    Todo,
}

impl Requisito {
    /// Lectura de una clave concreta.
    pub fn clave(clave: &[u8]) -> Self {
        Requisito::Particion(particion(clave))
    }

    /// Lectura de un rango `[desde, hasta]`: si los dos extremos comparten los 4 primeros bytes, también
    /// todas las claves de en medio (orden lexicográfico), así que basta su partición.
    pub fn rango(desde: &[u8], hasta: &[u8]) -> Self {
        if desde.len() >= 4 && hasta.len() >= 4 && desde[..4] == hasta[..4] {
            Requisito::Particion(particion(desde))
        } else {
            Requisito::Todo
        }
    }

    fn de_rangos<T: Key>(rangos: &[IterateParams<T>]) -> Self {
        let mut requisito = None;
        for rango in rangos {
            let r = Requisito::rango(&rango.begin.serialize(0), &rango.end.serialize(0));
            match (requisito, r) {
                (_, Requisito::Todo) => return Requisito::Todo,
                (None, r) => requisito = Some(r),
                (Some(a), b) if a != b => return Requisito::Todo,
                _ => {}
            }
        }
        requisito.unwrap_or(Requisito::Todo)
    }
}

/// Partición de una clave: sus 4 primeros bytes (en casi todos los subespacios, la cuenta).
pub fn particion(clave: &[u8]) -> usize {
    (xxh3_64(&clave[..clave.len().min(4)]) % PARTICIONES as u64) as usize
}

/// Particiones que toca una escritura.
#[derive(Debug, Default)]
pub struct Tocadas {
    pub particiones: Vec<usize>,
    pub todo: bool,
}

impl Tocadas {
    fn clave(&mut self, clave: &[u8]) {
        self.particiones.push(particion(clave));
    }

    fn rango(&mut self, desde: &[u8], hasta: &[u8]) {
        match Requisito::rango(desde, hasta) {
            Requisito::Particion(p) => self.particiones.push(p),
            Requisito::Todo => self.todo = true,
        }
    }

    /// Clave cuyo documento aún no se conoce (identificador pendiente): si los 4 primeros bytes no dependen
    /// del documento, se apunta su partición; si dependen, se marca todo.
    fn clave_variable(&mut self, serializar: impl Fn(u32, u64) -> Vec<u8>) {
        let a = serializar(0, 0);
        let b = serializar(u32::MAX, u64::MAX);
        if a.len() >= 4 && b.len() >= 4 && a[..4] == b[..4] {
            self.clave(&a);
        } else {
            self.todo = true;
        }
    }

    /// Recorre un lote como lo haría el motor SQL y apunta la partición de cada clave escrita.
    pub fn de_lote(batch: &Batch<'_>) -> Self {
        let mut tocadas = Tocadas::default();
        let mut buf = Vec::with_capacity(32);
        for asignacion in batch.allocations() {
            buf.clear();
            asignacion.serialize_key_into(&mut buf, 0);
            tocadas.clave(&buf);
        }

        let mut cuenta = u32::MAX;
        let mut coleccion = u8::MAX;
        let mut documento: Option<u32> = Some(u32::MAX);
        for op in batch.ops.iter() {
            match op {
                Operation::AccountId { account_id } => cuenta = *account_id,
                Operation::Collection { collection } => coleccion = u8::from(*collection),
                Operation::DocumentId { document_id } => {
                    documento = match document_id {
                        PendingId::Assigned(id) => Some(*id),
                        PendingId::Slot(_) => None,
                    }
                }
                Operation::Value { class, .. } => match documento {
                    Some(doc) => tocadas.clave(&class.serialize(cuenta, coleccion, doc, 0)),
                    None => {
                        tocadas.clave_variable(|doc, _| class.serialize(cuenta, coleccion, doc, 0))
                    }
                },
                Operation::Index { field, key, .. } => {
                    let indice = |doc| {
                        IndexKey {
                            account_id: cuenta,
                            collection: coleccion,
                            document_id: doc,
                            field: *field,
                            key: key.as_slice(),
                        }
                        .serialize(0)
                    };
                    match documento {
                        Some(doc) => tocadas.clave(&indice(doc)),
                        None => tocadas.clave_variable(|doc, _| indice(doc)),
                    }
                }
                Operation::Log { collection, .. } => tocadas
                    .clave_variable(|_, cambio| collection.log_key(cuenta, cambio).serialize(0)),
                // Los rangos borrados por lote vienen ya serializados por quien los pide: no se sabe con qué
                // forma, así que se marca todo (es una operación rara).
                Operation::ClearRange { .. } => tocadas.todo = true,
                Operation::AssertValue { .. } => {}
            }
        }
        tocadas.particiones.sort_unstable();
        tocadas.particiones.dedup();
        tocadas
    }
}

/// Registro, por partición, de la última escritura de este nodo que la tocó.
pub struct Frescura {
    /// Escrituras terminadas (cuenta creciente; la «época»).
    epoca: AtomicU64,
    /// Época de la última escritura que no se pudo clasificar: obliga a todas las lecturas.
    barrera: AtomicU64,
    particiones: Box<[AtomicU64]>,
}

impl Default for Frescura {
    fn default() -> Self {
        Frescura {
            epoca: AtomicU64::new(0),
            barrera: AtomicU64::new(0),
            particiones: (0..PARTICIONES).map(|_| AtomicU64::new(0)).collect(),
        }
    }
}

impl Frescura {
    /// Se llama DESPUÉS de cada intento de escritura (con éxito o no) y antes de devolver el control.
    pub fn escrito(&self, tocadas: &Tocadas) {
        let epoca = self.epoca.fetch_add(1, Ordering::AcqRel) + 1;
        for p in &tocadas.particiones {
            self.particiones[*p].fetch_max(epoca, Ordering::AcqRel);
        }
        if tocadas.todo {
            self.barrera.fetch_max(epoca, Ordering::AcqRel);
        }
    }

    pub fn epoca(&self) -> u64 {
        self.epoca.load(Ordering::Acquire)
    }

    /// Época mínima que debe contener una réplica para servir esta lectura.
    pub fn requisito(&self, requisito: Requisito) -> u64 {
        match requisito {
            Requisito::Todo => self.epoca(),
            Requisito::Particion(p) => self
                .barrera
                .load(Ordering::Acquire)
                .max(self.particiones[p].load(Ordering::Acquire)),
        }
    }
}

/// Estado de una réplica.
pub struct Replica {
    pub(crate) miembro: MiembroSql,
    pub(crate) nombre: String,
    sana: AtomicBool,
    /// Época hasta la que se sabe que contiene todas las escrituras de este nodo.
    fresca_hasta: AtomicU64,
    /// Milisegundos (desde el arranque) del momento en que se la vio al día con el primario.
    al_dia_en: AtomicU64,
    pub(crate) lecturas: AtomicU64,
}

/// Contadores (para pruebas y diagnóstico).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Estadisticas {
    pub lecturas_primario: u64,
    pub lecturas_replicas: u64,
    pub reintentos_en_primario: u64,
}

/// Un almacén SQL con réplicas de lectura (`readReplicas`).
pub struct ReplicasSql {
    pub(crate) primario: MiembroSql,
    pub(crate) replicas: Box<[Replica]>,
    turno: AtomicUsize,
    pub(crate) frescura: Frescura,
    inicio: Instant,
    lecturas_primario: AtomicU64,
    reintentos: AtomicU64,
    /// Ya se avisó de que el primario no da posición (se avisa una sola vez).
    avisado_sin_posicion: AtomicBool,
}

/// Una observación del primario: (época de este nodo, posición, milisegundos desde el arranque).
type Observacion = (u64, Posicion, u64);

impl ReplicasSql {
    /// Crea el almacén y arranca su comprobación periódica (termina sola cuando se suelta el almacén).
    pub fn nuevo(primario: MiembroSql, replicas: Vec<(String, MiembroSql)>) -> Arc<Self> {
        let almacen = Arc::new(ReplicasSql {
            primario,
            replicas: replicas
                .into_iter()
                .map(|(nombre, miembro)| Replica {
                    miembro,
                    nombre,
                    sana: AtomicBool::new(true),
                    fresca_hasta: AtomicU64::new(0),
                    al_dia_en: AtomicU64::new(NUNCA),
                    lecturas: AtomicU64::new(0),
                })
                .collect(),
            turno: AtomicUsize::new(0),
            frescura: Frescura::default(),
            inicio: Instant::now(),
            lecturas_primario: AtomicU64::new(0),
            reintentos: AtomicU64::new(0),
            avisado_sin_posicion: AtomicBool::new(false),
        });
        if !almacen.replicas.is_empty() {
            let debil = Arc::downgrade(&almacen);
            tokio::spawn(vigilar(debil));
        }
        almacen
    }

    pub fn primario_store(&self) -> Store {
        self.primario.store()
    }

    pub fn estadisticas(&self) -> Estadisticas {
        Estadisticas {
            lecturas_primario: self.lecturas_primario.load(Ordering::Relaxed),
            lecturas_replicas: self
                .replicas
                .iter()
                .map(|r| r.lecturas.load(Ordering::Relaxed))
                .sum(),
            reintentos_en_primario: self.reintentos.load(Ordering::Relaxed),
        }
    }

    fn ahora_ms(&self) -> u64 {
        self.inicio.elapsed().as_millis() as u64
    }

    /// Elige réplica por turnos entre las sanas, al día y frescas para esta lectura.
    pub(crate) fn elegir(&self, requisito: Requisito) -> Option<&Replica> {
        let n = self.replicas.len();
        if n == 0 {
            return None;
        }
        let necesita = self.frescura.requisito(requisito);
        let ahora = self.ahora_ms();
        let max_retraso = MAX_RETRASO.as_millis() as u64;
        let inicio = self.turno.fetch_add(1, Ordering::Relaxed);
        (0..n).map(|k| &self.replicas[(inicio + k) % n]).find(|r| {
            let al_dia = r.al_dia_en.load(Ordering::Acquire);
            r.sana.load(Ordering::Acquire)
                && al_dia != NUNCA
                && ahora.saturating_sub(al_dia) <= max_retraso
                && r.fresca_hasta.load(Ordering::Acquire) >= necesita
        })
    }

    /// Una lectura en réplica falló: si es un fallo de conexión o del servidor, la réplica se marca caída.
    fn fallo(&self, replica: &Replica, err: &trc::Error) {
        self.reintentos.fetch_add(1, Ordering::Relaxed);
        let de_infraestructura = [
            trc::StoreEvent::PostgresqlError,
            trc::StoreEvent::MysqlError,
            trc::StoreEvent::PoolError,
        ]
        .into_iter()
        .any(|e| err.matches(trc::EventType::Store(e)));
        if de_infraestructura && replica.sana.swap(false, Ordering::AcqRel) {
            trc::event!(
                Store(trc::StoreEvent::PoolError),
                Details = "bezenti: réplica de lectura caída; sus lecturas van al primario",
                Hostname = replica.nombre.clone(),
                CausedBy = err.clone(),
            );
        }
    }

    fn en_primario(&self) {
        self.lecturas_primario.fetch_add(1, Ordering::Relaxed);
    }

    // ---- lecturas ----------------------------------------------------------------------------------------

    pub async fn get_value<U>(&self, key: impl Key) -> trc::Result<Option<U>>
    where
        U: Deserialize + 'static,
    {
        if let Some(replica) = self.elegir(Requisito::clave(&key.serialize(0))) {
            match replica.miembro.get_value(key.clone()).await {
                Ok(v) => {
                    replica.lecturas.fetch_add(1, Ordering::Relaxed);
                    return Ok(v);
                }
                Err(err) => self.fallo(replica, &err),
            }
        }
        self.en_primario();
        self.primario.get_value(key).await
    }

    pub async fn get_value_fresh<U>(&self, key: impl Key) -> trc::Result<Option<U>>
    where
        U: Deserialize + 'static,
    {
        self.en_primario();
        self.primario.get_value(key).await
    }

    pub async fn key_exists(&self, key: impl Key) -> trc::Result<bool> {
        if let Some(replica) = self.elegir(Requisito::clave(&key.serialize(0))) {
            match replica.miembro.key_exists(key.clone()).await {
                Ok(v) => {
                    replica.lecturas.fetch_add(1, Ordering::Relaxed);
                    return Ok(v);
                }
                Err(err) => self.fallo(replica, &err),
            }
        }
        self.en_primario();
        self.primario.key_exists(key).await
    }

    pub async fn get_counter(&self, key: ValueKey<ValueClass>) -> trc::Result<i64> {
        if let Some(replica) = self.elegir(Requisito::clave(&key.serialize(0))) {
            match replica.miembro.get_counter(key.clone()).await {
                Ok(v) => {
                    replica.lecturas.fetch_add(1, Ordering::Relaxed);
                    return Ok(v);
                }
                Err(err) => self.fallo(replica, &err),
            }
        }
        self.en_primario();
        self.primario.get_counter(key).await
    }

    /// Recorrido de un rango. Si la réplica falla ANTES de entregar la primera fila se repite en el primario;
    /// si ya había entregado filas, el error se devuelve (repetir duplicaría filas en quien llama).
    pub async fn iterate<T: Key>(
        &self,
        params: IterateParams<T>,
        mut cb: impl for<'x> FnMut(&'x [u8], &'x [u8]) -> trc::Result<bool> + Sync + Send,
    ) -> trc::Result<()> {
        let requisito = Requisito::rango(&params.begin.serialize(0), &params.end.serialize(0));
        if let Some(replica) = self.elegir(requisito) {
            let mut entregadas = false;
            let mut error_propio = false;
            let resultado = replica
                .miembro
                .iterate(params.clone(), |k: &[u8], v: &[u8]| {
                    entregadas = true;
                    let r = cb(k, v);
                    error_propio = r.is_err();
                    r
                })
                .await;
            match resultado {
                Ok(()) => {
                    replica.lecturas.fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
                Err(err) if error_propio => return Err(err),
                Err(err) => {
                    self.fallo(replica, &err);
                    if entregadas {
                        return Err(err);
                    }
                }
            }
        }
        self.en_primario();
        self.primario.iterate(params, cb).await
    }

    pub async fn iterate_many<T: Key>(
        &self,
        ranges: Vec<IterateParams<T>>,
        mut cb: impl for<'x> FnMut(&'x [u8], &'x [u8]) -> trc::Result<bool> + Sync + Send,
    ) -> trc::Result<()> {
        if let Some(replica) = self.elegir(Requisito::de_rangos(&ranges)) {
            let mut entregadas = false;
            let mut error_propio = false;
            let resultado = replica
                .miembro
                .iterate_many(ranges.clone(), |k: &[u8], v: &[u8]| {
                    entregadas = true;
                    let r = cb(k, v);
                    error_propio = r.is_err();
                    r
                })
                .await;
            match resultado {
                Ok(()) => {
                    replica.lecturas.fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
                Err(err) if error_propio => return Err(err),
                Err(err) => {
                    self.fallo(replica, &err);
                    if entregadas {
                        return Err(err);
                    }
                }
            }
        }
        self.en_primario();
        self.primario.iterate_many(ranges, cb).await
    }

    pub async fn get_blob(&self, key: &[u8]) -> trc::Result<Option<Vec<u8>>> {
        if let Some(replica) = self.elegir(Requisito::clave(key)) {
            match replica.miembro.get_blob(key).await {
                Ok(v) => {
                    replica.lecturas.fetch_add(1, Ordering::Relaxed);
                    return Ok(v);
                }
                Err(err) => self.fallo(replica, &err),
            }
        }
        self.en_primario();
        self.primario.get_blob(key).await
    }

    /// Consultas SQL libres (directorios y búsquedas SQL): siempre al primario, pueden ser escrituras.
    pub async fn sql_query<T: QueryResult>(
        &self,
        query: &str,
        params: &[Value<'_>],
    ) -> trc::Result<T> {
        self.primario.sql_query(query, params).await
    }

    // ---- escrituras --------------------------------------------------------------------------------------

    pub async fn write(&self, batch: Batch<'_>, ids: &mut AssignedIds) -> trc::Result<()> {
        let tocadas = Tocadas::de_lote(&batch);
        let resultado = self.primario.write(batch, ids).await;
        // También si falló: una aserción fallida suele significar que otro escribió esas claves, y la
        // relectura que viene después debe ver su escritura.
        self.frescura.escrito(&tocadas);
        resultado
    }

    pub async fn delete_range(&self, from: impl Key, to: impl Key) -> trc::Result<()> {
        let mut tocadas = Tocadas::default();
        tocadas.rango(&from.serialize(0), &to.serialize(0));
        let resultado = self.primario.delete_range(from, to).await;
        self.frescura.escrito(&tocadas);
        resultado
    }

    pub async fn purge_store(&self) -> trc::Result<()> {
        let resultado = self.primario.purge_store().await;
        self.frescura.escrito(&Tocadas {
            particiones: vec![],
            todo: true,
        });
        resultado
    }

    pub async fn create_tables(&self) -> trc::Result<()> {
        self.primario.create_tables().await
    }

    pub async fn put_blob(&self, key: &[u8], data: &[u8]) -> trc::Result<()> {
        let resultado = self.primario.put_blob(key, data).await;
        self.frescura.escrito(&Tocadas {
            particiones: vec![particion(key)],
            todo: false,
        });
        resultado
    }

    pub async fn delete_blob(&self, key: &[u8]) -> trc::Result<bool> {
        let resultado = self.primario.delete_blob(key).await;
        self.frescura.escrito(&Tocadas {
            particiones: vec![particion(key)],
            todo: false,
        });
        resultado
    }

    // ---- salud y frescura --------------------------------------------------------------------------------

    /// Una vuelta de la comprobación: posición del primario y de cada réplica.
    async fn comprobar(&self, historial: &mut VecDeque<Observacion>) {
        // 1) La época se lee ANTES de preguntar la posición al primario: toda escritura contada en ella ya
        //    había terminado, así que está por debajo de esa posición.
        let epoca = self.frescura.epoca();
        let ahora = self.ahora_ms();
        match tokio::time::timeout(TIEMPO_COMPROBACION, self.primario.posicion(true)).await {
            Ok(Ok(Some(posicion))) => {
                if historial.len() == HISTORIAL {
                    historial.pop_front();
                }
                historial.push_back((epoca, posicion, ahora));
            }
            Ok(Ok(None)) => {
                if self.avisado_sin_posicion.swap(true, Ordering::Relaxed) {
                    return;
                }
                trc::event!(
                    Store(trc::StoreEvent::PoolError),
                    Details = "bezenti: el primario no da posición de replicación (¿MySQL sin GTID, MariaDB sin binlog?); no se usan las réplicas",
                );
                return;
            }
            // Primario inaccesible: no se puede verificar nada; las réplicas caducan solas por MAX_RETRASO.
            _ => return,
        }

        // 2) Cada réplica: la observación más reciente del primario que ya contiene.
        for replica in self.replicas.iter() {
            let resultado =
                tokio::time::timeout(TIEMPO_COMPROBACION, replica.miembro.posicion(false)).await;
            match resultado {
                Ok(Ok(Some(posicion))) => {
                    let estaba_caida = !replica.sana.load(Ordering::Acquire);
                    if let Some((epoca, _, instante)) = historial
                        .iter()
                        .rev()
                        .find(|(_, observada, _)| posicion.contiene(observada))
                    {
                        if estaba_caida {
                            replica.fresca_hasta.store(*epoca, Ordering::Release);
                        } else {
                            replica.fresca_hasta.fetch_max(*epoca, Ordering::AcqRel);
                        }
                        replica.al_dia_en.fetch_max(*instante, Ordering::AcqRel);
                        if replica.al_dia_en.load(Ordering::Acquire) == NUNCA {
                            replica.al_dia_en.store(*instante, Ordering::Release);
                        }
                    } else if estaba_caida {
                        replica.fresca_hasta.store(0, Ordering::Release);
                    }
                    if estaba_caida {
                        replica.sana.store(true, Ordering::Release);
                        trc::event!(
                            Store(trc::StoreEvent::PoolError),
                            Details = "bezenti: réplica de lectura recuperada",
                            Hostname = replica.nombre.clone(),
                        );
                    }
                }
                otro => {
                    if replica.sana.swap(false, Ordering::AcqRel) {
                        let motivo = match otro {
                            Ok(Err(err)) => err.to_string(),
                            Ok(Ok(None)) => {
                                "no es una réplica (no da posición de réplica)".to_string()
                            }
                            _ => "no responde a tiempo".to_string(),
                        };
                        trc::event!(
                            Store(trc::StoreEvent::PoolError),
                            Details =
                                "bezenti: réplica de lectura caída; sus lecturas van al primario",
                            Hostname = replica.nombre.clone(),
                            Reason = motivo,
                        );
                    }
                }
            }
        }
    }
}

async fn vigilar(almacen: Weak<ReplicasSql>) {
    let mut historial = VecDeque::with_capacity(HISTORIAL);
    loop {
        tokio::time::sleep(INTERVALO).await;
        let Some(almacen) = almacen.upgrade() else {
            return;
        };
        almacen.comprobar(&mut historial).await;
    }
}

// ---- llamadas a cada motor ---------------------------------------------------------------------------------

#[allow(unreachable_patterns)]
impl MiembroSql {
    pub fn store(&self) -> Store {
        match self {
            #[cfg(feature = "postgres")]
            MiembroSql::Postgres(s) => Store::PostgreSQL(s.clone()),
            #[cfg(feature = "mysql")]
            MiembroSql::Mysql(s) => Store::MySQL(s.clone()),
        }
    }

    async fn get_value<U: Deserialize + 'static>(&self, key: impl Key) -> trc::Result<Option<U>> {
        match self {
            #[cfg(feature = "postgres")]
            MiembroSql::Postgres(s) => s.get_value(key).await,
            #[cfg(feature = "mysql")]
            MiembroSql::Mysql(s) => s.get_value(key).await,
        }
    }

    async fn key_exists(&self, key: impl Key) -> trc::Result<bool> {
        match self {
            #[cfg(feature = "postgres")]
            MiembroSql::Postgres(s) => s.key_exists(key).await,
            #[cfg(feature = "mysql")]
            MiembroSql::Mysql(s) => s.key_exists(key).await,
        }
    }

    async fn get_counter(&self, key: ValueKey<ValueClass>) -> trc::Result<i64> {
        match self {
            #[cfg(feature = "postgres")]
            MiembroSql::Postgres(s) => s.get_counter(key).await,
            #[cfg(feature = "mysql")]
            MiembroSql::Mysql(s) => s.get_counter(key).await,
        }
    }

    async fn iterate<T: Key>(
        &self,
        params: IterateParams<T>,
        cb: impl for<'x> FnMut(&'x [u8], &'x [u8]) -> trc::Result<bool> + Sync + Send,
    ) -> trc::Result<()> {
        match self {
            #[cfg(feature = "postgres")]
            MiembroSql::Postgres(s) => s.iterate(params, cb).await,
            #[cfg(feature = "mysql")]
            MiembroSql::Mysql(s) => s.iterate(params, cb).await,
        }
    }

    async fn iterate_many<T: Key>(
        &self,
        ranges: Vec<IterateParams<T>>,
        cb: impl for<'x> FnMut(&'x [u8], &'x [u8]) -> trc::Result<bool> + Sync + Send,
    ) -> trc::Result<()> {
        match self {
            #[cfg(feature = "postgres")]
            MiembroSql::Postgres(s) => s.iterate_many(ranges, cb).await,
            #[cfg(feature = "mysql")]
            MiembroSql::Mysql(s) => s.iterate_many(ranges, cb).await,
        }
    }

    async fn get_blob(&self, key: &[u8]) -> trc::Result<Option<Vec<u8>>> {
        match self {
            #[cfg(feature = "postgres")]
            MiembroSql::Postgres(s) => s.get_blob(key, 0..usize::MAX).await,
            #[cfg(feature = "mysql")]
            MiembroSql::Mysql(s) => s.get_blob(key, 0..usize::MAX).await,
        }
    }

    async fn put_blob(&self, key: &[u8], data: &[u8]) -> trc::Result<()> {
        match self {
            #[cfg(feature = "postgres")]
            MiembroSql::Postgres(s) => s.put_blob(key, data).await,
            #[cfg(feature = "mysql")]
            MiembroSql::Mysql(s) => s.put_blob(key, data).await,
        }
    }

    async fn delete_blob(&self, key: &[u8]) -> trc::Result<bool> {
        match self {
            #[cfg(feature = "postgres")]
            MiembroSql::Postgres(s) => s.delete_blob(key).await,
            #[cfg(feature = "mysql")]
            MiembroSql::Mysql(s) => s.delete_blob(key).await,
        }
    }

    async fn sql_query<T: QueryResult>(&self, query: &str, params: &[Value<'_>]) -> trc::Result<T> {
        match self {
            #[cfg(feature = "postgres")]
            MiembroSql::Postgres(s) => s.sql_query(query, params).await,
            #[cfg(feature = "mysql")]
            MiembroSql::Mysql(s) => s.sql_query(query, params).await,
        }
    }

    async fn write(&self, batch: Batch<'_>, ids: &mut AssignedIds) -> trc::Result<()> {
        match self {
            #[cfg(feature = "postgres")]
            MiembroSql::Postgres(s) => s.write(batch, ids).await,
            #[cfg(feature = "mysql")]
            MiembroSql::Mysql(s) => s.write(batch, ids).await,
        }
    }

    async fn delete_range(&self, from: impl Key, to: impl Key) -> trc::Result<()> {
        match self {
            #[cfg(feature = "postgres")]
            MiembroSql::Postgres(s) => s.delete_range(from, to).await,
            #[cfg(feature = "mysql")]
            MiembroSql::Mysql(s) => s.delete_range(from, to).await,
        }
    }

    async fn purge_store(&self) -> trc::Result<()> {
        match self {
            #[cfg(feature = "postgres")]
            MiembroSql::Postgres(s) => s.purge_store().await,
            #[cfg(feature = "mysql")]
            MiembroSql::Mysql(s) => s.purge_store().await,
        }
    }

    async fn create_tables(&self) -> trc::Result<()> {
        match self {
            #[cfg(feature = "postgres")]
            MiembroSql::Postgres(s) => s.create_storage_tables().await,
            #[cfg(feature = "mysql")]
            MiembroSql::Mysql(s) => s.create_storage_tables().await,
        }
    }

    /// Posición de replicación. `primario = true`: lo escrito; `false`: lo aplicado por la réplica.
    /// `Ok(None)`: el servidor no puede dar una posición comparable.
    async fn posicion(&self, primario: bool) -> trc::Result<Option<Posicion>> {
        match self {
            #[cfg(feature = "postgres")]
            MiembroSql::Postgres(s) => s.posicion_replicacion(primario).await,
            #[cfg(feature = "mysql")]
            MiembroSql::Mysql(s) => s.posicion_replicacion(primario).await,
        }
    }
}

#[cfg(test)]
mod pruebas {
    use super::*;

    #[test]
    fn rangos_con_prefijo_comun_usan_su_particion() {
        let a = [0, 0, 0, 7, 1, 2];
        let b = [0, 0, 0, 7, 255, 255, 255];
        assert_eq!(
            Requisito::rango(&a, &b),
            Requisito::Particion(particion(&a))
        );
        assert_eq!(Requisito::rango(&a, &[0, 0, 0, 8]), Requisito::Todo);
        assert_eq!(Requisito::rango(&[0, 0], &[0, 0]), Requisito::Todo);
        // La partición solo depende de los 4 primeros bytes.
        assert_eq!(particion(&a), particion(&b));
    }

    #[test]
    fn lee_lo_que_escribiste() {
        let f = Frescura::default();
        let cuenta_a = particion(&7u32.to_be_bytes());
        let cuenta_b = particion(&8u32.to_be_bytes());
        assert_eq!(f.requisito(Requisito::Particion(cuenta_a)), 0);

        f.escrito(&Tocadas {
            particiones: vec![cuenta_a],
            todo: false,
        });
        // La cuenta escrita exige la época 1; la otra sigue sin exigir nada; «todo» exige la última.
        assert_eq!(f.requisito(Requisito::Particion(cuenta_a)), 1);
        assert_eq!(f.requisito(Requisito::Particion(cuenta_b)), 0);
        assert_eq!(f.requisito(Requisito::Todo), 1);

        // Una escritura sin clasificar obliga a todas.
        f.escrito(&Tocadas {
            particiones: vec![],
            todo: true,
        });
        assert_eq!(f.requisito(Requisito::Particion(cuenta_b)), 2);
    }

    #[test]
    fn posiciones() {
        assert!(Posicion::Lsn(10).contiene(&Posicion::Lsn(10)));
        assert!(!Posicion::Lsn(9).contiene(&Posicion::Lsn(10)));

        let p = Posicion::gtid_mariadb("0-1-100,1-2-5").unwrap();
        assert!(
            Posicion::gtid_mariadb("0-1-100,1-3-6")
                .unwrap()
                .contiene(&p)
        );
        assert!(!Posicion::gtid_mariadb("0-1-99,1-3-6").unwrap().contiene(&p));
        assert!(!Posicion::gtid_mariadb("0-1-100").unwrap().contiene(&p));
        assert!(
            Posicion::gtid_mariadb("")
                .unwrap()
                .contiene(&Posicion::gtid_mariadb("").unwrap())
        );

        let u = "3E11FA47-71CA-11E1-9E33-C80AA9429562";
        let p = Posicion::gtid_mysql(&format!("{u}:1-5:7")).unwrap();
        assert!(
            Posicion::gtid_mysql(&format!("{u}:1-7"))
                .unwrap()
                .contiene(&p)
        );
        assert!(
            Posicion::gtid_mysql(&format!("{}:1-5:6-9", u.to_lowercase()))
                .unwrap()
                .contiene(&p)
        );
        assert!(
            !Posicion::gtid_mysql(&format!("{u}:1-5"))
                .unwrap()
                .contiene(&p)
        );
        assert!(!Posicion::gtid_mysql("").unwrap().contiene(&p));
        assert!(
            Posicion::gtid_mysql(&format!("{u}:1-5:7,otro:1-2"))
                .unwrap()
                .contiene(&p)
        );
        assert!(!Posicion::Lsn(1).contiene(&p));
    }
}
