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
| 2 | **Multi-inquilino**: inquilinos (`x:Tenant`), dominios, cuentas, grupos, listas y roles por inquilino; administradores limitados a su inquilino (aislamiento en toda la API, directorio, compartir y OAuth); techo de permisos; cuotas de objetos y de disco por inquilino; logo por dominio/inquilino (`/logo`); extensión propia: **revendedores** (inquilinos con subinquilinos, `bezentiParentTenantId`) — [`especificaciones/multi-inquilino.md`](especificaciones/multi-inquilino.md), guía [`guias/alta-de-empresa.md`](guias/alta-de-empresa.md), pruebas `pruebas/multi_inquilino.py`. Pendiente: cola SMTP e informes externos por inquilino | hecho |
| 3 | **Direcciones enmascaradas** (`x:MaskedEmail`: alta con prefijo/dominio/caducidad, límite `maxMaskedAddresses`, entrega por SMTP reescribiendo el `RCPT TO`, rechazo si está desactivada/caducada/borrada) y **archivo de borrados** (`x:ArchivedItem`: los mensajes borrados por JMAP/IMAP/POP3/vaciado automático se guardan `archiveDeletedItemsFor`; restaurar íntegro con buzones y etiquetas; purga al vencer con la limpieza de blobs; vista del administrador de inquilino; recuperar cuentas borradas con `archiveDeletedAccountsFor`) — [`especificaciones/enmascaradas.md`](especificaciones/enmascaradas.md), [`especificaciones/archivo.md`](especificaciones/archivo.md), pruebas `pruebas/archivo_enmascaradas.py`. Pendiente: archivar archivos, eventos, contactos y Sieve; API de Fastmail para enmascaradas | hecho |
| 4 | **IA**: modelos configurables (`x:AiModel`: OpenAI y compatibles —Azure, vLLM, llama.cpp—, Anthropic, Gemini, Ollama, TypeSafe) con tiempo de espera, un reintento, concurrencia, ritmo y cortacircuitos; **antispam con IA** (`x:SpamAi`: compuerta para los dudosos, hechos verificados, niveles con escalado, etiquetas `AI_*`, modo sombra, cabecera `X-Spam-AI`, aprendizaje, `ClassifySpam` con `aiFacts`/`aiGate`/`aiVerdict`); **`llm_prompt` en Sieve** (permiso `interactAi`); eventos, métricas y claves de traza `ai.*` — [`especificaciones/ia.md`](especificaciones/ia.md), guía [`guias/antispam-con-ia.md`](guias/antispam-con-ia.md), pruebas `pruebas/ia.py` (proveedores falsos) y `pruebas/ia_real_anthropic.py` (manual). Pendiente: `webSearch` | hecho |
| 5 | **Telemetría con historial y alertas**: historial de entregas (`x:Trace`, vistas de entrada y salida, filtros por texto, fecha, cola y tipo) y de métricas (`x:Metric`, un registro por nodo e intervalo con diferencias), en el almacén de datos o uno aparte (`x:TracingStore`, `x:MetricsStore`), escrito por lotes fuera del camino del correo; retención y purga (`holdTracesFor`, `holdMetricsFor`); alertas (`x:Alert`: `metric(...)` → correo por el propio servidor y/o evento para webhooks con reintentos, con histéresis: un aviso al dispararse y otro al resolverse); telemetría en vivo por SSE (`/api/live/tracing` con filtros y seguimiento de sesión, `/api/live/metrics`, tokens de 60 s); aislamiento: cada inquilino ve sus trazas, sus contadores y sus sesiones en vivo — [`especificaciones/telemetria.md`](especificaciones/telemetria.md), pruebas `pruebas/telemetria.py`. Pendiente: estado de alertas compartido entre nodos | hecho |
| 6 | **Réplicas de lectura y almacenes repartidos**: `readReplicas` en PostgreSQL y MySQL/MariaDB (lecturas por turnos entre réplicas sanas; escrituras, búsqueda y `sql_query` al primario; regla «lee lo que escribiste» por partición con posiciones de replicación —LSN, GTID— verificadas cada 200 ms; réplica caída o atrasada más de 5 s → primario, y vuelve sola), `urlFailover` en LDAP, almacén de blobs repartido (`BlobStore.Sharded`: rendezvous hashing estable, añadir miembros al final sin perder nada gracias a la lectura de respaldo) y en memoria/lookup repartido (`Sharded` sobre Redis, también para instantáneas de caché y el coordinador) — [`especificaciones/replicas-y-reparto.md`](especificaciones/replicas-y-reparto.md), pruebas unitarias en `store` (`backend::bezenti`), integración con PostgreSQL primario y réplica reales (`pruebas/postgres_replica.sh`) y servidor completo `pruebas/replicas_reparto.py`. Pendiente: probar MySQL/MariaDB y Redis repartido con servidores reales; registro de escrituras compartido entre nodos | hecho |
| 7 | SCIM 2.0 (aprovisionamiento desde Entra ID, Okta…) | pendiente |
