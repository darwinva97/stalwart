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
| 1 | Generador propio del registro a partir del esquema (structs, enums, propiedades) para los objetos de pago, tras la feature `bezenti` | pendiente |
| 2 | **Multi-inquilino**: inquilinos, dominios y cuentas por inquilino, administradores limitados a su inquilino, cuotas y límites por inquilino, cola SMTP por inquilino, logo por inquilino; extensión propia: **revendedores** (inquilinos con subinquilinos) | pendiente |
| 3 | Direcciones enmascaradas; archivo, retención y recuperación de borrados | pendiente |
| 4 | IA: modelos (OpenAI, Anthropic, Gemini, Ollama), clasificación antispam, `llm_prompt` en Sieve | pendiente |
| 5 | Historial de métricas y trazas, alertas, telemetría en vivo | pendiente |
| 6 | Réplicas de lectura y almacenes repartidos | pendiente |
| 7 | SCIM 2.0 (aprovisionamiento desde Entra ID, Okta…) | pendiente |
