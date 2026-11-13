# SCIM 2.0: aprovisionamiento de usuarios y grupos (fase 7)

Especificación de la edición bezenti. Escrita **solo** con fuentes permitidas. Cada punto dice de dónde sale:

- **[doc]**: documentación pública de Stalwart, sección «SCIM Provisioning»
  (<https://stalw.art/docs/auth/scim/>, con sus páginas `provisioning`, `configuration`, `endpoints`, `mapping` y
  `providers/{entra-id,okta,keycloak}`).
- **[esquema]**: `resources/schema/schema.json.gz` (permiso `scimAccess`, `x:Domain.allowScimProvisioning`,
  `externalId` de `x:UserAccount`/`x:GroupAccount`, eventos `scim.*`).
- **[RFC]**: RFC 7643 (esquema SCIM), RFC 7644 (protocolo SCIM), RFC 9865 (paginación con cursor).
- **[cliente]**: documentación pública de los clientes: «Develop and plan provisioning for a SCIM endpoint in
  Microsoft Entra ID» (Microsoft Learn) y «SCIM 2.0 — Build your SCIM API service» (Okta developer).
- **[bezenti]**: decisión nuestra, porque las fuentes no lo dicen o porque ampliamos.

El crate de la sala limpia `crates/scim` y `crates/scim-proto` eran 100 % SEL y quedaron vacíos: **no** se usan. Todo
el código de esta fase es nuevo, en el crate `crates/bezenti-scim` (AGPL-3.0-only), y solo entra con la feature
`bezenti`.

## 1. Qué es y qué no es

- [doc] Stalwart es el **proveedor de servicio** SCIM (recibe peticiones en `/scim/v2`); nunca es cliente. SCIM
  solo crea, cambia, suspende y borra cuentas: **no autentica**. Se combina con un directorio que verifica los
  inicios de sesión (OIDC, LDAP, SQL o el interno).
- [doc] Recursos: **User** → cuenta de usuario (`x:Account` de tipo `User`); **Group** → cuenta de grupo
  (`x:Account` de tipo `Group`) con sus miembros.
- [doc] Ninguna credencial cruza la interfaz: `changePassword` no se anuncia y `password` no se publica. Si llega
  (Okta la manda siempre al crear), se acepta y **se descarta**.
- [bezenti] En la edición oficial SCIM exige licencia; aquí no hay licencias: está siempre disponible si se
  compila con `bezenti`. Sin la feature, `/scim/v2` no existe (404 como cualquier ruta desconocida).

## 2. Endpoints

[doc] Todo bajo `/scim/v2`, con `Content-Type: application/scim+json` (también en los errores). Se acepta
`application/json` en las peticiones [RFC].

| endpoint | métodos | autenticación |
|---|---|---|
| `/Users` | GET, POST | portador |
| `/Users/{id}` | GET, PUT, PATCH, DELETE | portador |
| `/Users/.search` | POST | portador |
| `/Groups` | GET, POST | portador |
| `/Groups/{id}` | GET, PUT, PATCH, DELETE | portador |
| `/Groups/.search` | POST | portador |
| `/.search` | POST | portador (ambos tipos en una sola `ListResponse`) |
| `/Bulk` | POST | portador |
| `/ServiceProviderConfig` | GET | ninguna |
| `/ResourceTypes`, `/ResourceTypes/{id}` | GET | ninguna |
| `/Schemas`, `/Schemas/{urn}` | GET | ninguna |
| `/Me` | cualquiera | 501 |

- [doc] `OPTIONS` a cualquier ruta bajo la base → `204`. Ruta desconocida → `404`; método no admitido → `405` con
  cabecera `Allow`.
- [doc] Los de descubrimiento no exponen datos de cuentas y se leen sin autenticación. Un parámetro `filter` en
  ellos → `403` [RFC 7644 §4].
- [doc] `ServiceProviderConfig` (valores fijos): `patch` sí; `bulk` sí (`maxOperations` 1000,
  `maxPayloadSize` 1 MiB); `filter` sí (`maxResults` 200); `changePassword` no; `sort` sí; `etag` sí;
  paginación por índice y por cursor (página por defecto 100, máxima 200, por defecto índice);
  `authenticationSchemes`: solo «OAuth Bearer Token».
- [bezenti] `interopProfileConformant` no se publica en `ServiceProviderConfig` (es un atributo del perfil, no de
  RFC 7643); el comportamiento ante atributos no definidos es el de §5.
- [doc] Las URL absolutas (`meta.location`, `$ref`, cabecera `Location` del 201) se construyen con la URL pública
  del servidor (`STALWART_PUBLIC_URL` / `x:SystemSettings`), que en el código es `core.network.http.url_https`.

## 3. Autenticación y permisos

- [doc] Solo **portador**: una clave API (`x:ApiKey`) de la cuenta de aprovisionamiento (o cualquier token de
  portador que el servidor acepte: OAuth). **Basic → 401** con el detalle «HTTP Basic authentication is not
  supported, present a bearer token». Sin cabecera → 401 con `WWW-Authenticate: Bearer`.
- [doc] Permisos de la cuenta (efectivos, ya con roles, inquilino y modo de la clave API):

  | permiso | para |
  |---|---|
  | `authenticate` | presentar la credencial |
  | `scimAccess` | cualquier petición salvo descubrimiento |
  | `sysAccountGet` | GET de `/Users` y `/Groups`, toda consulta (también `/.search`) |
  | `sysAccountCreate` | POST |
  | `sysAccountUpdate` | PUT, PATCH y crear un grupo con miembros (la pertenencia se guarda en los usuarios) |
  | `sysAccountDestroy` | DELETE |

  [bezenti] Las consultas **no** piden `sysAccountQuery` (la doc dice `sysAccountGet` para «every query»).
- [doc] Regla de modificación de cuentas (la misma que en la API JMAP): quien crea, cambia o borra una cuenta debe
  tener **todos** sus permisos. Se cumple porque toda escritura pasa por el mismo `x:Account/set` del registro
  (`registry_set`), con todas sus validaciones (fase 2: ámbito, claves ajenas, cuotas; regla de permisos;
  direcciones únicas; caché; tarea de borrado). El 403 que resulta lleva el detalle del registro («You are not
  authorized to grant permissions: …»).
- [doc] Una clave API puede limitar permisos (modo `Disable` sin `sysAccountDestroy` = «suspender pero nunca
  borrar»), IP de origen (`allowedIps`) y caducidad (`expiresAt`).
- [doc] **Autoprotección**: desactivar o borrar la propia cuenta de aprovisionamiento → `403` «A SCIM client cannot
  deprovision or deactivate its own service principal».
- [bezenti] **Multi-inquilino**: `scimAccess` es un permiso más del techo del inquilino (fase 2, §3); como el rol
  por defecto «Tenant Administrator» no lo incluye, el operador decide qué inquilinos pueden usar SCIM añadiéndolo
  a `permissions` del inquilino (`Merge`). Ver la guía `guias/scim-entra-okta-keycloak.md`.
- [doc] Limitación de ritmo como cualquier petición HTTP autenticada: `429` con `Retry-After`.

## 4. Qué ve un token: ámbito

- [doc] SCIM ve y escribe solo cuentas de **dominios abiertos** (`x:Domain.allowScimProvisioning = true`). Una
  cuenta de otro dominio → `404` (como si no existiera).
- [doc] Un token de una cuenta de inquilino ve solo su inquilino; uno ajeno → `404`, no `403`, para no revelar que
  existe.
- [bezenti] El ámbito es exactamente el de la fase 2 (multi-inquilino §4.1): el inquilino del token y sus
  descendientes (revendedores). El **operador** (cuenta sin inquilino) ve todos los dominios abiertos.
- [bezenti] Cálculo: dominios visibles = dominios con `allowScimProvisioning` cuyo `memberTenantId` está en el
  ámbito (o todos, para el operador). Cuentas visibles = cuentas de esos dominios con `memberTenantId` dentro del
  ámbito. Se comprueba en cada lectura (también en `GET /Users/{id}`) y en cada escritura.
- [bezenti] Al crear un usuario, su inquilino es el del dominio de su `userName` (así el operador también crea
  cuentas de inquilino correctamente). Las cuotas del inquilino (`maxAccounts`, `maxGroups`, fase 2 §5) las
  aplica el registro: al pasarse, `403` con el detalle del registro.

## 5. Esquema y atributos

- [doc] Atributos que no se guardan pero pertenecen a un esquema conocido (núcleo User/Group de RFC 7643 y la
  extensión empresarial `urn:ietf:params:scim:schemas:extension:enterprise:2.0:User`) **se aceptan y se
  descartan** (POST, PUT y PATCH). Un atributo de ningún esquema conocido, una URI de esquema desconocida, un
  atributo repetido o falta `schemas` → `400 invalidSyntax`. Por eso Entra, Okta y las extensiones de Keycloak
  funcionan con sus mapeos por defecto, y un error de tecleo (`dispalyName`) no se pierde en silencio.
- [doc] `GET /Schemas` publica solo lo que se guarda (§5.1, §5.2).
- [RFC] Nombres de atributo sin distinguir mayúsculas; un atributo puede llevar el prefijo de la URI de su esquema
  (`urn:ietf:params:scim:schemas:core:2.0:User:userName`).

### 5.1 User (`urn:ietf:params:scim:schemas:core:2.0:User`)

| SCIM | cuenta | notas |
|---|---|---|
| `id` | id de la cuenta (el mismo de la API JMAP) | solo lectura |
| `externalId` | `externalId` | [doc] indexado, filtrable; [bezenti] único dentro del ámbito por tipo de recurso → `409 uniqueness` |
| `userName` | `name` + `domainId` | [doc] obligatorio, dirección completa; dominio existente y abierto a SCIM (si no, `400 invalidValue`; [bezenti] el mismo mensaje si no existe o no es visible, para no revelar dominios de otros inquilinos) |
| `displayName`, `name.formatted` | `description` | [doc] `displayName` gana; sin ninguno, `name.givenName` + « » + `name.familyName` |
| `active` | permiso `authenticate` | §5.3 |
| `emails` | dirección principal y alias | §5.4 |
| `locale`, `preferredLanguage` | `locale` | [doc] mismo campo; `locale` gana; se normaliza (`EN-us` → `en-US`, `_` → `-`); desconocido → `400 invalidValue`. Siempre tiene valor: quitarlo lo devuelve a `en-US`, que se sigue mostrando |
| `timezone` | `timeZone` | [doc] IANA; desconocido → `400 invalidValue` |
| `groups` | `memberGroupIds` | [doc] solo lectura (se gestiona desde Group) |
| `meta` | `createdAt`, versión | `resourceType`, `created`, `location`, `version`; sin `lastModified` (no hay fecha de cambio) |

### 5.2 Group (`urn:ietf:params:scim:schemas:core:2.0:Group`)

| SCIM | cuenta | notas |
|---|---|---|
| `id` | id de la cuenta | solo lectura |
| `externalId` | `externalId` | como en User |
| `displayName` | `description` | [doc] obligatorio, único entre los grupos del inquilino (sin distinguir mayúsculas) → `409 uniqueness` |
| `members` | `memberGroupIds` de cada usuario | [doc] solo usuarios (no hay grupos anidados): un miembro que es grupo → `400 invalidValue`; `display` de solo lectura |
| `meta` | | la versión incluye los miembros |

- [doc] **Dirección del grupo**: se deriva de `displayName` en minúsculas, cada racha de caracteres que no son
  letra ni dígito → `-` («Sales EMEA» → `sales-emea`), máximo 64 caracteres; si está ocupada se añade `-2`, `-3`…
  El dominio es el de la cuenta de aprovisionamiento (que debe estar abierto a SCIM). La dirección no cambia al
  renombrar el grupo. Sin dirección libre → `409`.
- [bezenti] Si el nombre no deja ningún carácter válido (p. ej. solo símbolos), la parte local es `group`.
- [bezenti] El dominio del operador: si la cuenta de aprovisionamiento no está en un dominio abierto, crear
  grupos → `400 invalidValue` («the provisioning account's domain is not open to SCIM»).
- [doc] Leer un grupo con muchos miembros se rechaza con `400 tooMany`; [bezenti] límite 1000 (la doc oficial usa
  200); con `excludedAttributes=members` siempre se puede.

### 5.3 `active`

- [doc] Leer: ¿incluyen los permisos **efectivos** de la cuenta `authenticate`? (roles + campo `permissions` +
  techo del inquilino).
- [doc] Escribir `false`: añade `authenticate` a `disabledPermissions` del campo `permissions` de la cuenta (gana a
  los roles). `true`: quita esa entrada y deja todo lo demás; una cuenta sin otros cambios vuelve exactamente a su
  estado anterior (`Inherit`).
- [bezenti] Detalle: si la cuenta estaba en `Inherit`, se pasa a `Merge` con solo ese permiso desactivado y, al
  reactivar, si el `Merge` queda vacío, vuelve a `Inherit`. En `Replace` solo se toca `disabledPermissions` (lo
  desactivado gana), así reactivar deja `enabledPermissions` como estaba.
- [doc] La cuenta suspendida conserva el buzón y **sigue recibiendo correo**; ninguna credencial se acepta.
- [doc][cliente] Entra manda `"active": "False"` (cadena); se acepta `"true"`/`"false"` sin distinguir
  mayúsculas además del booleano.

### 5.4 `emails`

- [doc] La principal sale de `userName`: `{"value": userName, "type": "work", "primary": true}`. Los alias son
  `{"value": …, "type": "work"}`. Quitar la principal, cambiarle el tipo o quitarle `primary` → `400` que remite a
  `userName`; cambiar su `value` es renombrar la cuenta (como cambiar `userName`).
- [doc] Toda otra entrada es un alias. PUT reemplaza el conjunto; PATCH añade/quita uno a uno. Duplicados y la
  repetición de la principal se ignoran. Cada alias debe estar en un dominio abierto a SCIM y del mismo inquilino
  que la cuenta (si no, `400 invalidValue`).
- [doc] Como todo es `work`, `emails[type eq "work"].value` casa con varias entradas en una cuenta con alias: un
  PATCH por ese camino → `400` «The value filter matched more than one 'emails' element». (Entra trae ese mapeo por
  defecto: la guía dice quitarlo.)

### 5.5 Cuentas nuevas y lo que queda local

- [doc] Usuarios nuevos: `roles: User`, `permissions: Inherit`, sin contraseña. Grupos nuevos: roles por defecto.
  Cuotas, roles, otros permisos, buzones, Sieve, cifrado… son locales: SCIM no los toca.
- [doc] **PUT** reemplaza: lo que falta vuelve al valor por defecto (sin alias, `locale` `en-US`, sin zona
  horaria, `active` true). **PATCH** solo cambia lo que dice. [doc] Los atributos de solo lectura en un PUT
  (`id`, `meta`, `groups`) se ignoran [RFC 7644 §3.5.1].
- [doc] DELETE de un usuario: borra la cuenta, sus concesiones OAuth y lo que compartía, y programa el borrado de
  sus datos (si `archiveDeletedAccountsFor` está activo, fase 3, se puede recuperar). Es la misma ruta que
  `x:Account/set destroy`.

## 6. Filtros

- [doc] La edición oficial solo acepta `eq` y `and` (lo que exige el perfil de interoperabilidad y mandan los
  clientes) sobre una lista cerrada de atributos.
- [bezenti] **Ampliamos** a la gramática completa de RFC 7644 §3.4.2.2: `eq`, `ne`, `co`, `sw`, `ew`, `pr`,
  `gt`, `ge`, `lt`, `le`, `and`, `or`, `not`, paréntesis y filtros de valor (`emails[type eq "work" and value
  co "@x.com"]`), operadores sin distinguir mayúsculas. Cadenas con `caseExact` falso (todas salvo `id` y
  `externalId`) se comparan sin distinguir mayúsculas [RFC 7643 §2.2]. Fechas (`meta.created`) se comparan como
  instantes.
- [bezenti] Atributos filtrables de User: `id`, `externalId`, `userName`, `displayName`, `name.formatted`,
  `name.givenName`/`name.familyName` (siempre ausentes), `emails`, `emails.value`, `emails.type`,
  `emails.primary`, `active`, `locale`, `preferredLanguage`, `timezone`, `groups`, `groups.value`,
  `meta.created`, `meta.resourceType`. De Group: `id`, `externalId`, `displayName`, `members`, `members.value`,
  `meta.created`. Cualquier otro (también `meta.lastModified`, que no existe) → `400 invalidFilter`. Sintaxis
  incorrecta → `400 invalidFilter` con la posición.
- [bezenti] **Plan de ejecución**: de los términos `eq` de la conjunción de primer nivel se saca un conjunto de
  candidatos por índice (`id`; `userName`/`emails`/`emails.value` por la tabla de direcciones; `externalId` por su
  índice; `groups.value`/`members.value` por `memberGroupIds`; `displayName` de grupos se recorre). El resto del
  filtro se evalúa en memoria sobre los candidatos. Si no hay término indexado, los candidatos son todas las cuentas
  visibles de ese tipo; si pasan de **5000** se responde `400 tooMany` pidiendo un filtro más estrecho (la oficial
  usa 200 y solo para `active`/`displayName`). Así `filter=active eq false` funciona en inquilinos medianos.
- [doc] Los filtros `[...]` también se aceptan en rutas de PATCH (§8).

## 7. Orden y paginación

- [doc] `sortBy` + `sortOrder` (`ascending` por defecto, `descending`). [bezenti] Se admite cualquier atributo
  simple filtrable de §6 (la oficial: `id` y `userName`); otro → `400 invalidValue`. Sin `sortBy`, el orden es
  el del id.
- [doc] **Índice** (RFC 7644): `startIndex` (desde 1; menor que 1 se toma como 1) y `count` (por defecto 100,
  máximo 200; `count=0` solo devuelve `totalResults`). Respuesta `ListResponse` con `totalResults`,
  `startIndex`, `itemsPerPage`, `Resources`.
- [doc] **Cursor** (RFC 9865): `cursor` (vacío = primera página) y la respuesta lleva `nextCursor` mientras
  queden resultados (y `previousCursor` cuando no es la primera página). [bezenti] El cursor es opaco: posición +
  huella (xxh3) de filtro, orden y tipo de recurso; presentarlo con otra consulta → `400 invalidCursor` «The supplied
  cursor was issued for a different query.». Usar `cursor` y `startIndex` a la vez → `400 invalidValue`.
- [doc] `attributes` y `excludedAttributes` (también en GET de un recurso y en `/.search`): `id`, `schemas` y
  `meta.resourceType` se devuelven siempre [RFC 7643 §7 «returned: always»].
- [doc] `POST …/.search` con `SearchRequest` (`schemas` = `urn:ietf:params:scim:api:messages:2.0:SearchRequest`)
  acepta los mismos parámetros. `/.search` en la raíz junta Users y Groups.

## 8. PATCH

[RFC 7644 §3.5.2] `PatchOp` (`urn:ietf:params:scim:api:messages:2.0:PatchOp`) con `Operations`. Cada operación:
`op` (`add` | `replace` | `remove`, [cliente] sin distinguir mayúsculas: Entra manda `Replace`, `Add`,
`Remove`), `path` opcional y `value`.

- [bezenti] **Cómo se aplica**: se toma la representación SCIM actual del recurso, se le aplican las operaciones
  en orden como un documento JSON (con la semántica del RFC) y el resultado se guarda como un PUT, recordando qué
  atributos tocó el PATCH (para decidir entre `displayName` y `name.formatted`, §5.1). Así cualquier forma de ruta
  se trata igual en usuarios y grupos.
- Rutas: `atributo`, `atributo.subatributo`, `atributo[filtro]`, `atributo[filtro].subatributo`, con o sin
  prefijo de URI de esquema. Ruta mal formada o atributo desconocido → `400 invalidPath`.
- `add` sin `path`: `value` es un objeto cuyos atributos se añaden; [cliente] Okta lo usa con `replace` sin
  `path` (`{"op":"replace","value":{"active":false}}`). Atributos multivalor: `add` añade los elementos que no
  estén; `replace` sustituye la lista entera (o el elemento filtrado); `remove` con `value` (forma de Entra para
  quitar miembros: `{"op":"Remove","path":"members","value":[{"value":"id"}]}`) quita los elementos que casen; sin
  `value`, quita el atributo entero o los elementos que casen con el filtro.
- `remove` con filtro que no casa con nada → `400 noTarget` [RFC]. [cliente] Okta repite `id` dentro del objeto de
  un `replace` sin `path` (`{"id": …, "displayName": …}`): un atributo de solo lectura con su valor actual se
  ignora. Filtro que casa con varios cuando
  se pide un subatributo de uno solo (caso `emails[type eq "work"].value`) → `400` con el detalle del §5.4.
  [bezenti] `add` (y `replace` con subatributo) con un filtro que no casa con nada crea el elemento con los
  valores de los términos `eq` del filtro [cliente: Entra espera que `emails[type eq "work"].value` funcione
  aunque el elemento no exista].
- Atributos de solo lectura (`id`, `meta`, `groups`) y quitar uno obligatorio (`userName`, `displayName` de un
  grupo) → `400 mutability`. Atributos conocidos pero no guardados
  (p. ej. `title`, `phoneNumbers`, la extensión empresarial, `password`) → la operación se salta y se registra
  `scim.attribute-ignored`.
- [cliente] Entra manda `"active": "False"`; Okta manda `"value": {"active": false}`.
- Respuesta: `200` con el recurso. [RFC] Un PATCH que no cambia nada no escribe (ni cambia la versión).

## 9. ETag y versión

- [doc] `meta.version` y cabecera `ETag` = `W/"<huella>"`, derivada del contenido (para grupos incluye los
  miembros). [bezenti] Huella = xxh3 de la representación serializada (sin `meta.version`); en los grupos entra la
  lista de **ids** de los miembros (no sus nombres), así la versión es la misma con o sin `members` en la salida y
  se calcula sin leer cada miembro.
- [doc] `If-None-Match` en GET → `304` si no cambió. `If-Match` en PUT/PATCH/DELETE → `412` si cambió. Sin
  cabeceras, sin condición. [bezenti] `If-Match: *` siempre se cumple si el recurso existe.

## 10. Bulk

- [doc] `POST /Bulk` (`urn:ietf:params:scim:api:messages:2.0:BulkRequest`), hasta 1000 operaciones y 1 MiB.
  `failOnErrors` (parar tras N errores: evento `scim.bulk-aborted`), `bulkId` y referencias `bulkId:<id>` en
  `path` y en cualquier valor de `data`. Cada operación tiene su `status` y un fallo no deshace las anteriores.
- [bezenti] Las operaciones se ejecutan en orden; una referencia a un `bulkId` que aún no existe → esa operación
  falla con `409` [RFC 7644 §3.7.3: no se resuelven ciclos]. `method` POST/PUT/PATCH/DELETE; `version` hace de
  `If-Match`. La respuesta (`BulkResponse`) lleva `location`, `status` (cadena con el código) y `response` (el
  error SCIM) por operación.

## 11. Errores

[doc][RFC] Documento `urn:ietf:params:scim:api:messages:2.0:Error` con `status` (cadena), `scimType` cuando lo hay
y `detail`.

| estado | `scimType` | causa |
|---|---|---|
| 400 | `invalidSyntax` | JSON mal formado, atributo/esquema desconocido, atributo repetido, falta `schemas` |
| 400 | `invalidFilter` | filtro mal formado o atributo no filtrable |
| 400 | `invalidPath` | ruta de PATCH mal formada o no admitida |
| 400 | `invalidValue` | valor inaceptable (dominio no abierto, locale/zona desconocidos, miembro que es grupo…) |
| 400 | `tooMany` | demasiados candidatos sin índice; grupo con demasiados miembros |
| 400 | `mutability` | cambiar un atributo de solo lectura |
| 400 | `noTarget` | ruta de PATCH sin elemento |
| 400 | `invalidCursor` | cursor de otra consulta |
| 401 | — | sin credencial, credencial inválida o caducada, Basic |
| 403 | — | falta un permiso, regla de modificación de cuentas, cuota del inquilino, autoprotección, `filter` en descubrimiento |
| 404 | — | recurso desconocido, fuera del ámbito del inquilino o de un dominio no abierto; ruta desconocida |
| 405 | — | método no admitido (`Allow`) |
| 409 | `uniqueness` | `userName`/alias/`externalId`/nombre de grupo ya en uso |
| 409 | — | modificación concurrente, objeto enlazado, sin dirección libre para un grupo |
| 412 | — | `If-Match` que ya no se cumple |
| 413 | — | cuerpo por encima del límite (1 MiB) |
| 429 | — | límite de ritmo (`Retry-After`) |
| 501 | — | `/Me` |

[bezenti] Correspondencia con los errores del registro: `forbidden` y `overQuota` → 403; `primaryKeyViolation` →
409 `uniqueness`; `invalidForeignKey`, `invalidProperties`, `validationFailed` → 400 `invalidValue`;
`objectIsLinked` → 409; `notFound` → 404.

## 12. Autoridad frente al aprovisionamiento «justo a tiempo»

- [doc] Con `allowScimProvisioning` activado en un dominio, SCIM manda: la sincronización al iniciar sesión con un
  directorio externo (OIDC/LDAP/SQL) **solo lee** (resuelve la dirección, comprueba que la cuenta existe y no
  escribe nada) y **no crea** cuentas: quien entra antes de ser aprovisionado recibe un fallo de autenticación.
- [doc] Activarlo en un dominio con cuentas no las migra: siguen ahí y el IdP las adopta al casar por `userName`.
- [bezenti] Implementado con una marca en la caché de dominios (`DOMAIN_FLAG_SCIM`) y un gancho al principio de
  `Server::sync_account` (`crates/common/src/cache/directory.rs`).

## 13. Eventos

[esquema] Nombres, etiquetas y niveles de la doc. [bezenti] Identificadores numéricos en huecos libres (615–624,
349 y 350), en `crates/trc/src/event/scim.rs`:

| evento | id | nivel |
|---|---|---|
| `scim.request` | 615 | debug (método, ruta, estado, tiempo) |
| `scim.forbidden` | 616 | info |
| `scim.conflict` | 617 | info |
| `scim.error` | 618 | debug |
| `scim.resource-created` | 619 | info |
| `scim.resource-updated` | 620 | info |
| `scim.resource-deleted` | 621 | info |
| `scim.query` | 622 | debug |
| `scim.attribute-ignored` | 623 | debug |
| `scim.bulk-processed` | 624 | debug |
| `scim.bulk-aborted` | 349 | warn |
| `scim.rollback-failed` | 350 | error |

[bezenti] `scim.rollback-failed` se emite si al crear un grupo con miembros falla la asignación de miembros y
tampoco se puede borrar el grupo recién creado.

## 14. Compatibilidad con los clientes

| | Entra ID | Okta | Keycloak (extensiones) |
|---|---|---|---|
| Prueba de conexión | `GET /Users?filter=userName eq "<aleatorio>"` → 200 y lista vacía [cliente] | `GET /Users?count=1` y `filter=userName eq` | — |
| Búsqueda | `userName eq`, `externalId eq`, `displayName eq` en grupos | siempre `userName eq` | `externalId eq` o ninguna |
| Cambios | PATCH (`Replace`, `"False"`, `members` con lista de valores) | PUT completo (asistente) o PATCH sin `path` | según extensión |
| Suspensión | `active` false | `active` false | según extensión |
| Borrado | DELETE tras su retención | nunca (solo grupos) | según extensión |
| Grupos | POST con `members`, PATCH add/remove `members` | Push Groups: POST, PATCH `members`, DELETE | — |

## 15. Lo implementado

| pieza | dónde |
|---|---|
| Analizador y evaluador de filtros y rutas de PATCH (pruebas unitarias) | `crates/bezenti-scim/src/filtro.rs` |
| Errores SCIM y su correspondencia con el registro | `crates/bezenti-scim/src/error.rs` |
| Descubrimiento (`ServiceProviderConfig`, `ResourceTypes`, `Schemas`) | `crates/bezenti-scim/src/esquema.rs` |
| Ámbito (dominios abiertos e inquilino), lectura de cuentas y escritura por `registry_set` | `crates/bezenti-scim/src/contexto.rs` |
| User y Group ↔ cuenta (lectura, validación, PUT, PATCH, `active`, alias, miembros) | `crates/bezenti-scim/src/recursos.rs`, `patch.rs` |
| Listas, `/.search`, orden, paginación por índice y cursor | `crates/bezenti-scim/src/consulta.rs` |
| Bulk | `crates/bezenti-scim/src/bulk.rs` |
| Enrutado y autenticación (portador, Basic → 401) | `crates/bezenti-scim/src/lib.rs`, gancho en `crates/http/src/request.rs` (`"scim"`) |
| Eventos `scim.*` | `crates/trc/src/event/scim.rs` |
| Autoridad de SCIM sobre la sincronización justo a tiempo | `crates/common/src/cache/{principals,directory}.rs` |

### Pruebas

- `bezenti/pruebas/scim.py`: servidor real con SQLite, por HTTP (`/scim/v2`), JMAP, SMTP e IMAP (127
  comprobaciones): descubrimiento; 401 sin credencial, con Basic y con token falso; 403 sin `scimAccess`; prueba de
  conexión de Entra; altas estilo Entra (extensión empresarial, `title`, teléfonos), Okta (con `password`, que se
  descarta) y Keycloak (solo partes del nombre); errores 400 (`invalidSyntax`, `invalidValue`, `invalidFilter`,
  `invalidPath`, `mutability`, `invalidCursor`), 404, 405, 409 y 412; filtros (`eq co sw pr gt`, `and or not`,
  filtros de valor, mayúsculas); ETag e `If-None-Match` → 304; PATCH de Entra (`Replace`, `"False"`, extensión) y de
  Okta (sin `path`, con `id`); PUT; un usuario creado por SCIM recibe por SMTP (principal y alias) y lee por IMAP;
  suspendido no entra pero sigue recibiendo, reactivado vuelve a `Inherit`; autoprotección; grupos (dirección
  derivada y con sufijo, miembros, anidados no, PATCH de Entra y de Okta, PUT, DELETE); Bulk con `bulkId` y
  `failOnErrors`; paginación por índice y por cursor, orden y `count=0`; aislamiento entre inquilinos con el token de
  cada uno (todo lo ajeno → 404, ni alias ni miembros ajenos, `externalId` único por ámbito), cuota `maxAccounts` y
  el token del operador.
- `bezenti/pruebas/scim_conformidad.py`: la herramienta pública **scim2-tester** (python-scim, Apache-2.0) contra el
  servidor real: 143 comprobaciones, 138 correctas. Las 5 que marca como error son decisiones documentadas: la
  dirección principal no se quita por `emails` y siempre está en la lista (§5.4), y `active`, `locale` y
  `preferredLanguage` siempre tienen valor (quitarlos los devuelve a su valor por defecto). Única adaptación: los
  `userName`/`emails` aleatorios se generan en un dominio abierto a SCIM (lo mismo que hay que mapear en un IdP).
- Unitarias: `cargo test -p bezenti-scim` (filtros, rutas, PATCH, esquema, parámetros, Bulk) y `cargo test -p trc
  --features bezenti --lib scim::` (identificadores de eventos).
- No cubierto por integración: la autoridad sobre el aprovisionamiento justo a tiempo (§12) necesita un directorio
  OIDC/LDAP real; y Entra/Okta reales necesitan el servidor publicado en Internet.
