/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Prueba de integración de las réplicas de lectura contra un PostgreSQL primario y una réplica física REALES.
//!
//! La lanza `bezenti/pruebas/replicas_postgres.sh`, que levanta los dos servidores sin root (`initdb`,
//! `pg_basebackup -R`, `pg_ctl`) y pasa sus datos por variables de entorno. Sin esas variables la prueba no hace
//! nada (así `cargo test` normal no necesita PostgreSQL).

use super::replicas::MAX_RETRASO;
use crate::{
    BlobStore, IterateParams, Store, ValueKey,
    backend::postgres::PostgresStore,
    write::{BatchBuilder, ValueClass, blob_codec::BlobOpts},
};
use registry::schema::{prelude::List, structs};
use std::{process::Command, time::Duration};
use types::collection::Collection;

struct Entorno {
    puerto_primario: u16,
    puerto_replica: u16,
    pg_ctl: String,
    psql: String,
    dir_replica: String,
}

fn entorno() -> Option<Entorno> {
    Some(Entorno {
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

fn config(puerto: u16, replicas: &[u16]) -> structs::PostgreSqlStore {
    structs::PostgreSqlStore {
        host: "127.0.0.1".into(),
        port: puerto as u64,
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

fn psql_replica(e: &Entorno, sql: &str) {
    let ok = Command::new(&e.psql)
        .args([
            "-h",
            "127.0.0.1",
            "-p",
            &e.puerto_replica.to_string(),
            "-U",
            "stalwart",
        ])
        .args(["-d", "stalwart", "-qAt", "-c", sql])
        .status()
        .unwrap()
        .success();
    assert!(ok, "psql en la réplica: {sql}");
}

fn pg_ctl_replica(e: &Entorno, orden: &[&str]) {
    let ok = Command::new(&e.pg_ctl)
        .args(["-D", &e.dir_replica, "-w"])
        .args(orden)
        .arg("-l")
        .arg(format!("{}/../replica.log", e.dir_replica))
        .status()
        .unwrap()
        .success();
    assert!(ok, "pg_ctl {orden:?}");
}

/// Al soltarse, reanuda la replicación y arranca la réplica (por si la prueba falló con ella parada).
struct Restaurar<'a>(&'a Entorno);

impl Drop for Restaurar<'_> {
    fn drop(&mut self) {
        let e = self.0;
        let _ = Command::new(&e.pg_ctl)
            .args(["-D", &e.dir_replica, "-w", "-l"])
            .arg(format!("{}/../replica.log", e.dir_replica))
            .arg("start")
            .output();
        let _ = Command::new(&e.psql)
            .args([
                "-h",
                "127.0.0.1",
                "-p",
                &e.puerto_replica.to_string(),
                "-U",
                "stalwart",
            ])
            .args([
                "-d",
                "stalwart",
                "-qAt",
                "-c",
                "SELECT pg_wal_replay_resume()",
            ])
            .output();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replicas_postgres_reales() {
    let Some(e) = entorno() else {
        eprintln!("sin BEZENTI_PG_*: prueba de réplicas reales omitida");
        return;
    };
    let store = PostgresStore::open(config(e.puerto_primario, &[e.puerto_replica]))
        .await
        .unwrap();
    let Store::Replicas(replicas) = &store else {
        panic!("con readReplicas el almacén debe ser Store::Replicas, es {store:?}")
    };
    // Otro «nodo»: mismo primario, sin réplicas. Sus escrituras no las ve el registro de este nodo.
    let otro_nodo = PostgresStore::open(config(e.puerto_primario, &[]))
        .await
        .unwrap();
    assert!(matches!(otro_nodo, Store::PostgreSQL(_)));
    store.create_tables().await.unwrap();
    // Si la prueba falla a medias, la réplica queda otra vez en marcha y replicando.
    let _restaurar = Restaurar(&e);
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
    psql_replica(&e, "SELECT pg_wal_replay_pause()");
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
    psql_replica(&e, "SELECT pg_wal_replay_resume()");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(leer(&store, c2).await.as_deref(), Some("ajeno"));
    assert_eq!(
        replicas.estadisticas().lecturas_replicas,
        r3.lecturas_replicas + 1
    );

    println!("== 5. la réplica cae: todo sigue funcionando con el primario");
    pg_ctl_replica(&e, &["-m", "immediate", "stop"]);
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
    pg_ctl_replica(&e, &["start"]);
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
