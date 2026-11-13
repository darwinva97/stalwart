/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Pruebas de integración de las réplicas de lectura contra servidores REALES: un PostgreSQL primario con una
//! réplica física, y un MariaDB/MySQL primario con una réplica GTID.
//!
//! Las lanza `bezenti/pruebas.sh`, que levanta los servidores sin root con `bezenti/pruebas/postgres_replica.sh`
//! y `bezenti/pruebas/mariadb_replica.sh` y pasa sus datos por variables de entorno. Sin esas variables cada
//! prueba no hace nada (así `cargo test` normal no necesita bases de datos).

use super::replicas::MAX_RETRASO;
use crate::{
    BlobStore, IterateParams, Store, ValueKey,
    write::{BatchBuilder, ValueClass, blob_codec::BlobOpts},
};
use std::{process::Command, time::Duration};
use types::collection::Collection;

/// Cómo manejar la réplica desde la prueba.
trait Control {
    /// Detener la aplicación de cambios (la réplica sigue respondiendo, pero se atrasa).
    fn pausar(&self);
    fn reanudar(&self);
    /// Parar el servidor réplica de golpe.
    fn parar(&self);
    fn arrancar(&self);
}

/// Al soltarse, arranca la réplica y reanuda la replicación (por si la prueba falló a medias).
struct Restaurar<'a>(&'a dyn Control);

impl Drop for Restaurar<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // Cada paso por separado: si la réplica ya estaba en marcha, arrancarla falla y no pasa nada.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.0.arrancar()));
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.0.reanudar()));
        }
    }
}

fn clave(cuenta: u32) -> ValueKey<ValueClass> {
    ValueKey {
        account_id: cuenta,
        collection: Collection::Email.into(),
        document_id: 1,
        class: ValueClass::Property(1),
    }
}

async fn escribir(store: &Store, cuenta: u32, valor: &str) {
    let mut lote = BatchBuilder::new();
    lote.with_account_id(cuenta)
        .with_collection(Collection::Email)
        .with_document(1)
        .set(ValueClass::Property(1), valor.as_bytes().to_vec());
    store.write_batch(&mut lote).await.unwrap();
}

async fn leer(store: &Store, cuenta: u32) -> Option<String> {
    store.get_value::<String>(clave(cuenta)).await.unwrap()
}

fn ejecutar(programa: &str, args: &[&str]) {
    let ok = Command::new(programa)
        .args(args)
        .status()
        .unwrap_or_else(|e| panic!("{programa}: {e}"))
        .success();
    assert!(ok, "{programa} {args:?}");
}

/// El escenario común: `store` tiene una réplica; `otro_nodo` es otro nodo con el mismo primario y sin réplicas.
async fn escenario(store: Store, otro_nodo: Store, control: &dyn Control) {
    let Store::Replicas(replicas) = &store else {
        panic!("con readReplicas el almacén debe ser Store::Replicas, es {store:?}")
    };
    store.create_tables().await.unwrap();
    let _restaurar = Restaurar(control);
    // Cuentas nuevas en cada ejecución (la base puede venir de una ejecución anterior).
    let base = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
        % 1_000_000) as u32
        * 10;
    let (c1, c2, c3) = (base + 1, base + 2, base + 3);

    // Esperar a que la réplica esté al día (las tablas recién creadas tienen que haber llegado).
    escribir(&store, c1, "v0").await;
    tokio::time::sleep(Duration::from_millis(1500)).await;

    println!("== 1. leer lo que se acaba de escribir: del primario, nunca un valor viejo");
    for i in 1..=150 {
        let valor = format!("v{i}");
        escribir(&store, c1, &valor).await;
        assert_eq!(
            leer(&store, c1).await.as_deref(),
            Some(valor.as_str()),
            "vuelta {i}"
        );
    }
    let antes = replicas.estadisticas();
    assert!(antes.lecturas_primario >= 150, "{antes:?}");

    println!("== 2. pasado el intervalo, las lecturas van a la réplica");
    tokio::time::sleep(Duration::from_millis(1000)).await;
    for _ in 0..20 {
        assert_eq!(leer(&store, c1).await.as_deref(), Some("v150"));
    }
    let despues = replicas.estadisticas();
    assert_eq!(
        despues.lecturas_replicas - antes.lecturas_replicas,
        20,
        "{despues:?}"
    );
    assert_eq!(
        despues.lecturas_primario, antes.lecturas_primario,
        "{despues:?}"
    );

    // Recorrido de rango (iterate) y contador servidos por la réplica.
    let mut filas = 0;
    store
        .iterate(
            IterateParams::new(clave(c1), clave(c1)).ascending(),
            |_, v| {
                assert_eq!(v, b"v150");
                filas += 1;
                Ok(true)
            },
        )
        .await
        .unwrap();
    assert_eq!(filas, 1);
    assert_eq!(
        replicas.estadisticas().lecturas_replicas - despues.lecturas_replicas,
        1
    );

    println!("== 3. blobs: escribir y leer al momento, y luego desde la réplica");
    let blobs = BlobStore::Store(store.clone());
    let clave_blob = blake3::hash(&base.to_be_bytes()).as_bytes().to_vec();
    blobs
        .put_blob(&clave_blob, b"contenido", &BlobOpts::PLAIN)
        .await
        .unwrap();
    let leido = blobs
        .get_blob(&clave_blob, 0..usize::MAX, &Default::default())
        .await
        .unwrap();
    assert_eq!(leido.as_deref(), Some(&b"contenido"[..]));
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let r0 = replicas.estadisticas().lecturas_replicas;
    let leido = blobs
        .get_blob(&clave_blob, 0..usize::MAX, &Default::default())
        .await
        .unwrap();
    assert_eq!(leido.as_deref(), Some(&b"contenido"[..]));
    assert_eq!(replicas.estadisticas().lecturas_replicas, r0 + 1);

    println!("== 4. réplica atrasada (replicación en pausa)");
    control.pausar();
    tokio::time::sleep(Duration::from_millis(500)).await;
    // Lo escrito por este nodo se sigue leyendo bien: va al primario.
    escribir(&store, c1, "en-pausa").await;
    assert_eq!(leer(&store, c1).await.as_deref(), Some("en-pausa"));
    // Lo que escribe OTRO nodo no está en el registro de este: dentro del retraso máximo puede leerse viejo
    // de la réplica (límite documentado). Esto prueba además que la lectura la sirvió la réplica de verdad.
    escribir(&otro_nodo, c2, "ajeno").await;
    let r1 = replicas.estadisticas();
    assert_eq!(leer(&store, c2).await, None);
    assert_eq!(
        replicas.estadisticas().lecturas_replicas,
        r1.lecturas_replicas + 1
    );
    // Superado MAX_RETRASO sin ponerse al día, la réplica deja de usarse: se lee lo último del primario.
    tokio::time::sleep(MAX_RETRASO + Duration::from_millis(800)).await;
    let r2 = replicas.estadisticas();
    assert_eq!(leer(&store, c2).await.as_deref(), Some("ajeno"));
    let r3 = replicas.estadisticas();
    assert_eq!(r3.lecturas_replicas, r2.lecturas_replicas);
    assert_eq!(r3.lecturas_primario, r2.lecturas_primario + 1);
    // Al reanudar, la réplica vuelve a usarse.
    control.reanudar();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(leer(&store, c2).await.as_deref(), Some("ajeno"));
    assert_eq!(
        replicas.estadisticas().lecturas_replicas,
        r3.lecturas_replicas + 1
    );

    println!("== 5. la réplica cae: todo sigue funcionando con el primario");
    control.parar();
    for i in 0..30 {
        let valor = format!("caida-{i}");
        escribir(&store, c3, &valor).await;
        assert_eq!(leer(&store, c3).await.as_deref(), Some(valor.as_str()));
        assert_eq!(leer(&store, c2).await.as_deref(), Some("ajeno"));
        assert_eq!(leer(&store, c1).await.as_deref(), Some("en-pausa"));
    }
    let caida = replicas.estadisticas();
    println!("   estadísticas con la réplica caída: {caida:?}");

    println!("== 6. la réplica vuelve y se usa otra vez");
    control.arrancar();
    let mut vuelta = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let antes = replicas.estadisticas().lecturas_replicas;
        assert_eq!(leer(&store, c2).await.as_deref(), Some("ajeno"));
        if replicas.estadisticas().lecturas_replicas > antes {
            vuelta = true;
            break;
        }
    }
    assert!(
        vuelta,
        "la réplica no volvió a usarse: {:?}",
        replicas.estadisticas()
    );
    println!("   estadísticas finales: {:?}", replicas.estadisticas());
}

// ---- PostgreSQL ---------------------------------------------------------------------------------------------

#[cfg(feature = "postgres")]
mod postgres {
    use super::*;
    use crate::backend::postgres::PostgresStore;
    use registry::schema::{prelude::List, structs};

    struct Pg {
        puerto_primario: u16,
        puerto_replica: u16,
        pg_ctl: String,
        psql: String,
        dir_replica: String,
    }

    impl Pg {
        fn del_entorno() -> Option<Pg> {
            Some(Pg {
                puerto_primario: std::env::var("BEZENTI_PG_PUERTO_PRIMARIO")
                    .ok()?
                    .parse()
                    .ok()?,
                puerto_replica: std::env::var("BEZENTI_PG_PUERTO_REPLICA")
                    .ok()?
                    .parse()
                    .ok()?,
                pg_ctl: std::env::var("BEZENTI_PG_CTL").ok()?,
                psql: std::env::var("BEZENTI_PG_PSQL").ok()?,
                dir_replica: std::env::var("BEZENTI_PG_DIR_REPLICA").ok()?,
            })
        }

        fn psql_replica(&self, sql: &str) {
            let puerto = self.puerto_replica.to_string();
            ejecutar(
                &self.psql,
                &[
                    "-h",
                    "127.0.0.1",
                    "-p",
                    &puerto,
                    "-U",
                    "stalwart",
                    "-d",
                    "stalwart",
                    "-qAt",
                    "-c",
                    sql,
                ],
            );
        }

        fn pg_ctl(&self, orden: &[&str]) {
            let log = format!("{}/../replica.log", self.dir_replica);
            let mut args = vec!["-D", self.dir_replica.as_str(), "-w", "-l", log.as_str()];
            args.extend_from_slice(orden);
            ejecutar(&self.pg_ctl, &args);
        }

        fn config(&self, replicas: &[u16]) -> structs::PostgreSqlStore {
            structs::PostgreSqlStore {
                host: "127.0.0.1".into(),
                port: self.puerto_primario as u64,
                database: "stalwart".into(),
                auth_username: Some("stalwart".into()),
                auth_secret: structs::SecretKeyOptional::None,
                connect_timeout: Some(Duration::from_secs(2).into()),
                pool_max_connections: Some(8),
                read_replicas: List::from(
                    replicas
                        .iter()
                        .map(|p| structs::PostgreSqlSettings {
                            host: "127.0.0.1".into(),
                            port: *p as u64,
                            database: "stalwart".into(),
                            options: None,
                            auth_username: Some("stalwart".into()),
                            auth_secret: structs::SecretKeyOptional::None,
                        })
                        .collect::<Vec<_>>(),
                ),
                ..Default::default()
            }
        }
    }

    impl Control for Pg {
        fn pausar(&self) {
            self.psql_replica("SELECT pg_wal_replay_pause()");
        }
        fn reanudar(&self) {
            self.psql_replica("SELECT pg_wal_replay_resume()");
        }
        fn parar(&self) {
            self.pg_ctl(&["-m", "immediate", "stop"]);
        }
        fn arrancar(&self) {
            self.pg_ctl(&["start"]);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn replicas_postgres_reales() {
        let Some(pg) = Pg::del_entorno() else {
            eprintln!("sin BEZENTI_PG_*: prueba de réplicas PostgreSQL reales omitida");
            return;
        };
        let store = PostgresStore::open(pg.config(&[pg.puerto_replica]))
            .await
            .unwrap();
        let otro_nodo = PostgresStore::open(pg.config(&[])).await.unwrap();
        assert!(matches!(otro_nodo, Store::PostgreSQL(_)));
        escenario(store, otro_nodo, &pg).await;
    }
}

// ---- MySQL / MariaDB ----------------------------------------------------------------------------------------

#[cfg(feature = "mysql")]
mod mysql {
    use super::*;
    use crate::backend::mysql::MysqlStore;
    use registry::schema::{prelude::List, structs};

    struct My {
        puerto_primario: u16,
        puerto_replica: u16,
        guion: String,
    }

    impl My {
        fn del_entorno() -> Option<My> {
            Some(My {
                puerto_primario: std::env::var("BEZENTI_MY_PUERTO_PRIMARIO")
                    .ok()?
                    .parse()
                    .ok()?,
                puerto_replica: std::env::var("BEZENTI_MY_PUERTO_REPLICA")
                    .ok()?
                    .parse()
                    .ok()?,
                guion: format!("{}/replica.sh", std::env::var("BEZENTI_MY_DIR").ok()?),
            })
        }

        fn config(&self, replicas: &[u16]) -> structs::MySqlStore {
            structs::MySqlStore {
                host: "127.0.0.1".into(),
                port: self.puerto_primario as u64,
                database: "stalwart".into(),
                auth_username: Some("stalwart".into()),
                auth_secret: structs::SecretKeyOptional::None,
                pool_max_connections: Some(8),
                pool_min_connections: Some(1),
                read_replicas: List::from(
                    replicas
                        .iter()
                        .map(|p| structs::MySqlSettings {
                            host: "127.0.0.1".into(),
                            port: *p as u64,
                            database: "stalwart".into(),
                            auth_username: Some("stalwart".into()),
                            auth_secret: structs::SecretKeyOptional::None,
                        })
                        .collect::<Vec<_>>(),
                ),
                ..Default::default()
            }
        }
    }

    impl Control for My {
        fn pausar(&self) {
            ejecutar("bash", &[&self.guion, "pausar"]);
        }
        fn reanudar(&self) {
            ejecutar("bash", &[&self.guion, "reanudar"]);
        }
        fn parar(&self) {
            ejecutar("bash", &[&self.guion, "parar"]);
        }
        fn arrancar(&self) {
            ejecutar("bash", &[&self.guion, "arrancar"]);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn replicas_mariadb_reales() {
        let Some(my) = My::del_entorno() else {
            eprintln!("sin BEZENTI_MY_*: prueba de réplicas MySQL/MariaDB reales omitida");
            return;
        };
        let store = MysqlStore::open(my.config(&[my.puerto_replica]))
            .await
            .unwrap();
        let otro_nodo = MysqlStore::open(my.config(&[])).await.unwrap();
        assert!(matches!(otro_nodo, Store::MySQL(_)));
        escenario(store, otro_nodo, &my).await;
    }
}
