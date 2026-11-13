# Conectar Entra ID, Okta o Keycloak por SCIM

Guía para dar de alta y de baja las cuentas de correo automáticamente desde el directorio de la empresa. El
proveedor de identidad (IdP) crea la cuenta cuando entra un empleado, la cambia cuando cambia, la **suspende**
cuando se va (`active: false`) y, si se quiere, la **borra**. Los detalles están en
[`../especificaciones/scim.md`](../especificaciones/scim.md).

SCIM solo crea y mantiene cuentas: **no** comprueba contraseñas. Los usuarios entran con el directorio que ya
uséis (OIDC del mismo IdP, LDAP…) o con contraseñas que ponga el administrador.

En los ejemplos `$S` es la URL pública del servidor y `jmap()` es la función de la guía
[`alta-de-empresa.md`](alta-de-empresa.md) (API JMAP de administración).

## 1. Preparar el servidor

### 1.1 Si es para una empresa (inquilino): darle permiso de SCIM

El rol por defecto de los inquilinos no incluye SCIM; el operador lo concede a cada empresa que lo vaya a usar:

```sh
jmap '["x:Tenant/set",{"update":{"ID_INQUILINO":{"permissions":{"@type":"Merge",
  "enabledPermissions":{"scimAccess":true},"disabledPermissions":{}}}}},"0"]'
```

Las cuotas del inquilino (`maxAccounts`, `maxGroups`…) siguen valiendo: si el IdP intenta pasarse, recibe un 403.

### 1.2 Abrir los dominios al IdP

Solo los dominios con `allowScimProvisioning` se pueden aprovisionar. Desde ese momento SCIM **manda** sobre las
cuentas del dominio: el inicio de sesión con un directorio externo ya no crea ni cambia cuentas en él.

```sh
jmap '["x:Domain/set",{"update":{"ID_DOMINIO":{"allowScimProvisioning":true}}},"0"]'
```

Si el dominio ya tiene cuentas, no se migran: el IdP las adopta si casan por `userName`. Haced la primera
sincronización con pocos usuarios y comprobad que no se duplican.

### 1.3 La cuenta de aprovisionamiento

Una cuenta dedicada, **en uno de los dominios abiertos** (los grupos que cree el IdP irán a ese dominio) y, si es
de una empresa, en su inquilino:

```sh
jmap '["x:Account/set",{"create":{"a":{
  "@type":"User","name":"scim-idp","domainId":"ID_DOMINIO","memberTenantId":"ID_INQUILINO",
  "description":"Aprovisionamiento SCIM (Entra ID)","roles":{"@type":"User"},
  "permissions":{"@type":"Merge","enabledPermissions":{"scimAccess":true,"sysAccountGet":true,
    "sysAccountCreate":true,"sysAccountUpdate":true,"sysAccountDestroy":true},"disabledPermissions":{}},
  "credentials":{"0":{"@type":"Password","secret":"UNA-CONTRASEÑA-LARGA"}}
}}},"0"]'
```

- Tiene el rol `User` porque quien crea o cambia una cuenta debe tener **todos** los permisos de esa cuenta.
- Para «suspender pero nunca borrar», quitad `sysAccountDestroy`.
- Si el IdP también da de alta administradores, la cuenta necesita sus permisos (p. ej. `roles: Admin`).

### 1.4 La clave API (el «token» que se pega en el IdP)

La cuenta de aprovisionamiento crea su propia clave entrando con su contraseña (o el administrador con
`accountId`):

```sh
curl -s -u 'scim-idp@empresa.pe:UNA-CONTRASEÑA-LARGA' -H 'Content-Type: application/json' "$S/jmap/" -d '{
 "using":["urn:ietf:params:jmap:core","urn:stalwart:jmap"],
 "methodCalls":[["x:ApiKey/set",{"create":{"k":{"description":"Entra ID",
   "permissions":{"@type":"Inherit"},"allowedIps":{"20.0.0.0/8":true}}}},"0"]]}'
```

El `secret` de la respuesta **solo se muestra una vez**. `allowedIps` y `expiresAt` son opcionales (si se ponen,
hay que mantenerlos). Para rotar: crear otra clave, cambiarla en el IdP y borrar la vieja.

### 1.5 Comprobar

```sh
curl -s "$S/scim/v2/ServiceProviderConfig"                       # sin credencial: 200
curl -s -H "Authorization: Bearer $TOKEN" "$S/scim/v2/Users?count=1"   # 200 y una ListResponse
```

Un 401 es el token; un 403, un permiso (el `detail` dice cuál). Basic no sirve: siempre `Bearer`.

Datos para el IdP: **URL base** `https://SU-SERVIDOR/scim/v2` y el **token**.

## 2. Microsoft Entra ID

1. Entra admin center → *Enterprise apps* → *New application* → *Create your own application* → «Integrate any
   other application you don't find in the gallery».
2. *Provisioning* → modo **Automatic**: *Tenant URL* = la URL base, *Secret Token* = la clave API (no lo dejéis
   vacío). *Test Connection* (pide un usuario inventado y espera una lista vacía).
3. *Mappings* → usuarios:
   - `userPrincipalName` → `userName` (debe ser la **dirección de correo** en un dominio abierto; si el UPN no lo
     es, usad `mail`).
   - `displayName` → `displayName`; `mailNickname` (o `objectId`) → `externalId`; `Switch([IsSoftDeleted]…)` →
     `active` (dejadlo: es lo que suspende).
   - **Borrad** `mail` → `emails[type eq "work"].value`: aquí todas las direcciones son `work` y ese mapeo falla
     con alias (o renombra la cuenta si no tiene).
   - El resto (`title`, teléfonos, extensión empresarial…) se acepta y se descarta; mejor quitarlo para no
     confundir.
4. Grupos: `displayName`, `members`, `objectId` → `externalId`. Su dirección será `nombre-del-grupo@` el dominio
   de la cuenta de aprovisionamiento.
5. *Users and groups*: asignad a quien se tenga que aprovisionar. Probad con *Provision on demand* y luego
   arrancad el ciclo (cada ~40 min).
6. Entra manda `PATCH` con `"op":"Replace"` y `"active":"False"` (como cadena): está soportado. Desasignar o
   bloquear a alguien lo **suspende** (deja de entrar, conserva el buzón y sigue recibiendo correo); el `DELETE`
   llega cuando se borra del todo en Entra (tras su periodo de retención).
7. Activad *accidental deletions prevention* en *Properties* antes del primer ciclo completo.

## 3. Okta

1. *Applications* → *Create App Integration* → SAML 2.0 o SWA (Okta no hace SCIM en integraciones OIDC: si los
   usuarios entran por OIDC, es otra aplicación aparte). En *General* → *Provisioning*: **SCIM**.
2. *Provisioning* → *Integration*: *SCIM connector base URL* = la URL base; *Unique identifier field for users* =
   `userName`; acciones: *Push New Users*, *Push Profile Updates* y, si hay grupos, *Push Groups*; *Authentication
   Mode* = **HTTP Header** con la clave API (Okta añade `Bearer`). *Test Connector Configuration*.
3. *To App*: activad *Create Users*, *Update User Attributes* y *Deactivate Users*. **No** activéis *Sync
   Password* (la contraseña que Okta manda al crear se descarta).
4. *Profile Editor*: `userName` debe ser la dirección de correo (si no, mapead `email` → `userName`); mapead
   `displayName` (si no, se forma con nombre + apellido).
5. Okta busca siempre por `userName eq "..."`, actualiza con `PUT` completo (los alias que no mande se quitan:
   gestionadlos en Okta) o con `PATCH` sin `path`, y **nunca borra**: desactivar/desasignar suspende. Los buzones
   de quien se fue se borran a mano en Stalwart (o con `DELETE /scim/v2/Users/{id}`).
6. *Push Groups*: el grupo que da acceso a la app y el que se empuja deben ser distintos. Quitar el *push* sí borra
   el grupo (`DELETE /Groups/{id}`).
7. Dejad desactivadas las *Imports* (Okta filtra por `meta.lastModified`, que no existe).

## 4. Keycloak

Keycloak (26.x) no trae cliente SCIM de salida. Opciones:

- **Extensión** de la comunidad (p. ej. `Termindiego25/keycloak-scim-outbound` o `mitodl/keycloak-scim`): URL base
  y la clave API como *bearer token* estático. Mandan `name.givenName`/`familyName` sin `displayName`: se forma el
  nombre con los dos. Probad con un usuario y mirad los eventos `scim.*` y el registro de la extensión: no
  reintentan si algo falla.
- **Sin SCIM**: crear las cuentas por la API JMAP (`x:Account/set`) desde un servicio que escuche los eventos de
  administración de Keycloak, o apuntar a Stalwart el cliente SCIM del sistema que está detrás de Keycloak.

Recordad: si Keycloak es también el directorio OIDC del dominio y el dominio tiene `allowScimProvisioning`, quien
entre antes de estar aprovisionado recibe un fallo de autenticación (no se crea la cuenta al vuelo).

## 5. Si algo falla

Cada petición deja eventos `scim.*` (`scim.request` con método, ruta, estado y tiempo; `scim.forbidden` para
403; `scim.conflict` para 409/412; `scim.error` para lo demás, con el `detail`). Los más habituales:

| el IdP ve | causa |
|---|---|
| 401 | token mal copiado, caducado, IP fuera de `allowedIps`, o Basic en vez de Bearer |
| 403 «lacks the 'scimAccess' permission» | falta `scimAccess` en la cuenta **o en el inquilino** (§1.1) |
| 403 «You are not authorized to grant permissions» | la cuenta de aprovisionamiento no tiene los permisos de los usuarios que gestiona (§1.3) |
| 403 «exceeded your quota» | cuota del inquilino |
| 400 `invalidValue` «not open to SCIM» | el dominio del `userName` no existe o no tiene `allowScimProvisioning` |
| 400 `invalidSyntax` | atributo mal escrito en un mapeo personalizado |
| 409 `uniqueness` | dirección, `externalId` o nombre de grupo ya usados |
