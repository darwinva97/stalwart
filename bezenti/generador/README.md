# Generador del registro (edición bezenti)

Escribe los seis archivos de `crates/registry/src/schema/` (`enums.rs`, `enums_impl.rs`, `properties.rs`,
`properties_impl.rs`, `structs.rs`, `structs_impl.rs`) a partir del esquema de la API
`resources/schema/schema.json.gz`. En Stalwart esos archivos los produce una herramienta que no es pública; al
limpiar la rama se perdieron las partes de los objetos de pago (unas 1.500 marcas `[fragmento SEL eliminado]`).
Este generador es nuestro, AGPL-3.0-only, y vuelve a producir los seis archivos completos: lo libre igual que
antes y lo de pago (multi-inquilino, IA, direcciones enmascaradas, archivo, telemetría…) tras
`#[cfg(feature = "bezenti")]`. `x:Enterprise` (licencias) no se genera.

## Por qué Python

Python 3 sin dependencias externas (solo la biblioteca estándar) más el `rustfmt` de la cadena de Rust, que ya
está donde se compila Stalwart. Es un traductor de texto: no necesita compilarse con el workspace (un binario
Rust dentro del workspace obligaría a compilar el crate antes de poder regenerar el propio crate) y se lee y se
cambia fácil. El formato final lo pone `rustfmt`, igual que en el código original.

## Archivos

| archivo | qué es |
|---|---|
| `generar.py` | programa principal: `--modo bezenti` (por defecto, escribe en el crate) o `--modo fidelidad` |
| `modelo.py`, `reglas.py` | el modelo (esquema + perfil + decisiones) y las reglas esquema → código |
| `gen_enums.py`, `gen_propiedades.py`, `gen_structs.py` | las plantillas de cada archivo |
| `perfil.py` → `perfil.json` | datos que el esquema no tiene, extraídos del código AGPL de la rama limpia |
| `decisiones.json` | lo que ni el esquema ni el código libre dicen de lo de pago, decidido y razonado por nosotros |
| `asignaciones.json` | identificadores numéricos persistidos de lo de pago (no se cambian nunca) |
| `extensiones.json` | campos propios de la edición bezenti que el esquema oficial no tiene (fase 2: `x:Tenant.bezentiParentTenantId`, revendedores; fase 8: `bezentiMaxQueuedMessages`, `bezentiMessageRate`, `bezentiRecipientRate`, límites de cola y de envío); solo en modo bezenti |
| `gen_inquilino.py` | (fase 2) escribe `crates/registry/src/bezenti/inquilino_gen.rs`: leer/escribir `memberTenantId` de cualquier objeto |
| `gen_esquema.py` | (fase 2) escribe `resources/schema/schema_bezenti.json.gz` (+ `.sha256`), el esquema que sirve `/api/schema`: completo, sin `x:Enterprise` y con las extensiones |
| `comprobar.sh` | prueba de fidelidad (la lanza `bezenti/pruebas.sh` en la CI); también comprueba que los archivos de `gen_inquilino.py` y `gen_esquema.py` están al día |

### El perfil

El esquema describe la API, no el código: faltan los **identificadores numéricos** que se guardan en disco
(tipos de objeto, propiedades, variantes de enums, posición de cada campo en el formato binario y su bit),
el orden de los campos, los validadores de cadenas, los índices, los grupos de variables de las expresiones,
los indicadores (`FLAGS`) de cada objeto y algunos detalles de estilo. `perfil.py` los saca **como datos**
(nombres y números, nunca código) de los seis archivos de la rama `limpio-upstream`, que es Stalwart sin código
SEL. Las plantillas del generador son nuestras.

### Lo de pago

Lo que no está en el código limpio se genera con las mismas reglas que lo libre y:

- **Identificadores**: los nuevos ocupan los huecos de la numeración libre (números que el código libre nunca
  usará) y si no quedan, el siguiente libre; se guardan en `asignaciones.json` y no cambian al regenerar.
  No pretenden coincidir con los de la edición Enterprise: los datos de pago de esta edición son nuestros.
- **Formato binario**: un campo de pago opcional o booleano ocupa un hueco de campo y de bit; uno obligatorio
  va en su propio bloque (en el código libre ya hay ahí un bloque vacío de relleno, así que las dos
  compilaciones se entienden).
- **Indicadores de objeto**: las reglas que cumplen todos los objetos libres (singleton, `memberTenantId` →
  `OBJ_FILTER_TENANT`, `accountId` en todas las variantes → `OBJ_FILTER_ACCOUNT`, sin permiso → `OBJ_NO_*`) más
  `decisiones.json`.
- **Índices, validadores, contextos de expresión**: reglas por formato y tipo, más `decisiones.json`. Para los
  objetos nuevos, `indice_campos` dice qué campo alimenta cada índice declarado (p. ej. el nombre único de
  `x:Tenant`).
- **Extensiones propias** (`extensiones.json`): se añaden al esquema antes de generar, marcadas como de pago, así
  que siguen las mismas reglas (identificadores en `asignaciones.json`, `cfg(feature = "bezenti")`). Llevan el
  prefijo `bezenti` para no chocar nunca con un campo futuro del esquema oficial.

## Uso

```sh
# Regenerar los seis archivos (modo bezenti) en crates/registry/src/schema/
python3 bezenti/generador/generar.py

# Regenerar lo de la fase 2 (accesores de memberTenantId y esquema servido)
python3 bezenti/generador/gen_inquilino.py
python3 bezenti/generador/gen_esquema.py

# Prueba de fidelidad completa
bash bezenti/generador/comprobar.sh
```

Hace falta `rustfmt` (`rustup component add rustfmt`).

## Prueba de fidelidad

`comprobar.sh`:

1. vuelve a extraer el perfil de `limpio-upstream` y comprueba que es `perfil.json`;
2. genera en **modo fidelidad** (esquema completo filtrando lo de pago, con la cabecera y el
   `not(feature = "enterprise")` originales) y compara línea a línea con los archivos de `limpio-upstream`
   quitando solo las líneas de marca;
3. comprueba que los seis archivos del crate son exactamente la salida del generador en modo bezenti.

En modo fidelidad lo de pago se genera entre centinelas y se quita **después** de pasar `rustfmt`; así el
formato de lo que queda es el mismo que tenía el original (que se formateó con lo de pago dentro). En unos 59
bloques `impl` el código limpio termina en una línea en blanco porque detrás había código de pago que no se
corresponde con nada del esquema; el perfil guarda dónde (`huecos`) y el modo fidelidad deja esa línea.

Resultado con Stalwart v1.1.0: **98.004 líneas comparadas, 0 distintas** (1.482 marcas SEL eliminadas).

## Cambiar de versión de Stalwart

1. Limpiar la versión nueva con `bezenti/sala-limpia.py`, hacer commit en `limpio-upstream` y fusionarla en
   `bezenti` (ver `bezenti/README.md`). En el conflicto de los seis archivos generados, quedarse con lo nuestro:
   se van a regenerar.
2. Extraer el perfil de la versión limpia:
   ```sh
   git show limpio-upstream:crates/registry/src/schema/<archivo>   # los seis, en un directorio temporal
   python3 bezenti/generador/perfil.py <dir> resources/schema/schema.json.gz bezenti/generador/perfil.json
   ```
3. `bash bezenti/generador/comprobar.sh`. Si la fidelidad falla, la versión nueva trae un patrón que las
   plantillas no conocen: mirar el diff y ajustar `reglas.py`/`gen_*.py` (o extraer el dato nuevo en
   `perfil.py`) hasta llegar a 0 líneas distintas.
4. `python3 bezenti/generador/generar.py`: regenera los archivos del crate y asigna identificadores a lo de pago
   nuevo (`asignaciones.json` crece; los antiguos no cambian). Si el esquema trae objetos o campos de pago
   nuevos que necesiten índices, validadores o contextos de expresión, el generador lo dice; se deciden en
   `decisiones.json`.
5. Compilar con y sin `bezenti`, `bash bezenti/pruebas.sh`, commit de todo (incluidos `perfil.json` y
   `asignaciones.json`).
