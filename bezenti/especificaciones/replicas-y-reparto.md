# Réplicas de lectura y almacenes repartidos (fase 6)

Especificación de la edición bezenti. Escrita **solo** con fuentes permitidas: la documentación pública de
Stalwart, el esquema `resources/schema/schema.json.gz`, el código AGPL de la rama `bezenti` (que solo dice
*dónde* engancharse: los archivos `store/src/backend/composite/*` del inventario están vacíos) y la
documentación de PostgreSQL y MySQL/MariaDB. Cada punto dice de dónde sale:

- **[doc]**: lo dice la documentación pública. Páginas: <https://stalw.art/docs/storage/backends/composite/>
  (almacenes compuestos: réplicas SQL, *sharding* de memoria y de blobs), `…/storage/backends/postgresql/`,
  `…/storage/backends/mysql/`, `…/storage/backends/redis/`, `…/cluster/deployment/storage/`,
  `…/server/enterprise/`.
- **[esquema]**: lo define el esquema de la API.
- **[bezenti]**: decisión nuestra, porque la documentación no lo dice o porque la cambiamos a propósito.

Código: `crates/store/src/backend/bezenti/` (`replicas.rs`, `reparto.rs`, `mod.rs`),
`crates/store/src/backend/{postgres,mysql}/bezenti.rs` y enganches marcados `bezenti (fase 6)` en `store`,
`common` (instantáneas de caché), `coordinator` y `directory` (LDAP). Todo tras la feature `bezenti`.

## 1. Interfaz (lo que define el esquema)

- [esquema] `x:PostgreSqlStore.readReplicas`: lista de `x:PostgreSqlSettings` (`host`, `port` 5432,
  `database` «stalwart», `authUsername` «stalwart», `authSecret`, `options`). `x:MySqlStore.readReplicas`:
  lista de `x:MySqlSettings` (los mismos sin `options`, puerto 3306). Valen donde se use un almacén
  PostgreSQL/MySQL: almacén de datos (`config.json`), de blobs, de búsqueda y de búsqueda de claves (lookup).
- [esquema] `x:BlobStore` gana la variante `Sharded` (`x:ShardedBlobStore`: `stores`, lista de al menos 2
  `x:BlobStoreBase` = `S3`, `Azure`, `FileSystem`, `FoundationDb`, `PostgreSql`, `MySql`).
- [esquema] `x:InMemoryStore` y `x:LookupStore` ganan `Sharded` (`x:ShardedInMemoryStore`: `stores`, al menos
  2 `x:InMemoryStoreBase` = `Redis`, `RedisCluster`, `RedisSentinel`).
- [esquema] `x:LdapDirectory.urlFailover`: conjunto de URI, «servidores LDAP de respaldo, probados en orden
  cuando el principal no responde».
- [doc] La documentación actual menciona además `readFromReplicas` en `x:ShardedInMemoryStore`; **no está en
  el esquema v1.1.0** que implementamos, así que no existe aquí (cada miembro Redis ya decide eso).
- [doc] Cambiar el almacén de datos exige reiniciar (vive en `config.json` y se lee al arrancar).

## 2. Réplicas de lectura SQL

### 2.1 Qué va a cada servidor

- [doc] Todas las escrituras van al primario. También las lecturas hechas durante una escritura y todas las
  consultas de búsqueda.
- [bezenti] Concretamente:
  - **Al primario siempre**: `write` (lotes, con sus aserciones, fusiones y reservas de identificadores, que el
    motor lee dentro de su propia transacción), `delete_range`, `purge_store`, `create_tables`, subir y borrar
    blobs, `get_value_fresh` (lecturas que el código marca como «tiene que ser lo último», p. ej. la cola SMTP),
    `sql_query` (consultas SQL libres de directorios y lookups: pueden ser escrituras y suelen ser de
    autenticación) y la **búsqueda de texto** (un almacén de búsqueda construido sobre un PostgreSQL/MySQL con
    réplicas usa solo su primario).
  - **A una réplica si se puede**: `get_value`, `key_exists`, `get_counter`, `iterate`, `iterate_many` y leer
    blobs.

### 2.2 Regla de consistencia: «lee lo que escribiste» por partición [bezenti]

[doc] Stalwart no sigue el retraso de replicación y recomienda replicación síncrona o de poco retraso. Nosotros
sí lo seguimos, para que una lectura justo después de una escritura en el mismo nodo **nunca** vea un valor
viejo:

1. **Partición** de una clave = sus 4 primeros bytes (serializada sin subespacio), resumidos con XXH3 en 4096
   casillas. En casi todos los subespacios esos bytes son la cuenta: la partición es, en la práctica, «la cuenta».
2. Cada escritura terminada (con éxito **o no**: una aserción fallida suele significar que otro escribió esas
   claves y la relectura debe ver lo nuevo) suma 1 a la **época** del nodo y apunta esa época en las
   particiones de todas las claves que tocó (valores, índices, registros de cambios, contadores y reservas,
   blobs). Lo que no se puede clasificar marca una **barrera** que vale para todas: `ClearRange` dentro de un
   lote, `purge_store`, rangos borrados que cruzan particiones, claves cuyo prefijo depende de un identificador
   aún no asignado.
3. Cada 200 ms se pregunta la posición al primario (antes se lee la época, así toda escritura contada ya está
   por debajo de esa posición) y a cada réplica: PostgreSQL `pg_current_wal_lsn()` / `pg_last_wal_replay_lsn()`;
   MySQL `@@gtid_executed` (con `gtid_mode = ON`); MariaDB `@@gtid_binlog_pos` / `@@gtid_slave_pos`. Se
   recuerdan las últimas 64 observaciones del primario (~13 s); para cada réplica se busca la más reciente que
   ya contiene, y la réplica queda **fresca hasta** esa época y **al día en** ese instante.
4. Una lectura de una clave exige que la réplica esté fresca hasta `max(barrera, época de su partición)`. Un
   rango cuyos extremos comparten los 4 primeros bytes usa esa partición (todas las claves de en medio la
   comparten); si no, exige todas las escrituras del nodo.
5. Resultado: tras escribir en una cuenta, sus lecturas van al primario unos 200–400 ms (hasta que la réplica
   aplica esa escritura y la comprobación lo ve); las demás cuentas siguen leyendo de las réplicas.

Garantía y límites (documentados a propósito):

- Por nodo: lo que escribió **este** nodo se lee siempre actualizado. Lo que escribe **otro** nodo del clúster
  no lo conoce este registro: puede leerse de una réplica con hasta **5 s** de retraso (ver 2.3). Quien necesite
  lo último entre nodos debe usar replicación síncrona (`synchronous_commit = remote_apply` en PostgreSQL) o un
  solo nodo con réplicas.
- Réplicas sin posición verificable (MySQL sin GTID, MariaDB sin binlog, un «réplica» que no está en
  recuperación, replicación lógica) **no se usan**: se avisa en el registro y todo va al primario.

### 2.3 Reparto de carga, salud y conmutación

- [doc] Las lecturas rotan entre las réplicas; una lectura que falla en una réplica se repite en el primario.
- [bezenti] Por turnos (contador atómico) entre las réplicas que estén **sanas**, **al día** (vistas al día hace
  como mucho 5 s) y **frescas** para esa lectura; si ninguna cumple, al primario.
- [bezenti] Si una lectura falla en una réplica por un error del servidor o de conexión (PostgreSQL, MySQL o la
  piscina de conexiones), la réplica se marca **caída** (evento `store.pool-error` con el servidor) y la lectura
  se repite en el primario. Un recorrido (`iterate`) solo se repite si la réplica no había entregado aún ninguna
  fila (repetirlo duplicaría filas en quien llama); si ya había entregado, se devuelve el error. Un error del
  propio llamador no se repite.
- [bezenti] La comprobación periódica (timeout 2 s por servidor) también marca caída a la réplica que no
  responde o deja de ser réplica, y la vuelve a dar por buena cuando responde (evento «réplica recuperada»).
  Al volver, su frescura se recalcula desde cero.
- [bezenti] Una réplica que se atrasa más de 5 s (replicación rota o en pausa, aunque conteste) deja de usarse
  hasta ponerse al día. Si el primario no contesta, no se puede verificar nada: las réplicas caducan solas a
  los 5 s y entonces todo falla igual que sin réplicas (no hay modo «solo lectura»).
- [doc] Cada réplica tiene su propia piscina de conexiones, del mismo tamaño que la del primario: hay que
  contarlas para `max_connections`. [bezenti] La piscina de la réplica usa los mismos ajustes que la del
  primario (TLS, tiempos de espera, reciclado) salvo servidor, puerto, base, usuario, contraseña y opciones.

## 3. LDAP con servidores de respaldo (`urlFailover`)

- [esquema] Los URI de `urlFailover` se añaden, en orden, detrás de `url`. [bezenti] La conmutación ya existe en
  el código libre (`directory/src/backend/ldap/failover.rs`): si el servidor activo no responde se prueba el
  siguiente; tras un fallo se vuelve a intentar el principal pasado un minuto. Los URI vacíos se ignoran.

## 4. Almacenes repartidos

### 4.1 A qué miembro va cada clave

- [doc] Cada clave se asigna a un miembro según la clave y el número de miembros; todos los nodos llegan al
  mismo. Los miembros no se replican entre sí. La documentación habla de «hash y módulo».
- [bezenti] Usamos **rendezvous hashing** (HRW): para cada miembro `i` (en el orden de la lista),
  `puntuación = XXH3-64(clave, semilla = 0x62657a656e746936 + i)`; gana la más alta (empate: el índice menor).
  Es estable entre nodos, versiones y reinicios (XXH3 es un formato congelado; una prueba fija valores) y, a
  diferencia del módulo, al **añadir un miembro al final** solo cambia de sitio ~1/(n+1) de las claves (las que
  pasan al nuevo; ninguna salta entre los viejos).
- Blobs: la clave es el identificador del blob (su hash), así que un mensaje siempre se lee del miembro donde se
  guardó. Memoria: la clave tal cual (con su prefijo de tipo); los cerrojos y arriendos, por su clave completa.

### 4.2 Añadir, quitar o reordenar miembros

- [doc] No se rebalancea nunca. Cambiar la lista reasigna claves; en blobs hace falta migrar datos, en memoria
  solo se pierden datos efímeros (contadores de ritmo, listas grises, cerrojos), como en un reinicio.
- [bezenti] **No rebalanceamos tampoco**, pero los blobs tienen **lectura y borrado de respaldo**: si el blob no
  está en su miembro, se busca en los demás. Así, **añadir un miembro al final es seguro**: lo viejo se sigue
  leyendo y borrando donde quedó, y lo nuevo se reparte entre todos. **Quitar o reordenar** miembros cambia el
  índice de la semilla: lo escrito sigue leyéndose por el respaldo (si el miembro sigue en la lista), pero si se
  quita un miembro sus blobs desaparecen. Coste del respaldo: un blob que de verdad no existe se busca en
  todos los miembros (es raro: los mensajes guardan referencias a blobs que existen).

### 4.3 Lecturas, escrituras y borrados

- Blobs [bezenti]: escribir y sustituir → solo en su miembro; leer → su miembro y, si no está, los demás;
  borrar → su miembro y, si no estaba, los demás; borrado en bloque → agrupado por miembro (cada grupo con el
  borrado en bloque de su miembro). Un miembro de tipo `PostgreSql`/`MySql` puede tener a su vez réplicas.
- Memoria [bezenti]: cada operación va al miembro de su clave; `key_delete_prefix` se hace en todos; el
  almacén es «remoto» (apto para clúster) si todos sus miembros lo son.
- [doc] Fallos: si un miembro cae, fallan las operaciones de sus claves y los demás siguen; no hay conmutación
  a otro miembro: cada miembro debe ser de alta disponibilidad por sí mismo (Redis replicado, S3 replicado).
- [doc] El almacén en memoria repartido cumple el requisito de clúster y puede guardar las instantáneas de
  caché (`cache swap`) si todos sus miembros son Redis [bezenti: cada instantánea va al miembro de su ruta, con
  el mismo reparto]. Stalwart Keep no puede ser miembro (el esquema no lo permite). El coordinador `Default`
  manda los mensajes por el **primer** miembro, para que todos los nodos usen el mismo Redis.

## 5. Pruebas

- Unitarias (`cargo test -p store --features "postgres sqlite bezenti" --lib backend::bezenti`): reparto estable
  (valores fijos), equilibrado (±5 % con 2, 3 y 5 miembros) y que añadir un miembro mueve ~1/4 de las claves y
  solo al nuevo; particiones de rangos, la regla «lee lo que escribiste» y la comparación de posiciones (LSN,
  GTID de MySQL y de MariaDB); blobs repartidos en tres directorios (cada blob en su fragmento y solo en él,
  reparto razonable, borrado suelto y en bloque, añadir un cuarto fragmento y seguir leyendo y borrando lo
  viejo); memoria repartida (cada clave en su miembro, contadores, prefijos, cerrojos, arriendos y ritmo).
- Integración con servidores reales (`pruebas_reales.rs`; `bezenti/pruebas/postgres_replica.sh` levanta un
  PostgreSQL primario y una réplica física sin root, y `bezenti/pruebas/mariadb_replica.sh` un MariaDB primario y
  una réplica GTID, también sin root; el mismo escenario para los dos): 150 escrituras seguidas de lectura nunca
  ven un valor viejo y van al primario; luego las lecturas, recorridos y blobs van a la réplica; con la
  replicación en pausa lo escrito por este nodo se sigue leyendo bien, lo escrito por otro nodo se ve viejo
  (prueba de que leyó la réplica) hasta que pasan los 5 s y entonces va al primario; al reanudar, vuelve a la
  réplica; con la réplica parada todo sigue funcionando con el primario y, al arrancarla, vuelve a usarse. En la
  CI corre la de PostgreSQL (la de MariaDB, donde haya binarios de MariaDB).
- Servidor completo (`bezenti/pruebas/replicas_reparto.py`): almacén de blobs repartido en tres directorios
  (30 correos por SMTP leídos íntegros por IMAP y JMAP, reparto entre los tres, ningún blob repetido, siguen
  tras reiniciar) y, con `--pg`, almacén de datos PostgreSQL con réplica (correo leído al momento, marcas JMAP
  escritas y leídas al momento sin valores viejos, la réplica atiende consultas según sus propias
  estadísticas, con la réplica parada el servidor sigue recibiendo y sirviendo correo, y al volver se usa).

## 6. Pendiente

- Probar MySQL (no MariaDB) con réplicas reales: la posición por `gtid_executed` solo tiene pruebas unitarias.
- Probar el almacén en memoria repartido con varios Redis reales (las pruebas usan miembros locales).
- Un registro de escrituras compartido entre nodos (hoy la garantía de 2.2 es por nodo).
