# Multi-inquilino (fase 2)

Especificación de la edición bezenti. Escrita **solo** con fuentes permitidas: la documentación pública de
Stalwart (citada con su URL), el esquema `resources/schema/schema.json.gz` y el código AGPL de la rama `bezenti`.
Cada punto dice de dónde sale:

- **[doc]**: lo dice la documentación pública (con la página).
- **[esquema]**: lo define el esquema de la API.
- **[bezenti]**: decisión nuestra, porque la documentación no lo dice o porque es una extensión propia.

El objetivo de negocio: dar correo a otras empresas. Cada empresa es un **inquilino**: gestiona sus dominios,
cuentas, grupos, listas y roles sin ver a las demás. Más adelante habrá **revendedores**: inquilinos que crean y
gestionan otros inquilinos.

## 1. Qué es un inquilino

- [doc] «Each organisation is a tenant». Las cuentas de un inquilino no alcanzan los objetos de otro y su
  administrador gestiona solo los objetos de su inquilino
  (<https://stalw.art/docs/auth/authorization/tenants/>).
- [esquema] Objeto `x:Tenant` (`sysTenant{Get,Create,Update,Destroy,Query}`) con `name` (obligatorio), `logo`
  (URL o imagen en base64, nulable), `roles` (`Default` | `Custom`), `permissions` (`Inherit` | `Merge` |
  `Replace`), `quotas` (mapa `TenantStorageQuota` → entero), y, puestos por el servidor, `createdAt` y
  `usedDiskQuota`. La lista del panel filtra por `text` (<https://stalw.art/docs/ref/object/tenant/>).
- [bezenti] El nombre es único en todo el servidor (índice único; se rechaza con `primaryKeyViolation`, sin el id
  del inquilino con el que choca). Fase 1 lo declaraba pero
  el generador no lo indexaba: se corrige en esta fase (`decisiones.json`, `indice_campos`).
- [bezenti] Un inquilino no se puede borrar mientras tenga objetos (cuentas, dominios…) o inquilinos hijos: el
  registro lo impide con `objectIsLinked` (cada `memberTenantId` es clave ajena de `x:Tenant`).

## 2. Qué objetos pertenecen a un inquilino

- [doc] «An object belongs to a tenant through its `memberTenantId` field»: cuentas y grupos, dominios, listas
  de correo, roles, clientes OAuth, directorios, firmas DKIM, proveedores ACME, servidores DNS e informes externos
  de los dominios del inquilino (<https://stalw.art/docs/auth/authorization/tenants/>).
- [esquema] Son los objetos con el indicador `OBJ_FILTER_TENANT`: `x:Account`, `x:Domain`, `x:MailingList`,
  `x:Role`, `x:OAuthClient`, `x:Directory`, `x:DkimSignature`, `x:AcmeProvider`, `x:DnsServer`,
  `x:ArfExternalReport`, `x:DmarcExternalReport`, `x:TlsExternalReport`.
- [doc] Lo que crea un administrador de inquilino «is placed in the administrator's tenant automatically»; el
  operador del servidor trabaja fuera de todo inquilino y pone `memberTenantId` a mano.
- [doc] Una lista de correo de un inquilino debe usar un dominio del mismo inquilino
  (<https://stalw.art/docs/email/management/mailing-lists/>).
- [bezenti] Generalizamos esa regla a **toda referencia** (clave ajena) entre objetos de inquilino: una cuenta,
  sus alias, sus grupos y sus roles, una lista, una firma DKIM… solo pueden apuntar a objetos del **mismo**
  inquilino. Un objeto sin inquilino (del operador) no puede apuntar a objetos de un inquilino. Un objeto de un
  inquilino sí puede apuntar a objetos sin inquilino si lo hace el operador; un administrador de inquilino solo
  puede usar así los **proveedores ACME y servidores DNS** del operador (servicios compartidos que no exponen
  datos de nadie). Lo comprueba el almacén del registro al escribir (red de seguridad para todos) y la API
  (para que un administrador de inquilino no distinga «no existe» de «es de otro»).
- [bezenti] Las firmas DKIM que el servidor crea solo para un dominio heredan el inquilino del dominio.

## 3. Permisos

- [doc] Orden de cálculo de los permisos efectivos
  (<https://stalw.art/docs/auth/authorization/permissions>): (1) se suman los roles, incluidos los anidados;
  (2) se aplica el campo `permissions` de la cuenta; (3) se aplica el **techo del inquilino**: «restricted to the
  permissions of the account's tenant, and `impersonate` is removed from members of a tenant»; (4) lo deshabilitado
  en cualquier nivel gana.
- [doc] El techo del inquilino son sus propios `roles`/`permissions`; con `roles: Default` se usan los
  `defaultTenantRoleIds` de `x:Authentication`. «A permission that the tenant lacks therefore has no effect on its
  members, whoever assigns it.»
- [doc] Roles creados en el primer arranque: User, Group, **Tenant Administrator** (en `defaultTenantRoleIds`
  junto con User) y System Administrator (<https://stalw.art/docs/auth/authorization/roles/>). Tenant
  Administrator concede `authenticate`, `authenticateWithAlias`, `liveDeliveryTest`, `interactAi` y
  `sysDomain*`, `sysDkimSignature*`, `sysAcmeProvider*`, `sysAccount*`, `sysRole*`, `sysOAuthClient*`,
  `sysMailingList*`, los informes externos, `sysDnsServer*` y `sysQueuedMessage*`. **No** incluye `sysTenant*`
  ni `sysDirectory*`.
- [doc] Una cuenta de usuario con `roles: Admin` que pertenece a un inquilino se convierte en «an administrator of
  its tenant and nothing more» (`x:Account`, <https://stalw.art/docs/ref/object/account/>).
- [doc] Para gestionar una cuenta, quien llama debe tener **todos** los permisos efectivos de esa cuenta, sin
  excepción para los superusuarios.
- [bezenti] Un administrador de inquilino (`roles: Admin`) recibe como base **el techo completo de su
  inquilino** y después se aplica su campo `permissions` y otra vez el techo. Con `roles: Default` el resultado es
  el mismo que dice la documentación (los `defaultTenantRoleIds`); la diferencia es que, si el operador amplía el
  techo de un inquilino (p. ej. con `sysTenant*` para hacerlo revendedor), sus administradores lo reciben sin más
  pasos. Así «administrador del inquilino» significa siempre «todo lo que el inquilino puede hacer».
- [bezenti] En instalaciones que ya existían (con roles creados antes de esta fase) se crea el rol «Tenant
  Administrator» y se rellena `defaultTenantRoleIds` la primera vez que arranca la edición bezenti si estaba
  vacío.

## 4. Aislamiento (lo que ve y hace un administrador de inquilino)

La documentación solo dice que el administrador de inquilino gestiona lo suyo «and nothing outside the tenant».
Todo lo que sigue es [bezenti] y es la parte más importante de la fase:

1. **Ámbito.** El ámbito de un usuario de inquilino T es T más todos sus descendientes (ver §8). Un usuario sin
   inquilino (el operador) no tiene límite.
2. **Tipos de objeto.** Un usuario de inquilino solo puede usar en la API del registro (`x:*/get`, `x:*/query`,
   `x:*/set`): los objetos de inquilino que se guardan en el registro (`x:Account`, `x:Domain`, `x:MailingList`,
   `x:Role`, `x:OAuthClient`, `x:Directory`, `x:DkimSignature`, `x:AcmeProvider`, `x:DnsServer`), los objetos de
   su propia cuenta (`x:AccountSettings`, `x:ApiKey`, `x:AppPassword`, `x:AccountPassword`, `x:Passkey`,
   `x:PublicKey`, `x:OAuthGrant`, `x:SpamTrainingSample`, `x:MaskedEmail`, `x:ArchivedItem`) y `x:Tenant` (solo
   los inquilinos **descendientes**, nunca el suyo ni sus ancestros: así no puede cambiarse sus propias cuotas ni
   permisos). Fase 8: también la **cola de salida** (`x:QueuedMessage`, solo los mensajes de su ámbito, §5), los
   **informes externos** (`x:ArfExternalReport`, `x:DmarcExternalReport`, `x:TlsExternalReport`, solo los de los
   dominios de su ámbito, §4.12) y las **cuentas borradas pendientes de destrucción** (`x:Task`, solo las tareas
   `DestroyAccount` de su ámbito; ver `archivo.md`, §7). Cualquier otro tipo (configuración del servidor, tareas
   del servidor, registros, acciones, informes salientes…) responde `forbidden` aunque se le hayan dado los
   permisos por error.
3. **get.** Un objeto fuera del ámbito se responde como `notFound`, igual que un id que no existe.
4. **query.** Los resultados se restringen al ámbito antes de ordenar y paginar: no se pueden contar ni deducir
   objetos ajenos. Filtrar por `memberTenantId` de otro inquilino devuelve una lista vacía.
5. **set.** Actualizar o destruir un objeto fuera del ámbito responde `notFound`. Al crear, `memberTenantId`
   vacío se rellena con el inquilino de quien crea; si se indica, debe estar dentro del ámbito (si no,
   `forbidden`). Al actualizar no se puede sacar un objeto del ámbito ni dejarlo sin inquilino. Las claves ajenas
   que apunten fuera del ámbito se rechazan como `invalidForeignKey`, igual que las que no existen. Los errores de
   clave primaria duplicada (p. ej. crear un dominio con el nombre de un dominio de otro inquilino) no devuelven
   el id del objeto ajeno. Un objeto no cambia de inquilino mientras otros objetos de su inquilino actual lo usen
   (p. ej. un dominio con cuentas): `objectIsLinked`.
   **Escalada**: un usuario de inquilino no puede conceder (en cuentas, grupos, roles o inquilinos hijos) un
   permiso que él no tenga; se rechaza con `forbidden` aunque el techo ya lo anularía, para que quede claro.
   **Secretos**: [doc] solo por valor; se rechaza cualquier origen `File` o `EnvironmentVariable` (leer archivos
   o variables del servidor).
6. **changes.** El registro no tiene método `/changes` (en el código AGPL solo hay `get`, `query` y `set`), así
   que no hay nada que filtrar; el método responde como desconocido.
7. **Directorio de principales.** `Principal/get`, `Principal/query`, la disponibilidad de calendario y la
   búsqueda y el listado de principales de WebDAV solo muestran a un usuario de inquilino cuentas de su **mismo**
   inquilino (ni las de sus hijos: el directorio es de la empresa, no de la administración). Los usuarios del
   operador (sin inquilino) siguen viendo el directorio como en la edición libre.
8. **Compartir.** No se puede compartir (ACL de JMAP e IMAP) con una cuenta de otro inquilino.
9. **Suplantación.** A los miembros de un inquilino se les quita `impersonate` siempre ([doc]); por eso no pueden
   usar el `accountId` de otra cuenta.
   **OAuth**: [doc] un cliente OAuth de un inquilino solo deja entrar a cuentas de ese inquilino (las demás
   reciben «denegado»).
10. **Red.** [doc] Los administradores de inquilino no tienen la posición de red del servidor: las URL que
    configuran (ACME, DNS, logos) van con la política `PublicOnly` (https, sin credenciales, IP públicas).
11. **El operador** (cuenta sin inquilino con permisos de administración) ve y gestiona todo.
12. **Informes externos** (fase 8). [esquema] `x:ArfExternalReport`, `x:DmarcExternalReport` y
    `x:TlsExternalReport` tienen `memberTenantId`. [bezenti] Al recibir un informe (DMARC agregado, TLS-RPT o
    ARF) se le pone el inquilino del dominio al que se refiere: primero los dominios de su contenido (el dominio
    publicado en DMARC, las políticas de TLS, los dominios denunciados en ARF) que son de este servidor y, si
    ninguno lo es, los de sus destinatarios. Si todos los de este servidor son del mismo inquilino, el informe es
    de ese inquilino; si son de varios, o alguno es del operador, queda solo para el operador (nunca se enseña a
    un inquilino un informe que también habla de otro). Se indexa por `memberTenantId`, así que `query`, `get` y
    `set` (borrar) del administrador de inquilino ven solo los de su ámbito y lo demás responde `notFound`. Los
    informes salientes (`x:DmarcInternalReport`, `x:TlsInternalReport`) siguen siendo del operador. El rol
    «Tenant Administrator» ya tiene los permisos de los informes externos (§3).

## 5. Cuotas y límites

- [doc] Claves de `TenantStorageQuota`: `maxAccounts`, `maxGroups`, `maxDomains`, `maxMailingLists`,
  `maxRoles`, `maxOauthClients`, `maxDkimKeys`, `maxDnsServers`, `maxDirectories`, `maxAcmeProviders` y
  `maxDiskQuota` (bytes) (<https://stalw.art/docs/auth/authorization/tenants/#quotas>). Se aplican a todo lo que
  se crea en el inquilino, lo cree quien lo cree, y a lo que entra en él cambiando `memberTenantId`; al pasarse se
  responde `overQuota` («You have exceeded your quota of 50 accounts»).
- [doc] La cuota de disco es un segundo límite, compartido por los miembros, además del de cada cuenta. IMAP
  responde `NO [OVERQUOTA] Organization disk quota exceeded.` y la entrega SMTP se aplaza con
  `Organization over quota.`
- [bezenti] Uso de disco del inquilino = suma del uso de sus cuentas (y de las de sus descendientes). Se calcula
  bajo demanda sumando los contadores de cuota de cada cuenta y se guarda en caché unos segundos (no hay contador
  propio del inquilino: así nunca se desvía y no hace falta la tarea `recalculateQuota`). `usedDiskQuota` de
  `x:Tenant/get` devuelve ese valor.
- [bezenti] `maxAccounts` cuenta cuentas de usuario y `maxGroups` cuentas de grupo. Un 0 o la ausencia de la clave
  significa «sin límite».
- [bezenti] Con revendedores las cuotas son **jerárquicas**: lo de un hijo cuenta también para el padre, así que un
  revendedor nunca puede pasar de su propia cuota repartiéndola entre hijos.
- [doc] No hay límites de cola ni de envío por inquilino en la documentación (`MtaQueueQuota` y los throttles no
  tienen clave de inquilino). La fase 8 los añade (§5.1).

### 5.1 Cola SMTP y límites de envío por inquilino (fase 8)

Todo [bezenti]: ni el esquema ni la documentación lo definen. Objetivo: que un inquilino no pueda acaparar la cola
ni quemar la reputación de la IP de todos.

- **De quién es un mensaje en cola.** Se apunta al encolarlo (metadato propio del mensaje, al final de la lista
  para que los mensajes ya encolados se sigan leyendo) y solo cuando se sabe de verdad: el envío autenticado por
  SMTP (`AUTH`) o por JMAP (`EmailSubmission`) es del inquilino de la cuenta; lo que genera la entrega local para
  una cuenta (redirecciones y respuestas de Sieve) es del inquilino de esa cuenta; un aviso de entrega (DSN) de
  un mensaje de un inquilino es de ese inquilino. El correo entrante sin autenticar **no es de nadie** (es del
  operador) aunque el remitente diga ser de un dominio de un inquilino: así nadie puede colar mensajes (ni su
  contenido) en la vista de otro.
- **Campos propios de `x:Tenant`** (prefijo `bezenti`, `extensiones.json`; nulos = sin límite):
  - `bezentiMaxQueuedMessages`: mensajes del inquilino que pueden estar **a la vez en la cola**. Al llegar al
    límite, un envío autenticado nuevo se aplaza con `452 4.3.1` (el cliente lo reintenta más tarde). Cuenta todo
    lo del inquilino (también los avisos y lo generado por Sieve) pero solo rechaza los envíos autenticados: un
    aviso de entrega nunca se pierde por esto. Se lleva con los contadores de cuota de la cola del código libre
    (los de `MtaQueueQuota`), que se liberan cuando el mensaje sale de la cola (entregado, caducado o cancelado).
  - `bezentiMessageRate` (`x:Rate`): **mensajes por intervalo** de los envíos autenticados del inquilino.
  - `bezentiRecipientRate` (`x:Rate`): **destinatarios por intervalo** (la suma de los destinatarios de cada
    mensaje); un mensaje que no cabe entero se rechaza entero.
  Al pasarse de un ritmo, `451 4.7.1 Sending rate exceeded for your organization, try again later.` y el evento
  `queue.rate-limit-exceeded` con el inquilino. Los ritmos son ventanas fijas en el almacén en memoria (como los
  throttles libres), así que en un clúster con Redis se comparten entre nodos.
- **Jerárquicos** (revendedores, §8): lo de un hijo cuenta también para los límites de su padre y de sus
  ancestros; basta con que uno se pase para aplazar el envío.
- **Reparto justo al entregar.** En cada cola virtual, los hilos de entrega se reparten entre inquilinos (más el
  grupo «sin inquilino»): (1) el valor de la clave de cada evento de la cola lleva el inquilino del mensaje, así
  que el gestor lo sabe sin leer el mensaje; (2) al buscar mensajes listos mira hasta 2.048 eventos más allá de
  los que busca el código libre (variable de entorno `BEZENTI_COLA_VENTANA`) y elige los candidatos **por turnos
  entre inquilinos**, empezando por el que menos tiene en marcha; (3) de los listos sale primero el del inquilino
  con menos entregas en marcha; (4) un mensaje recién encolado de un inquilino que no tiene nada en marcha en esa
  cola entra en la lista de listos aunque esté llena. Resultado: un envío masivo de un inquilino no retrasa el
  correo de los demás (prueba: con 2 hilos y 40 mensajes de A delante, el de B sale a la vez que los que ya
  estaban en marcha). Límite honesto: un nodo que arranca con más de 2.048 mensajes vencidos de un mismo inquilino
  por delante tarda en ver los de otros hasta que esa cola baja; `bezentiMaxQueuedMessages` lo evita.
- **Vista y gestión para el administrador de inquilino** (permisos `sysQueuedMessage*`, que ya están en el rol
  «Tenant Administrator»): `x:QueuedMessage/query` (con sus filtros: texto, remitente, destinatario, cola,
  vencimiento) recorre los mensajes y deja solo los de su ámbito; `get` y la descarga del mensaje (`blobId`)
  responden `notFound` a lo ajeno; `set` reintenta («Retry Now», `nextRetry`) y cancela («Cancel Delivery»,
  `destroy`) solo los suyos. El historial de entrega de un mensaje (`x:Trace`) ya estaba aislado (fase 5).

## 6. Cómo se resuelve el inquilino

- [doc] No está documentado. Se deduce que el inquilino es el `memberTenantId` de la cuenta autenticada.
- [bezenti] Así es: en login (contraseña, OAuth, contraseñas de aplicación, claves API), SMTP AUTH, IMAP, POP3,
  JMAP y WebDAV el inquilino es el `memberTenantId` de la cuenta. El token de acceso lo guarda junto a los permisos
  (y se recalcula cuando cambian la cuenta, su inquilino o los roles). La recepción de correo no necesita
  inquilino: la dirección lleva a la cuenta y la cuenta lleva a su inquilino (para la cuota de disco).

## 7. Logo

- [doc] `GET /logo` sirve el logo de marca según el **host de la petición**, de lo más concreto a lo más general:
  `logo` del dominio, `logo` del inquilino dueño del dominio, logo general y logo integrado. Se aceptan URL https,
  base64 o URI `data:`, en PNG, JPEG, GIF, WebP, ICO o SVG, máx. 1 MiB; las URL las descarga el servidor (15 s,
  máx. 5 redirecciones) y todo se sirve con una CSP restrictiva
  (<https://stalw.art/docs/management/webui/branding/>).
- [bezenti] Implementamos `GET /logo` con esa cadena salvo el «logo general», que en Stalwart es parte de
  `x:Enterprise` (licencias) y no se implementa; sin logo propio se responde `404` y la interfaz usa el suyo. Un
  dominio hijo de un revendedor sin logo hereda el de su inquilino y, si no tiene, el de los ancestros. El logo se
  guarda en caché por dominio.

## 8. Extensión propia: revendedores

[bezenti] La documentación no tiene jerarquía de inquilinos. La añadimos así:

- **Campo propio** `x:Tenant.bezentiParentTenantId` (id de `x:Tenant`, nulable): el inquilino padre.
  `null` = inquilino de primer nivel, gestionado solo por el operador. Lleva el prefijo `bezenti` para que nunca
  choque con un campo futuro del esquema oficial; un cliente que no lo conozca lo ignora. Se genera con el resto
  del registro desde `bezenti/generador/extensiones.json` (solo en la edición bezenti), con índice de búsqueda y
  clave ajena (no se puede borrar un padre con hijos).
- **Por qué un campo y no un objeto aparte**: la relación es 1-a-N y forma parte de la identidad del inquilino;
  un campo indexado se consulta y se valida (clave ajena, ciclos) con la misma maquinaria que `memberTenantId`, y
  un objeto aparte obligaría a duplicar get/set/query y su aislamiento.
- **Quién es revendedor**: un inquilino cuyo techo incluye `sysTenant*`. Lo decide el operador ampliando
  `permissions` del inquilino (p. ej. `Merge` con `sysTenantGet`, `sysTenantCreate`, `sysTenantUpdate`,
  `sysTenantDestroy`, `sysTenantQuery`). Sus administradores lo reciben automáticamente (§3).
- **Reglas**:
  - El administrador del padre ve y gestiona a sus inquilinos descendientes y todos sus objetos (el ámbito de §4
    incluye a los descendientes). Al crear un inquilino, `bezentiParentTenantId` vacío se rellena con el suyo; si
    lo indica, debe ser el suyo o un descendiente.
  - Nunca ve ni modifica su propio objeto `x:Tenant` ni el de sus ancestros o hermanos.
  - El hijo no ve al padre ni a sus hermanos: su ámbito es él mismo y sus descendientes.
  - Techo de permisos: el de un hijo es siempre la intersección con el de su padre (y así hacia arriba): un
    revendedor no puede dar a sus clientes más de lo que tiene.
  - Cuotas jerárquicas (§5).
  - No se permiten ciclos: el padre no puede ser el propio inquilino ni un descendiente suyo.

## 9. Fuera de esta fase

- Directorios externos (LDAP/SQL/OIDC) por dominio o inquilino (`x:Domain.directoryId`): sigue fuera (ver
  `README.md`, «Estado»).
- Tarea `TenantMaintenance` (`recalculateQuota`) y `resetTenantQuotas`: no hacen falta con el cálculo bajo demanda
  y no se implementan (la API no deja crearlas).
- Evento y métrica `limit.tenant-quota`: se usa el evento libre `limit.quota` con el detalle «tenant».

## 10. Lo implementado en la fase 2

Todo va tras la feature `bezenti`; sin ella el código libre se compila y se comporta como antes.

| pieza | dónde |
|---|---|
| Campo propio `bezentiParentTenantId`, índice único del nombre del inquilino, accesores de `memberTenantId` | `bezenti/generador/extensiones.json`, `decisiones.json`, `gen_inquilino.py` → `crates/registry/src/bezenti/inquilino_gen.rs` |
| Esquema que sirve `/api/schema` (completo, sin `x:Enterprise`, con las extensiones) para que el panel muestre inquilinos | `bezenti/generador/gen_esquema.py` → `resources/schema/schema_bezenti.json.gz` |
| Red de seguridad del almacén: ninguna clave ajena cruza inquilinos; `RegistryQuery::with_tenant` | `crates/store/src/registry/bezenti.rs` |
| Caché de inquilinos, ámbito, techo de permisos, permisos efectivos, cuotas jerárquicas, uso de disco | `crates/common/src/bezenti/inquilino.rs` |
| Rol «Tenant Administrator» y `defaultTenantRoleIds` al arrancar | `crates/common/src/bezenti/arranque.rs` |
| Logo por dominio/inquilino y `GET /logo` | `crates/common/src/bezenti/logo.rs`, `crates/http/src/request.rs` |
| Inquilino en el token de acceso, `AccountCache` y `DomainCache`; invalidación de cachés (`CacheInvalidation::Tenant`, también entre nodos) | `crates/common/src/auth/`, `cache/`, `ipc.rs`, `crates/services/src/broadcast/` |
| Aislamiento de `x:*/get`, `x:*/query`, `x:*/set` y CRUD de `x:Tenant` | `crates/jmap/src/registry/bezenti.rs` y los ganchos en `get.rs`, `query.rs`, `set.rs` |
| Directorio de principales (JMAP y WebDAV), compartir (JMAP e IMAP), OAuth | `crates/jmap/src/principal/`, `api/acl.rs`, `crates/dav/src/principal/`, `crates/imap/src/op/acl.rs`, `crates/http/src/auth/oauth/auth.rs` |
| Cuota de disco del inquilino en la entrega SMTP, IMAP APPEND/COPY, JMAP y WebDAV | `crates/common/src/storage/quota.rs`, `crates/email/src/message/{ingest,copy,delivery}.rs`, `crates/imap/src/op/append.rs`, `crates/dav/src/common/propfind/mod.rs` |
| Firmas DKIM automáticas en el inquilino del dominio | `crates/services/src/task_manager/tasks/dkim.rs` |
| Fase 8: inquilino del mensaje en cola, límites y contador de cola | `crates/smtp/src/queue/bezenti.rs`, ganchos en `queue/spool.rs`, `inbound/data.rs`, `outbound/local.rs`, `queue/dsn.rs` |
| Fase 8: reparto justo entre inquilinos al entregar | `crates/smtp/src/queue/manager.rs` (`bezenti_*`), `queue/walk.rs`, `crates/common/src/ipc.rs` |
| Fase 8: campos `bezentiMaxQueuedMessages`, `bezentiMessageRate`, `bezentiRecipientRate` | `bezenti/generador/extensiones.json`, `crates/common/src/bezenti/inquilino.rs` (`TenantCache`) |
| Fase 8: `x:QueuedMessage` por inquilino (query, get, set, descarga) | `crates/jmap/src/registry/mapping/queued_message.rs`, `crates/jmap/src/blob/download.rs` |
| Fase 8: informes externos con inquilino | `crates/smtp/src/reporting/bezenti.rs`, `reporting/{analysis,index}.rs`, `crates/jmap/src/registry/mapping/report.rs` |
| Fase 8: cuentas borradas desde el administrador de inquilino | `crates/jmap/src/registry/archivo.rs`, `mapping/task.rs`, `crates/jmap/src/api/auth.rs` |

### Pruebas

- `bezenti/pruebas/multi_inquilino.py`: levanta el servidor real con SQLite y prueba por JMAP, SMTP, IMAP y HTTP
  (unas 145 comprobaciones): los ataques de un administrador de inquilino contra otro (get por id adivinado, get
  sin ids, query sin filtro, con filtro por inquilino ajeno, por nombre, por texto, paginada y con
  `calculateTotal`; modificar, borrar o mover objetos ajenos; robar un nombre de dominio o alias; apuntar a
  dominios, grupos, roles o listas ajenos; darse roles o permisos del operador; leer archivos del servidor como
  secreto; leer la configuración, la cola, tareas o registros; lanzar acciones; usar el `accountId` de otra
  cuenta; `/changes`; directorio de principales; compartir por JMAP e IMAP), una prueba «fuzz» que pasa por
  get/set update/set destroy/query de cada tipo con todos los ids que no son del ámbito (para el revendedor y para
  su hijo), la jerarquía de revendedores (ver y gestionar hijos, no verse a sí mismo ni al padre ni a hermanos, no
  crear ciclos, no dar más permisos de los que tiene), las cuotas (de objetos, jerárquicas y de disco por IMAP y
  SMTP), el flujo de correo SMTP → IMAP/JMAP en un dominio de inquilino, las firmas DKIM, el logo y el esquema
  servido.
- Pruebas unitarias en `crates/common/src/bezenti/` (`cargo test -p common --features bezenti --lib bezenti::`).
- Fase 8: `bezenti/pruebas/cola_inquilinos.py` (servidor real con SQLite y un SMTP remoto falso): reparto justo
  (40 mensajes de A con 2 hilos y uno de B, que sale enseguida), vista y gestión de la cola por cada
  administrador de inquilino (query, filtros, total, get, descarga, reintentar, cancelar; lo ajeno → `notFound`),
  ritmos de mensajes y de destinatarios, jerárquicos (revendedor), máximo de mensajes en cola (y que se libera al
  cancelar), informes DMARC y TLS por inquilino y recuperación de cuentas borradas por el administrador de
  inquilino (con su cuota).
- `bezenti/pruebas.sh` (CI) las lanza todas y comprueba que los crates tocados compilan y pasan sus pruebas sin la
  feature.
