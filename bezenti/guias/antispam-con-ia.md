# Cómo activar el antispam con IA

Guía corta para el operador del servidor (edición bezenti). Detalle técnico: [`../especificaciones/ia.md`](../especificaciones/ia.md).

La IA **no sustituye** al filtro clásico: solo se le pregunta por los correos **dudosos** (los que el filtro no sabe
clasificar con seguridad) y su veredicto **se suma** a la puntuación. Si el modelo falla o tarda, el correo sigue el
camino de siempre: nunca se pierde ni se queda bloqueado.

Todo se hace en el panel (Settings › AI y Settings › Spam Filter › AI Classifier) o por la API JMAP como administrador.
Abajo van los objetos en JSON (para `x:AiModel/set` y `x:SpamAi/set`).

## 1. Elegir el modelo

### Opción A: Ollama en tu propio servidor (el correo no sale de casa)

```bash
ollama pull qwen3:8b          # en la máquina con GPU (o CPU, más lento)
```

```json
{"@type": "Ollama", "name": "spam-local", "model": "qwen3:8b",
 "url": "http://10.0.0.20:11434", "apiKey": {"@type": "None"},
 "logprobs": true, "reasoning": "Off", "keepAlive": 3600000, "timeout": 60000}
```

- `logprobs: true` da una probabilidad medida (Ollama reciente); si tu versión no la devuelve, no pasa nada: se usa la
  confianza que declara el modelo.
- `keepAlive` evita que el modelo se descargue entre correos (si no, cada correo paga el tiempo de carga).
- vLLM o llama.cpp: `"@type": "OpenAi"` con `"profile": "Vllm"` o `"LlamaCpp"` y su `url` (por ejemplo
  `http://10.0.0.12:8000/v1`).

### Opción B: un proveedor (Anthropic, OpenAI, Gemini)

La clave **no** se escribe en el objeto: se lee de una variable de entorno del servicio (o de un archivo).

```json
{"@type": "Anthropic", "name": "claude", "model": "claude-haiku-5-5",
 "apiKey": {"@type": "EnvironmentVariable", "variableName": "ANTHROPIC_API_KEY"},
 "rateLimit": {"count": 50, "period": 60000}, "maxConcurrentRequests": 8}
```

OpenAI: `"@type": "OpenAi", "profile": "OpenAi", "model": "gpt-…", "logprobs": true`. Gemini: `"@type": "Gemini"`.
Con un proveedor alojado, los datos del §3 salen hacia él: revisa su política de datos.

## 2. Activar en modo sombra (solo mirar)

Con el id del modelo creado (`x:SpamAi` es un singleton: `update` sobre `singleton`):

```json
{"@type": "Enable", "mode": "Shadow",
 "tiers": {"0": {"modelId": "<id del modelo>"}},
 "learn": {"@type": "Disable"}, "webSearch": {"@type": "Disable"}}
```

Después, **recargar la configuración** (panel o `x:Action/set` con `{"@type": "ReloadSettings"}`). Si un modelo está mal
configurado (URL, clave, `extraParameters`), la recarga se rechaza y sigue la configuración anterior.

Durante unos días, mira:

- la cabecera `X-Spam-AI` de los correos (con `; shadow`): `phishing; p=0.93; confidence=0.88; model=…; tier=1`;
- el registro: `ai.spam-verdict` (veredictos), `ai.api-error` y `ai.breaker-open` (problemas con el modelo);
- las métricas `ai.request-time`, `ai.input-tokens`, `ai.output-tokens` (latencia y coste).

Para probar un correo concreto sin entregarlo: `x:Action/set` con `{"@type": "ClassifySpam", "message": "<correo
RFC 5322>", "remoteIp": "192.0.2.25", "ehloDomain": "mail.ejemplo.com", "mailFrom": "a@ejemplo.com", "rcptTo":
{"b@tu-dominio.com": true}}`. La respuesta trae `aiGate` (si se consultó o por qué no), `aiFacts` (**exactamente** lo
que se envía al modelo) y `aiVerdict`.

## 3. Qué se envía al modelo

Nunca el correo en crudo. Solo un resumen: remitente y Reply-To, número y dominios de los destinatarios, resultados
SPF/DKIM/DMARC/ARC, asunto, si es respuesta, si tiene baja de lista, textos de enlaces con su host real, nombres y tipos
de adjuntos (no su contenido) y hasta `maxInputChars` (6000) caracteres del cuerpo sin citas ni firma. Se puede
recortar más con `includeAuth`, `includeLinks`, `includeAttachments`, `maxLinks`, `maxAttachments` y `trimQuotes`.

## 4. Puntuar de verdad

```json
{"mode": "Automatic"}
```

y recargar. Desde ahí las etiquetas `AI_*` cuentan: por ejemplo `AI_PHISHING_HIGH` +7, `AI_BULK_SPAM_HIGH` +5,
`AI_IMPERSONATION` +3, `AI_LEGITIMATE_HIGH` −3. Se cambian como cualquier etiqueta con `x:SpamTag`. Si ningún modelo
responde, se añade `AI_ERROR` (0 puntos) y el correo sigue con su puntuación clásica.

¿A qué correos se pregunta? A los que cumplen `condition` (por defecto, puntuación entre −2 y umbral + 6 y clasificador
estadístico dudoso). Ampliarla sube coste y latencia; nunca se pregunta por correo de usuarios autenticados.

## 5. Opcional

- **Segundo modelo para los difíciles**: un modelo pequeño primero y uno grande solo cuando el primero duda o ve
  phishing/malware:

  ```json
  {"tiers": {"0": {"modelId": "<pequeño>", "escalateBelow": 0.3, "escalateOn": {"phishing": true, "malware": true}},
             "1": {"modelId": "<grande>"}}}
  ```

- **Aprender de la IA**: `{"learn": {"@type": "Enable", "minProbability": 0.95, "sampleRate": 0.1}}` entrena el
  clasificador estadístico con una muestra de los veredictos más seguros.
- **Límite de tiempo**: `deadline` (10 s por defecto) es el máximo por correo, sumando niveles y reintentos.
- **Cortacircuitos**: tras `breakerThreshold` fallos seguidos (5) el modelo se salta durante `breakerCooldown` (1 min).

## 6. IA en los filtros Sieve de los usuarios

Los usuarios (rol con el permiso `interactAi`, incluido por defecto) pueden pedir al modelo que clasifique su correo:

```sieve
require ["fileinto", "mailbox", "vnd.stalwart.expressions"];
let "prompt" "'Responde solo con una de estas palabras: Trabajo, Familia, Bandeja. Asunto: ' + header.subject";
let "reply" "llm_prompt('spam-local', prompt, 0.2)";
if eval "contains_ignore_case(['Trabajo', 'Familia'], reply)" {
    fileinto :create "${reply}";
}
```

- Si el modelo falla, `llm_prompt` devuelve `false` y el correo se queda en la bandeja.
- Usa **`contains_ignore_case`** para comprobar la respuesta: `contains` compara como número y el `false` de un fallo
  «coincide» con cualquier palabra.
- Acepta solo respuestas de una lista fija: el remitente puede escribir instrucciones para el modelo.
- Para quitar la IA a un grupo de usuarios, quita `interactAi` de su rol.
