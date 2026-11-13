# Stalwart, edición bezenti

Servidor de correo [Stalwart](https://github.com/stalwartlabs/stalwart) **100 % AGPL-3.0**, sin una sola línea
de código de la licencia comercial (SEL), y con las funciones de la edición Enterprise **reimplementadas desde
cero** como software libre: multi-inquilino, IA antispam, direcciones enmascaradas, archivo y recuperación,
historial de telemetría y alertas, réplicas y almacenes repartidos, y SCIM.

No es un producto de Stalwart Labs ni está respaldado por ellos. No contiene, no modifica ni elude su comprobación
de licencia: simplemente no incluye su código de pago, y el nuestro es independiente.

## Cómo está hecho

```
stalwartlabs/stalwart  (vX.Y.Z, AGPL O SEL)
        │  bezenti/sala-limpia.py: quita los 68 archivos SEL y los ~2.100 fragmentos SEL
        ▼
rama limpio-upstream   (vX.Y.Z solo AGPL; un commit por versión oficial)
        │  merge
        ▼
rama bezenti           (limpio-upstream + nuestro código, todo AGPL-3.0-only)
```

Para pasar a una versión nueva de Stalwart: limpiarla con la sala limpia, hacer commit en `limpio-upstream`
y fusionarla en `bezenti`. Los conflictos solo aparecen donde nuestro código toca algo que cambió arriba.

## Reglas de la sala limpia (obligatorias)

1. **Nadie que escriba código para esta edición abre código SEL.** Se trabaja sobre la rama `bezenti`, que ya no
   lo contiene. Prohibido mirar los archivos o fragmentos SEL de `stalwartlabs/stalwart`, sus binarios
   desensamblados o cualquier copia.
2. Fuentes permitidas: este repositorio, la **documentación pública** de Stalwart (stalw.art/docs), el **esquema**
   `resources/schema/schema.json.gz` (definición de la API, con la licencia AGPL del proyecto), los RFC y
   estándares, y las especificaciones de `bezenti/especificaciones/`.
3. Las marcas `// [fragmento SEL eliminado: N líneas]` y `// [archivo SEL eliminado: N líneas]` dicen **dónde**
   había código de pago, no qué hacía. Sirven para saber dónde engancharse; no se intenta adivinar su contenido.
4. Todo el código nuevo lleva la cabecera `SPDX-License-Identifier: AGPL-3.0-only` y va tras la feature de Cargo
   **`bezenti`** cuando reemplaza una función de pago. Nunca se usa la feature `enterprise` ni se toca
   `x:Enterprise` (licencias).
5. La CI comprueba en cada push que no hay código SEL (`bezenti/sala-limpia.py --comprobar .`) y que todo
   archivo nuevo es AGPL-3.0-only.
6. Compatibilidad: implementamos los mismos objetos de la API que define el esquema (`x:Tenant`, `x:MaskedEmail`…),
   para que clientes y paneles que hablen esa API funcionen con nosotros. Interfaz, no implementación.

## Plan

| Fase | Contenido | Estado |
|---|---|---|
| 0 | Sala limpia, rama limpia de la v1.1.0, CI con comprobaciones | hecho |
| 1 | Generador propio del registro a partir del esquema (structs, enums, propiedades) para los objetos de pago, tras la feature `bezenti` — [`generador/`](generador/README.md), modelo en [`especificaciones/modelo.md`](especificaciones/modelo.md); fidelidad: 0 líneas distintas frente a la rama limpia | hecho |
| 2 | **Multi-inquilino**: inquilinos (`x:Tenant`), dominios, cuentas, grupos, listas y roles por inquilino; administradores limitados a su inquilino (aislamiento en toda la API, directorio, compartir y OAuth); techo de permisos; cuotas de objetos y de disco por inquilino; logo por dominio/inquilino (`/logo`); extensión propia: **revendedores** (inquilinos con subinquilinos, `bezentiParentTenantId`) — [`especificaciones/multi-inquilino.md`](especificaciones/multi-inquilino.md), guía [`guias/alta-de-empresa.md`](guias/alta-de-empresa.md), pruebas `pruebas/multi_inquilino.py`. (La cola SMTP y los informes externos por inquilino, que quedaron pendientes, se hicieron en la fase 8) | hecho |
| 3 | **Direcciones enmascaradas** (`x:MaskedEmail`: alta con prefijo/dominio/caducidad, límite `maxMaskedAddresses`, entrega por SMTP reescribiendo el `RCPT TO`, rechazo si está desactivada/caducada/borrada) y **archivo de borrados** (`x:ArchivedItem`: los mensajes borrados por JMAP/IMAP/POP3/vaciado automático se guardan `archiveDeletedItemsFor`; restaurar íntegro con buzones y etiquetas; purga al vencer con la limpieza de blobs; vista del administrador de inquilino; recuperar cuentas borradas con `archiveDeletedAccountsFor`) — [`especificaciones/enmascaradas.md`](especificaciones/enmascaradas.md), [`especificaciones/archivo.md`](especificaciones/archivo.md), pruebas `pruebas/archivo_enmascaradas.py`. (Archivar archivos, eventos, contactos y Sieve y recuperar cuentas desde el administrador de inquilino se hicieron en la fase 8; la API de Fastmail para enmascaradas se descartó, ver `enmascaradas.md` §8) | hecho |
| 4 | **IA**: modelos configurables (`x:AiModel`: OpenAI y compatibles —Azure, vLLM, llama.cpp—, Anthropic, Gemini, Ollama, TypeSafe) con tiempo de espera, un reintento, concurrencia, ritmo y cortacircuitos; **antispam con IA** (`x:SpamAi`: compuerta para los dudosos, hechos verificados, niveles con escalado, etiquetas `AI_*`, modo sombra, cabecera `X-Spam-AI`, aprendizaje, `ClassifySpam` con `aiFacts`/`aiGate`/`aiVerdict`); **`llm_prompt` en Sieve** (permiso `interactAi`); eventos, métricas y claves de traza `ai.*` — [`especificaciones/ia.md`](especificaciones/ia.md), guía [`guias/antispam-con-ia.md`](guias/antispam-con-ia.md), pruebas `pruebas/ia.py` (proveedores falsos) y `pruebas/ia_real_anthropic.py` (manual). Pendiente: `webSearch` | hecho |
| 5 | **Telemetría con historial y alertas**: historial de entregas (`x:Trace`, vistas de entrada y salida, filtros por texto, fecha, cola y tipo) y de métricas (`x:Metric`, un registro por nodo e intervalo con diferencias), en el almacén de datos o uno aparte (`x:TracingStore`, `x:MetricsStore`), escrito por lotes fuera del camino del correo; retención y purga (`holdTracesFor`, `holdMetricsFor`); alertas (`x:Alert`: `metric(...)` → correo por el propio servidor y/o evento para webhooks con reintentos, con histéresis: un aviso al dispararse y otro al resolverse); telemetría en vivo por SSE (`/api/live/tracing` con filtros y seguimiento de sesión, `/api/live/metrics`, tokens de 60 s); aislamiento: cada inquilino ve sus trazas, sus contadores y sus sesiones en vivo — [`especificaciones/telemetria.md`](especificaciones/telemetria.md), pruebas `pruebas/telemetria.py`. (El estado de las alertas sobrevive a los reinicios desde la fase 8; no se comparte entre nodos: cada nodo avisa con sus métricas, como dice la documentación) | hecho |
| 6 | **Réplicas de lectura y almacenes repartidos**: `readReplicas` en PostgreSQL y MySQL/MariaDB (lecturas por turnos entre réplicas sanas; escrituras, búsqueda y `sql_query` al primario; regla «lee lo que escribiste» por partición con posiciones de replicación —LSN, GTID— verificadas cada 200 ms; réplica caída o atrasada más de 5 s → primario, y vuelve sola), `urlFailover` en LDAP, almacén de blobs repartido (`BlobStore.Sharded`: rendezvous hashing estable, añadir miembros al final sin perder nada gracias a la lectura de respaldo) y en memoria/lookup repartido (`Sharded` sobre Redis, también para instantáneas de caché y el coordinador) — [`especificaciones/replicas-y-reparto.md`](especificaciones/replicas-y-reparto.md), pruebas unitarias en `store` (`backend::bezenti`), integración con PostgreSQL y MariaDB primario + réplica reales (`pruebas/postgres_replica.sh`, `pruebas/mariadb_replica.sh`) y servidor completo `pruebas/replicas_reparto.py`. Pendiente: probar MySQL (no MariaDB) y Redis repartido con servidores reales; registro de escrituras compartido entre nodos | hecho |
| 7 | **SCIM 2.0** (RFC 7643/7644/9865) en `/scim/v2`: `Users`, `Groups`, `/.search`, `Bulk` (con `bulkId`, `failOnErrors`), descubrimiento sin autenticación (`ServiceProviderConfig`, `ResourceTypes`, `Schemas`), `/Me` → 501; token de portador (clave API de la cuenta de aprovisionamiento, permiso `scimAccess` + `sysAccount*`; Basic → 401); solo dominios con `allowScimProvisioning`; filtros completos del RFC (`eq ne co sw ew pr gt ge lt le`, `and`/`or`/`not`, filtros de valor) con búsqueda por índice; orden; paginación por índice y por cursor; proyección (`attributes`/`excludedAttributes`); PATCH con las rarezas de Entra y Okta; ETag/`If-Match`/`If-None-Match`; `active` ↔ permiso `authenticate`; alias, `externalId`, locale y zona; grupos con dirección derivada y miembros; atributos conocidos aceptados y descartados, desconocidos → 400; aislamiento por inquilino (lo ajeno → 404) y cuotas; autoridad de SCIM sobre el aprovisionamiento justo a tiempo de los directorios externos; eventos `scim.*` — [`especificaciones/scim.md`](especificaciones/scim.md), guía [`guias/scim-entra-okta-keycloak.md`](guias/scim-entra-okta-keycloak.md), pruebas `pruebas/scim.py` y conformidad con `scim2-tester` (`pruebas/scim_conformidad.py`). Pendiente: probar contra Entra/Okta reales (necesitan el servidor publicado); prueba de integración de la autoridad sobre un directorio OIDC/LDAP | hecho |
| 8 | **Cierre de pendientes**: **cola SMTP por inquilino** (cada mensaje sabe de qué inquilino es; límites propios en `x:Tenant`: `bezentiMaxQueuedMessages` —mensajes a la vez en la cola, `452 4.3.1`—, `bezentiMessageRate` y `bezentiRecipientRate` —mensajes y destinatarios por intervalo, `451 4.7.1`—, jerárquicos para revendedores; **reparto justo** de los hilos de entrega entre inquilinos, así un envío masivo de uno no retrasa el correo de otro; el administrador de inquilino ve, filtra, descarga, reintenta y cancela solo los mensajes de su ámbito); **informes externos por inquilino** (DMARC/TLS/ARF con el inquilino del dominio al que se refieren; cada administrador ve y borra solo los suyos); **archivo de archivos, eventos, contactos y guiones Sieve** (borrados por WebDAV, JMAP o ManageSieve; mover no archiva; restauración íntegra en su sitio con su nombre, cuotas y UID comprobados; purga al vencer); **recuperar cuentas borradas desde el administrador de inquilino** (solo las de su ámbito, con su cuota); el `blobId` de un mensaje archivado descarga el **mensaje íntegro** con las cabeceras del servidor; **estado de las alertas persistente** tras reiniciar — [`especificaciones/multi-inquilino.md`](especificaciones/multi-inquilino.md) §4 y §5.1, [`especificaciones/archivo.md`](especificaciones/archivo.md) §3, §4, §6 y §7, [`especificaciones/telemetria.md`](especificaciones/telemetria.md) §7, pruebas `pruebas/cola_inquilinos.py` y `pruebas/archivo_tipos.py` (y las de las fases 2, 3 y 5 ampliadas) | hecho |

## Estado: qué tiene la edición bezenti frente a la Enterprise

Resumen honesto tras la fase 8. «Hecho» significa reimplementado desde cero, AGPL, tras la feature `bezenti` y con
pruebas de integración contra el servidor real (`bezenti/pruebas.sh`, que lanza la CI). Lo que no está, o está
distinto, se dice.

| función de la edición Enterprise (documentación pública) | edición bezenti |
|---|---|
| Multi-inquilino: inquilinos, dominios, cuentas, grupos, listas, roles, clientes OAuth, DKIM, ACME, DNS por inquilino; aislamiento; techo de permisos; cuotas de objetos y de disco; logo | hecho (fase 2), más **revendedores** (inquilinos con subinquilinos), que no tiene la Enterprise |
| Cola SMTP por inquilino (ver y gestionar su cola) | hecho (fase 8), más **límites de envío y de cola por inquilino** y **reparto justo** al entregar (extensiones propias: el esquema no los define) |
| Informes externos (DMARC, TLS, ARF) por inquilino | hecho (fase 8) |
| Direcciones enmascaradas | hecho (fase 3). La API JMAP de Fastmail (`https://www.fastmail.com/dev/maskedemail`) no: se descartó (`enmascaradas.md` §8) |
| Archivo de borrados y restauración: correo, archivos, eventos, contactos, Sieve; purga al vencer | hecho (fases 3 y 8). Se restaura al sitio original (o al por defecto); no se puede elegir otro destino |
| Recuperar cuentas borradas (`archiveDeletedAccountsFor`) | hecho (fase 3), también desde el administrador de inquilino (fase 8) |
| IA: modelos (OpenAI y compatibles, Anthropic, Gemini, Ollama, TypeSafe), antispam con IA, `llm_prompt` en Sieve | hecho (fase 4). `webSearch` no (el esquema solo define `Disable`); TypeSafe sin probar con una cuenta real; el cortacircuitos y el ritmo se reinician al recargar la configuración |
| Historial de trazas y métricas, alertas, telemetría en vivo, aislada por inquilino | hecho (fase 5). Estado de alertas persistente desde la fase 8, pero por nodo (no compartido en un clúster) |
| Réplicas de lectura (PostgreSQL, MySQL/MariaDB), LDAP con conmutación, almacenes de blobs y en memoria repartidos | hecho (fase 6). MySQL (no MariaDB) y Redis repartido solo con pruebas unitarias o miembros locales; la garantía «lee lo que escribiste» es por nodo |
| SCIM 2.0 (Entra ID, Okta, Keycloak…) | hecho (fase 7). Probado con `scim2-tester` y con peticiones como las de Entra y Okta, no contra sus servicios reales |
| Directorios externos (LDAP/SQL/OIDC) por dominio o por inquilino (`x:Domain.directoryId`) | **no**: cada dominio usa el directorio del servidor. Los inquilinos pueden tener sus objetos `x:Directory` (aislados), pero el inicio de sesión no se enruta por dominio |
| Mantenimiento de inquilinos (`TenantMaintenance`, `recalculateQuota`, `resetTenantQuotas`) | no hace falta: el uso de disco del inquilino se calcula al momento (`multi-inquilino.md` §5) |
| Evento y métrica `limit.tenant-quota` | no: se usa `limit.quota` con el detalle `tenant` |
| Licencias (`x:Enterprise`, logo general de la instancia) | no, a propósito: esta edición no tiene licencias |
| Panel web | los objetos se sirven en `/api/schema` (con las extensiones propias en los formularios), pero la interfaz no se ha probado a fondo |
