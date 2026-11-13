# Cómo dar de alta una empresa (por la API)

Guía corta para el operador del servidor. Cada empresa cliente es un **inquilino**: tiene sus dominios, sus
cuentas y su administrador, y no ve a las demás. Todo se hace con la API JMAP de administración
(`POST https://TU-SERVIDOR/jmap/`), la misma que usa el panel web. Los detalles están en
[`../especificaciones/multi-inquilino.md`](../especificaciones/multi-inquilino.md).

En los ejemplos: `$S` es la URL del servidor y `$OP` es «usuario:contraseña» de una cuenta de administrador del
servidor (una cuenta **sin** inquilino con `roles: Admin`). Todas las peticiones llevan esta cabecera de
capacidades:

```sh
U='"using":["urn:ietf:params:jmap:core","urn:stalwart:jmap"]'
jmap() { curl -s -u "$OP" -H 'Content-Type: application/json' "$S/jmap/" -d "{$U,\"methodCalls\":[$1]}"; }
```

Los identificadores que devuelve cada paso (`"id": "..."`) se usan en el siguiente.

## 1. Crear el inquilino

```sh
jmap '["x:Tenant/set",{"create":{"t":{
  "name":"Ferretería López",
  "quotas":{"maxAccounts":25,"maxDomains":2,"maxDiskQuota":53687091200}
}}},"0"]'
```

- `quotas` (todas opcionales; sin clave = sin límite): `maxAccounts` (usuarios), `maxGroups`, `maxDomains`,
  `maxMailingLists`, `maxRoles`, `maxOauthClients`, `maxDkimKeys`, `maxDnsServers`, `maxDirectories`,
  `maxAcmeProviders` y `maxDiskQuota` (bytes; 53687091200 = 50 GiB, compartidos por todas sus cuentas).
- `logo` (opcional): URL https, `data:image/png;base64,...` o base64. Se verá en `https://SU-DOMINIO/logo`.
- Sin `roles` ni `permissions`, el inquilino puede lo que da el rol **Tenant Administrator** (dominios, cuentas,
  grupos, listas, roles, DKIM…), nunca la configuración del servidor.
- Límites de envío (opcionales, propios de esta edición; sin poner = sin límite), para que una empresa no acapare
  la cola ni queme la reputación de la IP de todos:
  `"bezentiMessageRate":{"count":500,"period":3600000}` (mensajes por hora; el periodo va en milisegundos),
  `"bezentiRecipientRate":{"count":2000,"period":3600000}` (destinatarios por hora) y
  `"bezentiMaxQueuedMessages":200` (mensajes suyos a la vez en la cola de salida). Al pasarse, sus envíos se
  aplazan con `451 4.7.1` o `452 4.3.1` y el programa de correo los reintenta. Aunque no pongas límites, la entrega
  reparte los hilos entre empresas: un envío masivo de una no retrasa el correo de las demás.
- Su administrador ve y gestiona su propia cola de salida (reintentar, cancelar), sus informes DMARC/TLS y las
  cuentas que borró mientras se puedan recuperar (si `archiveDeletedAccountsFor` está puesto).

## 2. Su dominio

```sh
jmap '["x:Domain/set",{"create":{"d":{"name":"ferreterialopez.pe","memberTenantId":"ID_INQUILINO"}}},"0"]'
```

Con `x:Domain/get` (propiedad `dnsZoneFile`) se obtienen los registros DNS (MX, SPF, DKIM, DMARC…) que la
empresa debe publicar. Las firmas DKIM se crean solas y quedan en su inquilino.

## 3. Su administrador

```sh
jmap '["x:Account/set",{"create":{"a":{
  "@type":"User","name":"admin","domainId":"ID_DOMINIO","memberTenantId":"ID_INQUILINO",
  "roles":{"@type":"Admin"},
  "credentials":{"0":{"@type":"Password","secret":"una-contraseña-larga-y-segura"}}
}}},"0"]'
```

`roles: Admin` en una cuenta de un inquilino la convierte en administradora **de ese inquilino y nada más**.
Desde aquí la empresa se gestiona sola: `admin@ferreterialopez.pe` entra al panel (o usa la API) y crea sus
cuentas, alias, grupos y listas; todo lo que crea queda en su inquilino automáticamente.

## 4. Comprobar

```sh
jmap '["x:Tenant/get",{"ids":["ID_INQUILINO"],"properties":["name","usedDiskQuota","quotas"]},"0"]'
jmap '["x:Account/query",{"filter":{"memberTenantId":"ID_INQUILINO"}},"0"]'
```

## Revendedores

Para que una empresa pueda dar de alta a sus propios clientes (inquilinos hijos), amplíale los permisos:

```sh
jmap '["x:Tenant/set",{"update":{"ID_INQUILINO":{"permissions":{"@type":"Merge",
  "enabledPermissions":{"sysTenantGet":true,"sysTenantCreate":true,"sysTenantUpdate":true,
                        "sysTenantDestroy":true,"sysTenantQuery":true},
  "disabledPermissions":{}}}}},"0"]'
```

Su administrador hará los pasos 1 a 3 con su propio usuario; los inquilinos que cree quedan debajo del suyo
(`bezentiParentTenantId`), no ve a sus hermanos ni a su padre, y todo lo de sus clientes cuenta para **sus**
cuotas: nunca puede repartir más de lo que tiene.

## Dar de baja

Primero se borran (o se mueven) sus cuentas, listas, dominios e inquilinos hijos; después el inquilino:

```sh
jmap '["x:Tenant/set",{"destroy":["ID_INQUILINO"]},"0"]'
```

Mientras le queden objetos, el servidor responde `objectIsLinked` con la lista de lo que falta.
