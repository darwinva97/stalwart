# Telemetría con historial y alertas (fase 5)

Especificación de la edición bezenti. Escrita **solo** con fuentes permitidas: la documentación pública de
Stalwart (citada con su URL), el esquema `resources/schema/schema.json.gz` y el código AGPL de la rama `bezenti`.
Cada punto dice de dónde sale:

- **[doc]**: lo dice la documentación pública (con la página).
- **[esquema]**: lo define el esquema de la API.
- **[bezenti]**: decisión nuestra, porque la documentación no lo dice o porque la cambiamos a propósito.

Páginas usadas: <https://stalw.art/docs/telemetry/>, `…/telemetry/history/`, `…/telemetry/management/`,
`…/telemetry/alerts/`, `…/telemetry/live/`, `…/telemetry/metrics/`, `…/telemetry/webhooks/` y las referencias
`…/ref/object/{trace,metric,alert,metrics-store,tracing-store,data-retention}/`.

Objetivo: que el operador y cada empresa (inquilino) puedan saber qué pasó con un correo (historial de entregas),
ver la evolución del servidor (historial de métricas), recibir avisos sin un sistema de monitorización externo
(alertas) y seguir en directo lo que pasa (telemetría en vivo), **gastando poca CPU y poco disco**: el servidor
real va en una máquina pequeña.

## 1. Piezas y objetos

- [doc] Hay dos historiales: **trazas de entrega** (los eventos de cada sesión SMTP que recibió un mensaje y de
  cada intento de entrega, una traza por sesión; alimentan el historial de entrada y de salida del panel) y
  **métricas** (los valores de las métricas, guardados por cada nodo a intervalos; alimentan el panel de control).
- [esquema] Objetos: `x:Trace` (solo `get`/`query`) con `from`, `to`, `size`, `timestamp` (del servidor) y
  `events` (lista de `x:TraceEvent`: `event`, `timestamp`, `keyValues` con `key` y un `value` tipado por `@type`:
  `String`, `UnsignedInt`, `Integer`, `Boolean`, `Float`, `UTCDateTime`, `Duration`, `IpAddr`, `List`, `Event`,
  `Null`); vistas `x:Trace/InboundDelivery` y `x:Trace/OutboundDelivery` (filtros `text`, `timestamp`,
  `queueId`, y fijo `event` = `smtp.connection-start` o `delivery.attempt-start`). `x:Metric` (solo
  `get`/`query`, filtro `nodeId`), múltiple: `Counter`, `Gauge` (`metric`, `count`, `nodeId`, `timestamp`) e
  `Histogram` (además `sum`). `x:Alert` (CRUD). Singletons `x:TracingStore` y `x:MetricsStore` (`Disabled`,
  `Default`, `FoundationDb`, `PostgreSql`, `MySql`). En `x:DataRetention`: `holdTracesFor` (30 días),
  `holdMetricsFor` (90 días) y `metricsCollectionInterval` (`x:Cron`, cada hora en punto). Permisos `sysTrace*`,
  `sysMetric*`, `sysAlert*`, `sysTracingStore*`, `sysMetricsStore*`, `liveTracing`, `liveMetrics`. Eventos
  `telemetry.alert-event`, `telemetry.alert-message`, `telemetry.metrics-stored`.
- [doc] Los objetos `x:Trace` y `x:Metric` no se pueden crear, cambiar ni borrar: son del servidor, que los borra
  al vencer su retención.

## 2. Qué se guarda en el historial de entregas

- [doc] Una traza por cada **sesión SMTP de entrada que encola al menos un mensaje** y por cada **intento de
  entrega**. Con los eventos de protocolo, autenticación, filtrado y entrega de la sesión, **hasta 2048 por
  traza**, sin los datos del protocolo ni otros eventos de bajo nivel. No se guardan sesiones IMAP, POP3, JMAP ni
  HTTP, ni sesiones SMTP que terminan sin encolar nada.
- [bezenti] Eventos que se guardan: los de los grupos `smtp`, `delivery`, `queue`, `auth`, `spf`, `dkim`,
  `dmarc`, `arc`, `iprev`, `mail-auth`, `spam`, `sieve`, `message-ingest`, `mta-sts`, `dane`, `milter`,
  `mta-hook`, `security`, `limit` y `tls`, salvo `*.raw-input`/`*.raw-output` y los de nivel `trace`. Se
  agrupan por `spanId`: la traza de entrada va de `smtp.connection-start` a `smtp.connection-end`; la de salida,
  de `delivery.attempt-start` a `delivery.attempt-end`. De cada evento se guardan sus claves salvo `spanId`.
- [bezenti] Campos: `timestamp` = inicio de la sesión; en una traza de entrada `from` es el remitente del primer
  mensaje encolado, `to` todos los destinatarios de los mensajes encolados (separados por `, `) y `size` la suma
  de sus tamaños; en una de salida, remitente, destinatarios pendientes y tamaño del mensaje (los del evento
  `delivery.attempt-start`). El «estado» de la entrega son sus propios eventos (`delivery.delivered`,
  `delivery.mx-lookup-failed`, `delivery.attempt-end`…).
- [bezenti] Dos detalles del código libre que el agrupador tiene en cuenta: (1) las sesiones SMTP y los intentos
  de entrega sacan su `spanId` de generadores distintos y a veces coinciden, así que las sesiones abiertas se
  distinguen por `(spanId, tipo)`; (2) el colector vacía por turnos las colas de cada hilo y los eventos de una
  sesión que cambia de hilo llegan desordenados (incluso antes que su inicio o después de su cierre). Por eso los
  eventos sin sesión abierta se guardan como «huérfanos» hasta 60 s y se adoptan al llegar el inicio, una traza
  se guarda 2 s después de su evento de cierre, y al guardarla se ordenan sus eventos por fecha (inicio primero,
  cierre al final).
- [bezenti] El id de la traza es el `spanId` de la sesión (un id «snowflake» ordenado por tiempo) con el bit más
  bajo indicando entrada (0) o salida (1). Así ordenar por id es ordenar por fecha y un filtro de fechas es un
  rango de claves.

## 3. Dónde y cómo se guarda (coste bajo)

- [doc] `Default` guarda en el almacén de datos; `PostgreSql`, `MySql` y `FoundationDb` usan una base aparte con
  los mismos campos que esos almacenes; `Disabled` no guarda nada. [doc] Una instalación nueva usa `Default`.
- [bezenti] **En esta edición el valor por defecto es `Disabled`** (el que da el generador del registro): el
  historial se activa a propósito desde el panel. Motivo: el servidor real es pequeño y el historial ocupa disco
  en proporción al correo × `holdTracesFor`.
- [bezenti] `Default` reutiliza el almacén de datos configurado (SQLite, PostgreSQL, RocksDB…) en los subespacios
  que el código libre ya reserva para telemetría (`TelemetrySpan`, `o`, y `TelemetryMetric`, `x`): no hacen
  falta tablas nuevas. Un almacén aparte se abre una vez y se reutiliza en las recargas de configuración.
- [bezenti] **Nunca en el camino del correo.** El historial es un suscriptor más del colector de eventos (como
  los trazadores de registro): recibe los eventos por lotes en su propia tarea y, si se queda atrás, el colector
  descarta eventos (`lossy`) en vez de esperar. Como mucho se pierde parte de una traza, nunca se retrasa un
  correo.
- [bezenti] **Escritura por lotes.** Las trazas cerradas se acumulan y se escriben en una sola transacción cada
  5 s o cada 128 trazas, lo que llegue antes. Por traza se escriben 3–4 claves:

  | clave (subespacio `o`) | valor |
  |---|---|
  | `[0] id` | la traza: versión, inquilinos, y los eventos en formato binario compacto (enteros LEB128, una etiqueta de un byte por valor), comprimidos con zstd si pasan de 1 KiB |
  | `[1] id` | resumen: tipo, fecha, tamaño, remitente, destinatarios, colas, inquilinos y las palabras del filtro `text` |
  | `[2] cola id` | índice por id de cola (vacío) |
  | `[3] inquilino id` | índice por inquilino (vacío) |

- [doc] Stalwart indexa además cada traza en el índice de búsqueda (`x:Search.indexTelemetry`).
  [bezenti] **No usamos el índice de búsqueda**: duplicaría la escritura y el disco. El filtro `text` busca en el
  resumen (direcciones, IP remota, EHLO, ids de mensaje y de cola), así que `indexTelemetry` no tiene efecto y la
  tarea `reindexTelemetry` termina sin hacer nada.
- [bezenti] Inquilinos de una traza: los dueños de los dominios del remitente y de los destinatarios y el de la
  cuenta autenticada, resueltos con las cachés de dominios y cuentas al volcar el lote.

## 4. Historial de métricas

- [doc] Cada nodo guarda un conjunto fijo de métricas cada `metricsCollectionInterval` (por defecto cada hora en
  punto) y las guarda `holdMetricsFor` (90 días). Los valores son **diferencias con el punto anterior del mismo
  nodo**: en un `Counter`, `count` es el número de eventos del intervalo; en un `Histogram`, `count` y `sum` son el
  número y la suma de las observaciones del intervalo (la media es `sum/count`); en un `Gauge`, `count` es el valor
  al guardarlo. El primer punto tras arrancar cubre desde el arranque. `nodeId` es 0 en un servidor sin clúster.
  `cache.swap-pending` no se guarda.
- [bezenti] Se guardan todos los contadores que cambiaron en el intervalo (los que no cambiaron no ocupan nada),
  todos los medidores y los histogramas con observaciones. **Un solo registro por nodo e intervalo**
  (`[0] segundos nodo` en el subespacio `x`, decenas de bytes por métrica), así que el coste es casi constante.
  El id de cada punto de la API es `segundos << 22 | nodo << 12 | métrica`: `get` lee un solo registro.
- [bezenti] **Métricas por inquilino**: al volcar las trazas, cada evento de una traza suma 1 al contador de su
  métrica (si la tiene) en cada inquilino de la traza; en cada intervalo se guardan como `[1] inquilino segundos
  nodo`. Es lo único que ve el administrador de un inquilino (§5).
- [bezenti] Filtros de `x:Metric/query`: `nodeId` [doc], `metric` (nombre) y `timestamp` (con
  `timestampIsGreaterThan`, etc.). Orden por id o `timestamp`. Como mucho 50 000 puntos por consulta.

## 5. Consultas y aislamiento por inquilino

- [esquema] `x:Trace/query` acepta `text`, `timestamp` y `queueId`, y las vistas fijan `event`.
  [bezenti] `text` busca sin distinguir mayúsculas; `timestamp` admite `AAAA-MM-DD` (el día entero) o fecha y
  hora RFC 3339, con los operadores `…IsGreaterThan`, `…IsGreaterThanOrEqual`, `…IsLessThan`,
  `…IsLessThanOrEqual`; `queueId` es el id de `x:QueuedMessage` (el que usa la acción «Delivery History» del
  panel) o el número decimal; `event` solo puede ser `smtp.connection-start` o `delivery.attempt-start`. Orden
  por id o `timestamp` (más recientes primero por defecto); se examinan como mucho las 10 000 trazas más
  recientes que cumplen el filtro.
- [bezenti] **Aislamiento**: el operador ve todo. Un usuario de inquilino con `sysTraceGet`/`sysTraceQuery` ve
  las trazas en las que participa su ámbito (su inquilino y sus descendientes): `get` de otra responde
  `notFound` y las consultas no la cuentan. Con `sysMetric*` ve solo los contadores de sus inquilinos (§4), nunca
  medidores ni histogramas del servidor. Estos permisos **no** están en el rol «Tenant Administrator»: el operador
  los da al inquilino (p. ej. `permissions: Merge` con `sysTraceGet`, `sysTraceQuery`, `sysMetricGet`,
  `sysMetricQuery`, `liveTracing`) y sus administradores los reciben (ver `multi-inquilino.md`, §3).
- [bezenti] Una traza con destinatarios de dos inquilinos la ven los dos, completa.
- [bezenti] `x:Alert`, `x:TracingStore` y `x:MetricsStore` son solo del operador (`forbidden` para un usuario de
  inquilino).

## 6. Retención y purga

- [doc] Las trazas se guardan `holdTracesFor` (30 días) y las métricas `holdMetricsFor` (90 días); las borra la
  purga diaria de datos (`dataCleanupSchedule`).
- [bezenti] La purga es la tarea `StoreMaintenance`/`purgeData`: borra las trazas (con sus índices) cuyo id es
  anterior a «ahora − holdTracesFor» y los registros de métricas anteriores a «ahora − holdMetricsFor». `null` =
  no se borra nunca. Los borrados van en lotes de 500–1000 claves.

## 7. Alertas

- [doc] `condition` es una expresión que lee métricas con `metric('<nombre>')` (combinables con `&&` y `||`); un
  nombre mal escrito se rechaza con `Invalid metric name`. `eventAlert` emite `telemetry.alert-event` (nivel
  `warn`) con `eventMessage` como detalle, que llega a los trazadores y a los webhooks que lo seleccionen.
  `emailAlert` envía un correo de `fromAddress` (`fromName`) a `to` con `subject` y `body`, con la cabecera
  `Auto-Submitted: auto-generated`, y lo registra como `telemetry.alert-message`. En mensaje, asunto y cuerpo,
  `%{<métrica>}%` se sustituye por su valor actual (un histograma, por su media); un nombre desconocido se deja
  igual. `enable` la apaga. Se evalúan cada cinco minutos, empezando cinco minutos tras arrancar, en cada nodo
  con sus propias métricas; una condición que no se puede evaluar es falsa.
- [doc] Según la documentación, mientras la condición se cumple se avisa **en cada evaluación** (cada cinco
  minutos). [bezenti] **Lo cambiamos: histéresis.** Se avisa una vez cuando la condición pasa de falsa a
  verdadera (evento con `type: triggered`, correo con el asunto configurado) y otra cuando vuelve a ser falsa
  (`type: resolved`, correo con «[Resolved] » delante del asunto y una línea explicativa); mientras no cambia no se
  repite nada. Fase 8: el estado (qué alertas están disparadas) se guarda en el almacén de datos, por nodo (clave
  `[2] nodo` del subespacio de métricas, que la purga de métricas no toca), así que tras reiniciar una condición
  que se sigue cumpliendo **no** vuelve a avisar, y si dejó de cumplirse mientras el servidor estaba parado se
  avisa de que está resuelta. Para los contadores, que solo crecen, la condición puede quedar cumplida mucho
  tiempo, pero no inunda el buzón.
- [bezenti] El correo sale **por el propio servidor**: se encola como mensaje autogenerado y se entrega como
  cualquier otro (local o remoto). Los webhooks son los `x:WebHook` libres que seleccionan
  `telemetry.alert-event`; sus **reintentos** son los del trazador de webhooks: si la petición falla se reintenta
  cada `throttle` hasta `discardAfter`.

## 8. Telemetría en vivo

- [doc] Flujos Server-Sent Events. `GET /api/live/tracing` (permiso `liveTracing`) y `GET /api/live/metrics`
  (`liveMetrics`), con cabecera `Authorization` o, para clientes sin cabeceras (`EventSource`), con un token de
  `GET /api/token/tracing` o `/api/token/metrics` en el parámetro `token` (válido 60 s y solo para su flujo).
- [doc] Eventos: sin parámetros, todos los que graba algún trazador, webhook o el historial. `filter=<texto>`
  deja los eventos con algún valor de texto, dirección o fecha que contenga el texto, o un número igual;
  `<clave>=<texto>` (nombres de clave exactos, p. ej. `remoteIp`) exige que esa clave lo contenga (número igual,
  lista con algún elemento); varias claves deben cumplirse todas; los parámetros que no son claves se ignoran.
  Cuando un evento coincide, se incluyen también los siguientes de la misma sesión. Empieza con
  `event: ping` / `data: {"interval": 30000}`, manda un ping cada 30 s sin tráfico y los eventos en lotes
  `event: trace` como mucho una vez por segundo, con el mismo formato que los eventos de `x:Trace`.
- [doc] Métricas: `metrics=a,b` (por defecto todas) e `interval=<s>` (mínimo 1, por defecto 30); la primera
  actualización sale enseguida; cada una es `event: metrics` con una lista de `{"metric", "@type", "count"[,
  "sum"]}` del nodo que atiende la petición.
- [bezenti] Cada flujo de eventos es un suscriptor temporal del colector (con pérdida: no frena el servidor) que se
  quita al cerrar la conexión; las recargas de configuración no lo cortan. **Usuarios de inquilino**: el flujo de
  eventos solo lleva sesiones de su ámbito (una sesión pasa a ser visible cuando algún evento nombra una dirección
  de un dominio o una cuenta de su ámbito; hasta 64 eventos anteriores de la sesión se envían entonces); los
  eventos sin sesión (del servidor) no se envían. El flujo de métricas es del servidor entero y les responde
  `forbidden`.

## 9. Identificadores de eventos y métricas nuevos

[bezenti] En `trc`, tras la feature `bezenti`: eventos `telemetry.alert-event` (531, `warn`),
`telemetry.alert-message` (536) y `telemetry.metrics-stored` (539); métricas (contadores)
`telemetry.alert-event` (143) y `telemetry.alert-message` (196). Son huecos libres de la numeración; si otra fase
usara los mismos, hay que cambiarlos aquí y allí.

## 10. Variables de entorno (pruebas y ajuste fino)

[bezenti] No forman parte de la API: `BEZENTI_METRICAS_INTERVALO` (segundos entre puntos de métricas, en vez de
`metricsCollectionInterval`), `BEZENTI_ALERTAS_INTERVALO` (segundos entre evaluaciones de alertas, en vez de
300) y `BEZENTI_TRAZAS_VOLCADO` (segundos entre volcados de trazas, en vez de 5).

## 11. Fuera de esta fase

- `FoundationDb` como almacén aparte solo funciona si el binario se compila con esa base.
- `search-index.pending-tracing` no existe: las trazas no pasan por el índice de búsqueda (§3).
- El estado de las alertas no se comparte entre nodos (cada nodo avisa con sus métricas, como dice la
  documentación); desde la fase 8 sí sobrevive a los reinicios de cada nodo (§7).
