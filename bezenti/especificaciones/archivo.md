# Archivo, retención y recuperación de borrados (fase 3)

Especificación de la edición bezenti. Escrita **solo** con fuentes permitidas: la documentación pública de
Stalwart (citada con su URL), el esquema `resources/schema/schema.json.gz` y el código AGPL de la rama `bezenti`.
Cada punto dice de dónde sale:

- **[doc]**: lo dice la documentación pública (con la página).
- **[esquema]**: lo define el esquema de la API.
- **[bezenti]**: decisión nuestra, porque la documentación no lo dice o porque es una extensión propia.

Páginas citadas: «Recovering deleted items» (<https://stalw.art/docs/management/maintenance/deleted-items/>),
referencias de `ArchivedItem` (<https://stalw.art/docs/ref/object/archived-item/>), `DataRetention`
(<https://stalw.art/docs/ref/object/data-retention/>) y `Task` (<https://stalw.art/docs/ref/object/task/>),
«Tasks» (<https://stalw.art/docs/management/tasks-actions/tasks/>) y «Manage accounts»
(<https://stalw.art/docs/auth/accounts/manage/>).

## 1. Qué es

- [doc] El archivo guarda durante un tiempo lo que los usuarios borran, para poder recuperarlo tras un error,
  incluidos los mensajes ya expurgados de la Papelera o del Spam. Cubre mensajes, archivos, eventos de
  calendario, contactos y guiones Sieve. Vive en los almacenes del propio servidor: **no sustituye a una copia de
  seguridad**.
- [esquema] Objeto `x:ArchivedItem` (permisos `sysArchivedItem{Get,Update,Destroy,Query}`, sin `Create`), de cuenta,
  múltiple (`@type`): `Email` (`from`, `subject`, `receivedAt`, `size`), `FileNode` (`name`, `createdAt`),
  `CalendarEvent` (`title`, `startTime`, `createdAt`), `ContactCard` (`name`, `createdAt`) y `SieveScript` (`name`,
  `createdAt`, `contents`). Todos con `accountId`, `blobId`, `archivedAt`, `archivedUntil` y `status`
  (`ArchivedItemStatus`: `archived` | `requestRestore`). La lista del panel filtra por `accountId`.
- [esquema] Vista `x:DataRetention/ArchivingRetention` con `archiveDeletedItemsFor` y `archiveDeletedAccountsFor`
  (duraciones nulables, en milisegundos). Tarea `RestoreArchivedItem` (`accountId`, `archivedItemType`, `blobId`,
  `archivedUntil`, `createdAt`, todos del servidor) y su permiso `taskRestoreArchivedItem`.

## 2. Activar y retención

- [doc] El archivo está apagado hasta que se fija `archiveDeletedItemsFor` (WebUI: Settings › Storage › Data
  Retention › Archiving). Vacío (por defecto) = lo borrado se elimina al momento. Ejemplo: 30 días =
  `2592000000`.
- [doc] Cada elemento guarda cuándo se archivó (`archivedAt`) y cuándo se borrará para siempre
  (`archivedUntil` = `archivedAt` + retención).
- [bezenti] La retención se aplica **en el momento de borrar**: cambiarla después no alarga ni acorta lo ya
  archivado. La precisión es de segundos.

## 3. Qué se archiva

- [bezenti] En esta fase se archivan los **mensajes** que se borran definitivamente por cualquier vía: JMAP
  `Email/set destroy`, IMAP `EXPUNGE`/`UID EXPUNGE`/`CLOSE`, POP3 `DELE`, borrar un buzón con sus mensajes
  (`onDestroyRemoveEmails`) y el vaciado automático de Papelera y Spam (`expungeTrashAfter`). Quitar un mensaje de
  un buzón cuando sigue en otro no es borrarlo y no se archiva.
- [bezenti] **Pendiente**: archivos (WebDAV), eventos, contactos y guiones Sieve. El modelo y la API ya los admiten
  (variantes del esquema); falta engancharlos al borrado de cada tipo y escribir su restauración. Hasta entonces no
  aparecen en el archivo.
- [bezenti] Lo que se borra al destruir una cuenta **no** se archiva: para eso está `archiveDeletedAccountsFor`
  (§7).

## 4. Restaurar

- [doc] Se restaura poniendo `status: requestRestore` (WebUI o `x:ArchivedItem/set`). El servidor crea una tarea
  `RestoreArchivedItem` que devuelve el elemento a la cuenta.
- [doc] Borrar un `ArchivedItem` lo elimina del archivo para siempre.
- [bezenti] Un mensaje vuelve **íntegro**: mismo contenido (el mismo blob, byte a byte), misma fecha de recepción,
  en sus **buzones originales** que sigan existiendo (si no queda ninguno, en la bandeja de entrada) y con sus
  **etiquetas** (`$seen`, `$flagged`, `$answered`, las propias…), salvo `$deleted` (si no, el siguiente `EXPUNGE`
  lo volvería a borrar); `$hasattachment`/`$hasnoattachment` los vuelve a calcular el servidor al ingresarlo.
  Las cabeceras que el servidor añadió al recibirlo (`Received`, `Delivered-To`…) no están en el blob (se guardan
  en los metadatos), así que el elemento las guarda aparte y la restauración rehace el mensaje completo. Después
  sale del archivo. Hilo, UID IMAP e id JMAP son nuevos (es un mensaje nuevo para los clientes).
- [bezenti] Si la restauración falla de forma definitiva (cuota llena, contenido ya purgado) la tarea queda como
  fallida con el motivo y el elemento vuelve a `archived`, para pedirla otra vez cuando se resuelva.
- [bezenti] Del objeto solo se puede cambiar `status`; cualquier otro campo (`from`, `subject`…) es el registro de
  lo que se borró y se rechaza con `invalidProperties`. Volver a poner `archived` no cancela una restauración ya
  pedida. `x:ArchivedItem/set create` responde `forbidden` (los crea el servidor).
- [bezenti] El `blobId` de un elemento es descargable por la cuenta dueña mientras esté archivado (enlace temporal
  de la cuenta), así se puede ver el mensaje antes de restaurarlo: es el mensaje tal como llegó, sin las cabeceras
  que añadió el servidor.

## 5. Cómo se guarda y purga al vencer

- [bezenti] Cada elemento es un registro propio (JSON compacto: tipo, cuenta, hash del blob, tamaño, fechas,
  remitente, asunto, buzones, etiquetas y, si las hay, las cabeceras añadidas por el servidor) en el subespacio
  `DeletedItems` del almacén, con la clave de registro `(x:ArchivedItem, id)` y el índice `accountId` del
  registro. `x:ArchivedItem` no es un objeto normal del registro (`OBJ_NOT_STORED`, decisión de la fase 1).
- [bezenti] El contenido no se copia: el blob sigue en el almacén de blobs gracias a un **enlace temporal**
  `(hash, cuenta, archivedUntil)` cuyo valor apunta al elemento. Si dos copias del mismo mensaje se borran en el
  mismo segundo, la segunda vence un segundo después para que cada una tenga su enlace.
- [bezenti] **Purga al vencer**: la hace la limpieza de blobs programada (`x:DataRetention.blobCleanupSchedule`,
  por defecto cada día a las 04:00, o a mano con la tarea `StoreMaintenance` de tipo `purgeBlob`). El código libre
  ya borra los enlaces temporales vencidos y el registro al que apuntan (como con las muestras antispam); con eso
  desaparecen el elemento, su índice y, si nadie más lo usa, el blob. No hace falta otra tarea.
- [bezenti] Un mensaje se archiva en dos pasos, porque el servidor suelta el blob de un mensaje borrado más tarde,
  en la cola del índice: (1) al borrarlo se apunta un «pendiente» con lo que el borrado pierde (buzones, etiquetas,
  fecha de recepción y vencimiento); (2) cuando la cola del índice quita sus metadatos y suelta el blob, crea el
  elemento con el remitente, el asunto y el tamaño y pone el enlace temporal **en el mismo lote** en que se suelta
  el de documento, así el blob nunca queda sin enlace. Por eso un mensaje borrado tarda unos instantes en aparecer
  en el archivo.

## 6. Quién ve qué (aislamiento)

- [doc] Los usuarios ven el archivo de su cuenta (Account › Archived Items). Los administradores usan la misma
  lista, una cuenta cada vez, con el filtro `accountId` de `x:ArchivedItem/query`. El filtro solo se respeta para
  administradores con `impersonate`; para los demás se ignora y se listan los de su cuenta.
- [doc] El rol User tiene `sysArchivedItem{Get,Query,Update,Destroy}` para sus propios elementos (ya estaba así en
  el código libre).
- [bezenti] A los miembros de un inquilino se les quita `impersonate` (fase 2), así que con la regla de la
  documentación un administrador de inquilino no podría recuperar el correo de sus usuarios. Extensión: un usuario
  de un inquilino con `sysAccountGet` (los administradores de inquilino lo tienen por el techo del inquilino) puede
  usar el filtro `accountId` y `get`/`set` sobre los elementos de las **cuentas de su ámbito** (su inquilino y sus
  descendientes, como en la fase 2). Fuera del ámbito, nada existe: `query` sale vacío (sin contar), `get` y `set`
  responden `notFound`.
- [bezenti] El operador (sin inquilino, con `impersonate`) ve el de cualquier cuenta con el filtro. Sin filtro, todos
  ven el de su propia cuenta (una cuenta cada vez, como dice la documentación).
- [bezenti] El campo `accountId` solo se oculta en la respuesta a quien únicamente ve lo suyo.

## 7. Cuentas borradas (`archiveDeletedAccountsFor`)

- [doc] Borrar una cuenta tiene dos pasos: al momento se borra el objeto (ya no puede entrar) y se revocan sus
  comparticiones; después una tarea `DestroyAccount` borra sus datos (buzones, mensajes, calendarios, contactos,
  archivos, Sieve, índice), sus claves públicas, sus enmascaradas y sus autorizaciones OAuth. Con
  `archiveDeletedAccountsFor`, esa tarea vence al final del plazo, y la cuenta se puede recuperar hasta entonces.
- [doc] Recuperar = **borrar la tarea `DestroyAccount` antes de que se ejecute**. La cuenta se vuelve a crear con
  su id, nombre y dominio originales, así que sus buzones, mensajes, calendarios, contactos y archivos vuelven; se
  programa la reconstrucción de su índice de comparticiones. La recuperación es parcial: credenciales, grupos y el
  resto de ajustes quedan por defecto. Errores: `Task is currently being processed and cannot be destroyed` (la
  tarea ya se está ejecutando) y `Account recovery failed` (no se puede volver a crear, p. ej. porque otra cuenta
  tiene ya su nombre). Hace falta `taskDestroyAccount`.
- [bezenti] La cuenta recuperada vuelve al **inquilino de su dominio** (si no, quedaría como cuenta del operador en
  un dominio de inquilino, lo que la fase 2 prohíbe). La tarea `DestroyAccount` y `x:Task` siguen siendo solo del
  operador: un administrador de inquilino no recupera cuentas (pendiente).
- [bezenti] Mientras dura el plazo, el archivo de la cuenta y sus enmascaradas se conservan; si se recupera, siguen
  ahí. Si la tarea se ejecuta, se borran también el archivo de la cuenta y sus pendientes.

## 8. Cuotas

- [bezenti] **El archivo no cuenta para la cuota de disco** (ni de la cuenta ni del inquilino). Motivo: el usuario
  borra precisamente para liberar espacio, y si lo borrado siguiera contando, borrar no serviría de nada durante
  todo el plazo de retención. El espacio que ocupa el archivo lo decide el operador con la retención. Al restaurar,
  el mensaje vuelve a contar (se ingresa con la comprobación de cuota normal y la del inquilino): si no cabe, la
  restauración falla con el motivo y el elemento sigue archivado.

## 9. Lo implementado

Todo tras la feature `bezenti`.

| pieza | dónde |
|---|---|
| Retención (`archiveDeletedItemsFor`, `archiveDeletedAccountsFor`) en la configuración | `crates/common/src/config/mailstore/email.rs` |
| Formato y claves del archivo, pendientes, búsqueda por enlace | `crates/common/src/bezenti/archivo.rs` |
| Subespacio `DeletedItems` para los elementos; un enlace temporal de un elemento es dato de usuario | `crates/store/src/write/{key,blob}.rs` |
| Apuntar el mensaje al borrarlo (JMAP, POP3, vaciado automático, borrar buzón, IMAP) | `crates/email/src/bezenti.rs`, `message/delete.rs`, `mailbox/destroy.rs`, `crates/imap/src/op/expunge.rs` |
| Crear el elemento al soltar el blob | `crates/services/src/bezenti/archivo.rs` y el gancho en `index_queue/index.rs` |
| Tarea `RestoreArchivedItem` | `crates/services/src/bezenti/archivo.rs`, `task_manager/{worker,manager}.rs` |
| `x:ArchivedItem/get`, `/query`, `/set` con la vista por inquilino | `crates/jmap/src/registry/archivo.rs` y los ganchos en `get.rs`, `query.rs`, `set.rs` |
| Aplazar `DestroyAccount` y recuperar la cuenta al borrar la tarea | `crates/jmap/src/registry/archivo.rs`, `mapping/principal.rs`, `mapping/task.rs` |
| Borrar el archivo y los pendientes con la cuenta | `crates/services/src/task_manager/tasks/destroy_account.rs` |

Pruebas: `bezenti/pruebas/archivo_enmascaradas.py` (servidor real con SQLite) y las unitarias de
`crates/common/src/bezenti/archivo.rs`.

## 10. Pendiente

- Archivar y restaurar archivos, eventos, contactos y guiones Sieve (§3).
- Recuperación de cuentas por un administrador de inquilino (`x:Task` es solo del operador).
- Métrica o evento propio del archivo (se usan los eventos libres de la cola del índice y de las tareas).
