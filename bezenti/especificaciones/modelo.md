# Modelo de datos de las funciones de pago (según el esquema)

**Fuente**: `resources/schema/schema.json.gz` de Stalwart v1.1.0 (definición de la API, AGPL), comparado con
`resources/schema/schema_oss.json.gz`. Lo que aquí se describe es la **interfaz** (objetos, campos, tipos y
relaciones) que el esquema publica; no dice nada de cómo se implementa. El código Rust de estos tipos lo genera
`bezenti/generador` tras `#[cfg(feature = "bezenti")]` (fase 1). Es la base de las fases 2 a 7.

Notación: `id→X` es el identificador de un objeto X; *nulable* = puede ser `null`; *servidor* = lo pone el
servidor (no se puede escribir); *inmutable* = solo al crear. Entre corchetes, los límites del esquema.

## 1. Objetos nuevos

El esquema completo define 15 objetos que no están en el libre. `x:Enterprise` (licencia y logo por defecto del
panel) **no se implementa**; su logo por defecto, si se quiere, irá en la fase 2 con otro nombre.

### `x:Tenant` — inquilino (fase 2)

Objeto (`sysTenant{Get,Create,Update,Destroy,Query}`). Entidad aislada con sus propios recursos y cuotas.

| campo | tipo | notas |
|---|---|---|
| `name` | cadena | obligatorio; único (decisión bezenti) |
| `logo` | cadena nulable | URL o imagen en base64 |
| `permissions` | `x:Permissions` | `Inherit` / `Merge` / `Replace` con lista de permisos |
| `roles` | `x:Roles` | `Default` / `Custom` con ids de roles |
| `quotas` | mapa `TenantStorageQuota` → entero | `maxAccounts`, `maxGroups`, `maxDomains`, `maxMailingLists`, `maxRoles`, `maxOauthClients`, `maxDkimKeys`, `maxDnsServers`, `maxDirectories`, `maxAcmeProviders`, `maxDiskQuota` |
| `usedDiskQuota` | tamaño, servidor | espacio usado |
| `createdAt` | fecha, servidor | |

**Relaciones**: casi todos los objetos del directorio y de la configuración por dominio tienen un campo nuevo
`memberTenantId: id→x:Tenant` nulable (ver §2): cuentas de usuario y de grupo, dominios, listas de correo,
roles, clientes OAuth, firmas DKIM 1 y 2, servidores DNS (los 45 proveedores), directorios LDAP/SQL/OIDC,
proveedores ACME e informes externos ARF/DMARC/TLS. Las listas del panel filtran por ese campo. Además:
`x:Authentication.defaultTenantRoleIds` (roles por defecto de los inquilinos), `x:Cache.tenants` (tamaño de la
caché de inquilinos, mín. 2048), la tarea `TenantMaintenance` (`tenantId`, `maintenanceType: recalculateQuota`),
`TaskStoreMaintenanceType.resetTenantQuotas` y el evento/métrica `limit.tenant-quota`.

### `x:MaskedEmail` — dirección enmascarada (fase 3)

Objeto de una cuenta (`OBJ_FILTER_ACCOUNT`; la lista filtra por `accountId`).

| campo | tipo | notas |
|---|---|---|
| `accountId` | id→`x:Account` | inmutable |
| `email` | cadena, servidor | la dirección asignada (única, decisión bezenti) |
| `emailPrefix`, `emailDomain` | cadena nulable, inmutable | solo al crear: prefijo/dominio que debe tener la dirección |
| `forDomain` | cadena nulable | sitio para el que se creó |
| `url` | cadena nulable | enlace de vuelta del integrador |
| `description` | cadena nulable | |
| `createdBy` | cadena nulable | cliente que la creó |
| `enable` | booleano | por defecto `true` |
| `expiresAt` | fecha nulable, inmutable | caducidad |
| `createdAt` | fecha, servidor | |

Relacionados: `x:Email.maxMaskedAddresses` (entero nulable, mín. 1: máximo por usuario) y la cuota
`StorageQuota.maxMaskedAddresses`.

### `x:ArchivedItem` — elemento archivado (fase 3)

Objeto múltiple (`@type`), de una cuenta, sin `Create` (los crea el servidor al borrar). Variantes:
`Email` (`from`, `subject`, `receivedAt`, `size`), `FileNode` (`name`, `createdAt`), `CalendarEvent` (`title`,
`startTime`, `createdAt`), `ContactCard` (`name`, `createdAt`), `SieveScript` (`name`, `contents`, `createdAt`).
Todas tienen `accountId: id→x:Account`, `blobId`, `archivedAt`, `archivedUntil` (fecha de borrado definitivo) y
`status: ArchivedItemStatus` (`archived` | `requestRestore`: pedir la restauración es cambiar el estado).

Relacionados: `x:DataRetention.archiveDeletedItemsFor` y `archiveDeletedAccountsFor` (duraciones nulables:
cuánto se guarda lo borrado; `null` = no se archiva), la tarea `RestoreArchivedItem` (`accountId`,
`archivedItemType`, `blobId`, `archivedUntil`, `createdAt`, todos del servidor) y el permiso
`taskRestoreArchivedItem`. Decisión bezenti: `OBJ_NOT_STORED` (se lee del almacén con lógica propia).

### `x:AiModel` — modelo de IA (fase 4)

Objeto múltiple: `OpenAi` (con `profile`: `OpenAi`, `Azure`, `Vllm`, `LlamaCpp`, `Generic`), `Anthropic`,
`Gemini`, `Ollama` (con `keepAlive`), `TypeSafe`. Campos comunes: `name` (referencia corta; único, decisión
bezenti), `model`, `url` nulable (si falta, la del proveedor), `apiKey: x:SecretKeyOptional`, `httpHeaders`
(mapa), `allowInvalidCerts`, `timeout` (60 s), `maxConcurrentRequests` [1–4096] (16), `rateLimit: x:Rate`
nulable. Generativos (todos menos TypeSafe): `maxOutputTokens` [1–1.000.000] (4096), `temperature` [0–2]
nulable, `reasoning` (`Default`, `Off`, `Low`, `Medium`, `High`), `extraParameters` (JSON que se mezcla en cada
petición); `logprobs` en OpenAi/Gemini/Ollama. Se usa en la IA antispam y en la función Sieve `llm_prompt`
(permiso `interactAi`).

### `x:SpamAi` — IA antispam (fase 4)

Singleton múltiple: `Disable` | `Enable` con `x:SpamAiProperties`:

- `mode` (`Automatic` puntúa | `Shadow` solo registra), `condition` (expresión con las variables de
  `SPAM_AI_VARIABLE`; por defecto la del esquema: solo los dudosos), `deadline`, `breakerThreshold` y
  `breakerCooldown` (cortacircuitos por modelo).
- `tiers`: lista (mín. 1) de `x:SpamAiTier` = `modelId: id→x:AiModel`, `escalateBelow` [0–1] (0,3),
  `escalateOn` (categorías que pasan al siguiente nivel).
- `categories`: lista (mín. 2) de `x:SpamAiCategory` (`name`, `description`, `includes`); generan las etiquetas
  `AI_<CATEGORÍA>_HIGH/MEDIUM/LOW` según `bucketHigh` (0,85) y `bucketMedium` (0,65); `uncertainBelow` da
  `AI_UNCERTAIN`. `signals`: preguntas sí/no (`name`, `question`) que dan `AI_<SEÑAL>`. El esquema trae
  categorías, señales e instrucciones (`instructions`, el «system prompt») por defecto.
- Qué se envía: `maxInputChars`, `trimQuotes`, `includeAuth`, `includeLinks`/`maxLinks`,
  `includeAttachments`/`maxAttachments`; `explanation`, `addHeader` (`X-Spam-AI`), `verbalHigh/Medium/Low`
  (probabilidad para modelos sin `logprobs`), `learn` (`Disable` | `Enable` con `minProbability`, `sampleRate`:
  entrenar el clasificador estadístico) y `webSearch` (solo `Disable` por ahora).

Relacionados: `x:SpamClassify` (acción de prueba) gana `aiFacts`, `aiGate`, `aiVerdict` (servidor); eventos
`ai.*` y claves de traza `model`, `inputTokens`, `outputTokens`, `cachedTokens`, `finishReason`, `category`,
`probability`, `confidence`, `tier`.

### `x:Alert` — alerta (fase 5)

Objeto: `enable` (true), `condition` (expresión sobre métricas), `emailAlert` (`Disabled` | `Enabled`:
`fromAddress`, `fromName`, `to` (mín. 1), `subject`, `body`) y `eventAlert` (`Disabled` | `Enabled`:
`eventMessage`). Eventos `telemetry.alert-event` / `telemetry.alert-message`.

### `x:Metric`, `x:Trace` — historial de telemetría (fase 5)

Solo lectura (`Get`/`Query`). `x:Metric` múltiple: `Counter` y `Gauge` (`x:MetricCount`: `metric`, `count`,
`nodeId`, `timestamp`) e `Histogram` (`x:MetricSum`: además `sum`); la lista filtra por `nodeId`.
`x:Trace`: `from`, `to`, `size`, `timestamp` (servidor) y `events`: lista de `x:TraceEvent` (`event`,
`timestamp`, `keyValues`: lista de `key: Key` + `value: x:TraceValue`, que es múltiple: `String`,
`UnsignedInt`, `Integer`, `Boolean`, `Float`, `UTCDateTime`, `Duration`, `IpAddr`, `List`, `Event`, `Null`).
Vistas `x:Trace/InboundDelivery` y `x:Trace/OutboundDelivery` (filtros texto, fecha y `queueId`).
Decisión bezenti: ambos `OBJ_NOT_STORED` (viven en sus almacenes).

Configuración: singletons `x:MetricsStore` y `x:TracingStore` (`Disabled`, `Default`, `FoundationDb`,
`PostgreSql`, `MySql`), `x:DataRetention.holdMetricsFor`, `holdTracesFor` y `metricsCollectionInterval`
(`x:Cron`), `x:Search.indexTelemetry` e `IndexType.telemetry`, `TaskStoreMaintenanceType.reindexTelemetry`,
permisos `liveTracing` y `liveMetrics`.

## 2. Campos de pago en objetos libres

| objeto | campo | tipo |
|---|---|---|
| ~60 objetos (ver §1, inquilino) | `memberTenantId` | id→`x:Tenant` nulable |
| `x:UserAccount`, `x:GroupAccount` | `externalId` | cadena nulable (identificador SCIM, fase 7) |
| `x:Domain` | `directoryId` | id→`x:Directory` nulable (directorio de las cuentas del dominio) |
| `x:Domain` | `allowScimProvisioning` | booleano (fase 7) |
| `x:Domain` | `logo` | cadena nulable |
| `x:Authentication` | `defaultTenantRoleIds` | conjunto de id→`x:Role` |
| `x:Cache` | `tenants` | tamaño [mín. 2048] |
| `x:CalendarAlarm` | `template` | HTML nulable (plantilla de avisos) |
| `x:CalendarScheduling` | `emailTemplate`, `httpRsvpTemplate` | HTML nulable |
| `x:DataRetention` | `archiveDeletedItemsFor`, `archiveDeletedAccountsFor`, `holdMetricsFor`, `holdTracesFor` | duración nulable |
| `x:DataRetention` | `metricsCollectionInterval` | `x:Cron` |
| `x:Email` | `maxMaskedAddresses` | entero nulable [mín. 1] |
| `x:LdapDirectory` | `urlFailover` | conjunto de URI (servidores de respaldo) |
| `x:OidcDirectory` | `trustedProvider` | booleano |
| `x:MySqlStore`, `x:PostgreSqlStore` | `readReplicas` | lista de `x:MySqlSettings` / `x:PostgreSqlSettings` (`host`, `port`, `database`, `authUsername`, `authSecret`, `options`) — fase 6 |
| `x:Search` | `indexTelemetry` | booleano |
| `x:SpamClassify` | `aiFacts`, `aiGate`, `aiVerdict` | cadenas del servidor |

Variantes nuevas en tipos libres: `BlobStore.Sharded` (`x:ShardedBlobStore`: `stores`, lista de mín. 2 de
`x:BlobStoreBase`), `InMemoryStore.Sharded` y `LookupStore.Sharded` (`x:ShardedInMemoryStore`: lista de
`x:InMemoryStoreBase`) — fase 6; `TaskType.RestoreArchivedItem` y `TenantMaintenance`; permisos nuevos de
todos los objetos anteriores más `interactAi`, `liveTracing`, `liveMetrics`, `scimAccess`.

## 3. Decisiones de la fase 1 (no vienen del esquema)

Están en `bezenti/generador/decisiones.json` con su porqué; se pueden revisar en cada fase:

- Indicadores: `x:Tenant` con `OBJ_SEQ_ID`; `x:ArchivedItem`, `x:Metric`, `x:Trace` con `OBJ_NOT_STORED`.
- Índices: `x:Tenant` texto + nombre único; `x:MaskedEmail` por cuenta + dirección única; `x:AiModel` nombre
  único; `memberTenantId` búsqueda por id (salvo informes externos, solo clave ajena); `externalId` búsqueda
  exacta; todo identificador de objeto de pago es clave ajena.
- Contextos de expresión: `SpamAiProperties.condition` usa `SPAM_AI_VARIABLE`; `Alert.condition` queda sin
  variables: las condiciones de alerta leen métricas con la función `metric('…')`, que ya existe en el motor de
  expresiones libre (fase 5, [`telemetria.md`](telemetria.md)).
- Fase 2: `indice_campos` (qué campo alimenta cada índice de un objeto nuevo; corrige que el nombre de
  `x:Tenant` no fuera único) y el campo propio `x:Tenant.bezentiParentTenantId` (`extensiones.json`, revendedores;
  ver [`multi-inquilino.md`](multi-inquilino.md)).
- `x:Metric` necesita un enum de variantes que el esquema no nombra: se genera `MetricVariantType`
  (`Counter`, `Gauge`, `Histogram`), porque `MetricType` ya es el enum de métricas de `trc`.

## 4. Pendiente para fases siguientes

- Fase 4: `trc::Key` ya tiene las claves de IA y `trc::EventType`/`MetricType` los eventos y métricas `ai.*`
  (tras la feature `bezenti` de `trc`; identificadores en [`ia.md`](ia.md), §6). Fase 5: eventos
  `telemetry.alert-event`, `telemetry.alert-message` y `telemetry.metrics-stored` y sus métricas (identificadores
  en [`telemetria.md`](telemetria.md), §9). Faltan los de otras fases (`scim.*`, `limit.tenant-quota`, …): se
  añadirán en `trc` en cada fase, sin reutilizar los identificadores ya asignados.
- Toda la lógica: cuotas y aislamiento por inquilino (hecho en la fase 2, [`multi-inquilino.md`](multi-inquilino.md)),
  asignación y entrega de direcciones enmascaradas y archivo y restauración (hechos en la fase 3,
  [`enmascaradas.md`](enmascaradas.md) y [`archivo.md`](archivo.md)), clientes de IA (hechos en la fase 4, [`ia.md`](ia.md)), almacenes de métricas/trazas (hechos en la fase 5, [`telemetria.md`](telemetria.md)), réplicas y almacenes repartidos, SCIM.
  En el código limpio, las marcas `[fragmento SEL eliminado]` fuera del registro indican dónde engancharla.
