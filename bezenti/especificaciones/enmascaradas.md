# Direcciones enmascaradas (fase 3)

Especificación de la edición bezenti. Escrita **solo** con fuentes permitidas: la documentación pública de
Stalwart (citada con su URL), el esquema `resources/schema/schema.json.gz`, el código AGPL de la rama `bezenti`
y estándares públicos. Cada punto dice de dónde sale:

- **[doc]**: lo dice la documentación pública (con la página).
- **[esquema]**: lo define el esquema de la API.
- **[bezenti]**: decisión nuestra, porque la documentación no lo dice o porque es una extensión propia.

Páginas citadas: «Masked email» (<https://stalw.art/docs/email/management/masked-email/>), referencia del objeto
(<https://stalw.art/docs/ref/object/masked-email/>), «RCPT stage» (<https://stalw.art/docs/mta/inbound/rcpt/>),
«Quotas» (<https://stalw.art/docs/auth/authorization/quotas/>) y «Manage accounts»
(<https://stalw.art/docs/auth/accounts/manage/>).

## 1. Qué es

- [doc] Una dirección enmascarada es una dirección **solo de recepción** que entrega en una cuenta existente sin
  revelar su dirección real. Cada sitio web recibe una distinta; si una empieza a recibir basura o se filtra, se
  desactiva solo esa. Un gestor de contraseñas puede crearla por JMAP al registrar un acceso nuevo.
- [doc] No sirve como identidad de envío ni como remitente del sobre SMTP: las respuestas del usuario salen con
  su dirección propia. [bezenti] No hace falta código: las identidades y el `MAIL FROM` autenticado solo admiten
  las direcciones de la cuenta, y la enmascarada no es una de ellas.
- [esquema] Objeto `x:MaskedEmail` (permisos `sysMaskedEmail{Get,Create,Update,Destroy,Query}`), objeto de cuenta
  (`OBJ_FILTER_ACCOUNT`; la lista del panel filtra por `accountId`):

| campo | tipo | quién lo pone |
|---|---|---|
| `accountId` | id→`x:Account` | servidor (la cuenta de la petición; el operador puede elegir otra) |
| `email` | cadena | servidor |
| `description`, `forDomain`, `url` | cadena nulable | cliente |
| `enable` | booleano, por defecto `true` | cliente |
| `emailPrefix`, `emailDomain`, `expiresAt` | nulables, **solo al crear** | cliente |
| `createdAt` | fecha | servidor |
| `createdBy` | cadena nulable | servidor (a partir de las credenciales) |

## 2. Formato de la dirección

- [doc] `<prefijo>.<código>@<dominio>`:
  - prefijo: 16 letras minúsculas y cifras aleatorias, o el `emailPrefix` dado: 1 a 64 caracteres ASCII letras,
    cifras o `_`, sin `_` inicial, guardado en minúsculas;
  - código: lo genera el servidor; identifica la dirección y codifica su caducidad, por eso `expiresAt` no se puede
    cambiar después;
  - dominio: `emailDomain`, que debe ser un dominio en el que la cuenta tenga dirección; si no se da, el del
    nombre de la cuenta.
- [bezenti] El código son **10 caracteres** del alfabeto `abcdefghijkmnpqrstuvwxyz23456789` (32 símbolos sin
  `0/o` ni `1/l`, ~50 bits de azar). No codificamos la caducidad dentro del código: la guardamos en el objeto
  (`expiresAt`, inmutable), que es donde la API la muestra; el efecto para el usuario es el mismo (no se puede
  cambiar). Como el prefijo no admite puntos, una enmascarada tiene exactamente un punto y el servidor la reconoce
  por su forma sin consultar nada; solo las direcciones con esa forma cuestan una lectura (por la clave única de
  `email`).
- [bezenti] La dirección es única en todo el servidor (índice único del registro). Al generarla se descartan las
  que ya existen como enmascarada o como dirección real (cuenta, alias, lista, grupo); se reintenta hasta 8 veces.

## 3. Alta (`x:MaskedEmail/set` create)

- [doc] Errores al crear:

| error | descripción | causa |
|---|---|---|
| `forbidden` en `emailDomain` | `The specified domain is not valid for this account.` | el dominio no existe o la cuenta no tiene dirección en él |
| `forbidden` en `emailDomain` | `No valid domain is available for this account.` | sin `emailDomain` y la cuenta no tiene dominio |
| `invalidProperties` en `emailPrefix` | | el prefijo no cumple las reglas |
| `overQuota` | `You have exceeded your quota of <n> masked addresses.` | la cuenta llegó a su límite |

- [bezenti] `expiresAt` en el pasado → `invalidProperties`.
- [doc] El servidor pone `accountId`, `createdAt` y `createdBy` (a partir de las credenciales). [bezenti]
  `createdBy` = nombre del cliente OAuth (o su `clientId`) si la petición viene con un token OAuth; la descripción
  de la contraseña de aplicación o clave API si viene con una; nada con la contraseña principal. Un cliente no lo
  puede cambiar después (se conserva el valor original aunque el esquema lo marque como modificable).
- [doc] `email` lo pone el servidor y no se puede modificar.

## 4. Límites

- [doc] El número de enmascaradas de una cuenta lo fija `x:Email.maxMaskedAddresses` (por defecto **5**); una
  entrada `maxMaskedAddresses` en las `quotas` de la cuenta lo sustituye para esa cuenta.
- [doc] Desactivar no libera plaza: las desactivadas cuentan hasta que se borran.
- [esquema] La clave es `StorageQuota.maxMaskedAddresses` (cuotas de **cuenta**). `TenantStorageQuota` no tiene
  clave de enmascaradas: **no hay límite por inquilino**. [bezenti] Las enmascaradas no ocupan disco (son un objeto
  pequeño del registro), así que no tocan la cuota de disco de la cuenta ni la del inquilino.

## 5. Recepción

- [doc] El servidor SMTP la reconoce en `RCPT TO`, **antes** de cualquier directorio externo, después de quitar la
  subdirección (`+etiqueta`). Se entrega a la cuenta dueña si: existe y `enable` es `true`, no ha caducado y la
  cuenta sigue teniendo una dirección en el dominio del destinatario. Si no, se trata como destinatario
  desconocido y se rechaza como cualquier dirección que no existe (`550 5.1.1 Mailbox does not exist.`).
- [doc] El mensaje llega «as addressed»: la enmascarada sigue en la cabecera `To`, para que el usuario vea a qué
  dirección le escribieron y qué sitio la filtró.
- [bezenti] En `RCPT TO` el destinatario del sobre se **reescribe** a la dirección de la cuenta en ese dominio
  (el mecanismo libre que ya usan la subdirección y los comodines; el `ORCPT` de DSN conserva la enmascarada). Las
  cabeceras no se tocan. La resolución de direcciones que usan otros módulos (iTIP, identidades, el clasificador
  antispam) también reconoce las enmascaradas vigentes.
- [bezenti] Una dirección con forma de enmascarada que no existe sigue la resolución normal (puede ser una cuenta
  real con ese nombre). Una que existe pero no vale se rechaza sin seguir: así una desactivada nunca cae en el
  comodín del dominio.

## 6. Desactivar, borrar, caducar

- [doc] `enable: false` hace la dirección desconocida al momento (se rechaza el correo) y conserva el objeto; con
  `true` vuelve a recibir. Borrar el objeto la elimina para siempre. Una caducada se rechaza igual que una
  desactivada.
- [doc] Las enmascaradas no sobreviven a su cuenta: al borrar la cuenta se destruyen con ella. [bezenti] Las borra
  la tarea `DestroyAccount` (por el registro, para que se libere también su clave única); mientras la tarea está
  aplazada (`archiveDeletedAccountsFor`, ver `archivo.md`) siguen existiendo pero se rechazan (la cuenta no
  existe), y si la cuenta se recupera vuelven a funcionar.

## 7. Permisos y aislamiento

- [doc] Cada operación pide su permiso (`sysMaskedEmailGet`, `…Query`, `…Create`, `…Update`, `…Destroy`). El rol
  User los tiene todos (cada usuario gestiona las suyas); el System Administrator también; los administradores de
  inquilino, a través del rol User. [bezenti] Ya estaba así en el código libre (`DefaultPermissions`).
- [doc] `x:MaskedEmail/query` con el filtro `accountId` lista las de una cuenta. [bezenti] El filtro solo lo
  respeta quien tiene `impersonate` (el operador); para los demás se ignora y ven solo las suyas. A los miembros de
  un inquilino se les quita siempre `impersonate` (fase 2), así que **un administrador de inquilino solo gestiona
  sus propias enmascaradas**, no las de sus usuarios (son datos privados del usuario, como sus contraseñas de
  aplicación). Para un usuario, las de otra cuenta no existen: `get`, `set` y `query` responden `notFound` o vacío.
- [bezenti] El dominio de la dirección siempre es uno en el que la cuenta tiene dirección; como las direcciones de
  una cuenta solo pueden estar en dominios de su inquilino (fase 2), una enmascarada nunca cruza inquilinos.

## 8. ¿Compatibilidad con la extensión JMAP de Fastmail?

- Fastmail publica la extensión `https://www.fastmail.com/dev/maskedemail` (<https://www.fastmail.com/dev/>):
  objeto `MaskedEmail` con `id`, `email`, `state` (`pending` | `enabled` | `disabled` | `deleted`), `forDomain`,
  `description`, `lastMessageAt`, `createdAt`, `createdBy`, `url` y `emailPrefix` (solo al crear); métodos
  `MaskedEmail/get` y `MaskedEmail/set` (sin `/query`); `pending` se borra a las 24 h si no recibe correo,
  `disabled` manda el correo a la papelera, `deleted` lo rebota; el mismo URI es el *scope* OAuth.
- El esquema de Stalwart (`x:MaskedEmail`) toma de ahí `forDomain`, `url`, `createdBy` y `emailPrefix`, pero no es
  la misma API: otro nombre de método (`x:MaskedEmail/*` bajo `urn:stalwart:jmap`), `enable` en vez de `state` y
  sin `lastMessageAt`.
- [bezenti] **Decisión: no implementarla en esta fase.** Las integraciones conocidas (gestores de contraseñas)
  hablan con el servidor de Fastmail, no con un servidor JMAP cualquiera, así que el beneficio real hoy es pequeño;
  y la semántica no coincide del todo (`disabled` de Fastmail acepta y tira a la papelera; nuestro `enable: false`
  rechaza, como dice la documentación de Stalwart). Queda anotada la correspondencia para una fase futura si un
  cliente lo pide: `enabled` ↔ `enable: true`; `deleted` ↔ `enable: false`; `disabled` ↔ (no existe: haría falta
  un modo «a la papelera»); `pending` ↔ (no existe); `lastMessageAt` ↔ (habría que apuntarlo al entregar).

## 9. Lo implementado

Todo tras la feature `bezenti`.

| pieza | dónde |
|---|---|
| Formato, validación del prefijo, reconocimiento y resolución (`masked_email_resolve`) | `crates/common/src/bezenti/enmascaradas.rs` |
| `RCPT TO`: reescritura o rechazo antes de los directorios externos | `crates/common/src/network/mta.rs` (`rcpt_resolve`) |
| Resolución de direcciones para el resto de módulos | `crates/common/src/cache/principals.rs` (`account_id_from_email`) |
| Límite por defecto `x:Email.maxMaskedAddresses` en las cuotas de objetos | `crates/common/src/config/mailstore/email.rs` |
| `x:MaskedEmail/set`: dirección, dominio, límite, caducidad, `createdAt`, `createdBy` | `crates/jmap/src/registry/enmascaradas.rs` y el gancho en `set.rs` |
| `get`/`query`/`set` por el registro genérico (filtro por cuenta, aislamiento de la fase 2) | `crates/jmap/src/registry/{get,query,set}.rs` |
| Borrado con la cuenta | `crates/services/src/task_manager/tasks/destroy_account.rs`, `ACCOUNT_ORPHAN_TYPES` en `set.rs` |

Pruebas: `bezenti/pruebas/archivo_enmascaradas.py` (servidor real con SQLite) y las unitarias de
`crates/common/src/bezenti/enmascaradas.rs`.
