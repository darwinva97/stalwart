# IA: modelos, antispam con IA y `llm_prompt` en Sieve (fase 4)

**Fuentes** (todas permitidas por la sala limpia):

- **[doc]** documentación pública de Stalwart: [AI models](https://stalw.art/docs/server/ai-models/),
  [AI classifier](https://stalw.art/docs/spamfilter/ai/), [Enabling the AI classifier](https://stalw.art/docs/spamfilter/ai/enable/),
  [LLM Integration (Sieve)](https://stalw.art/docs/sieve/llm/).
- **[esquema]** `resources/schema/schema.json.gz`: objetos `x:AiModel` (5 variantes), `x:SpamAi`, `x:SpamAiTier`,
  `x:SpamAiCategory`, `x:SpamAiSignal`, `x:SpamAiLearn`, enums `AiReasoning`, `AiOpenAiProfile`, `SpamAiMode`,
  eventos/métricas `ai.*`, claves de traza, permisos y la variable de expresión `SPAM_AI_VARIABLE`.
- **[API]** documentación pública de cada proveedor: OpenAI Chat Completions, Anthropic Messages, Gemini
  `generateContent`, Ollama `/api/chat`, TypeSafe `/v1/systemone`.
- **[bezenti]** decisión nuestra donde las fuentes no dicen nada; se explica el porqué.

Código: `crates/trc/src/event/ia.rs`, `crates/common/src/bezenti/ia/`, `crates/spam-filter/src/bezenti/`, todo tras la
feature `bezenti`. Pruebas: `bezenti/pruebas/ia.py` (CI) y `bezenti/pruebas/ia_real_anthropic.py` (manual).

## 1. `x:AiModel` — un modelo

Objeto múltiple **[esquema]** (`sysAiModel{Get,Create,Update,Destroy,Query}`; panel: Settings › AI **[doc]**).

| campo | variantes | notas |
|---|---|---|
| `name` | todas | nombre corto; con él lo nombra `llm_prompt` **[doc]**; único **[bezenti, fase 1]** |
| `model` | todas | identificador que se envía al proveedor |
| `url` | todas | URL base; vacía = la del proveedor (§2). Se añade la ruta salvo que la URL ya termine en ella **[doc]** |
| `apiKey` | todas | `x:SecretKeyOptional`: `None`, `Value`, `EnvironmentVariable`, `File` **[doc]**; nunca se registra **[bezenti]** |
| `httpHeaders` | todas | cabeceras extra en cada petición |
| `allowInvalidCerts` | todas | acepta certificados autofirmados |
| `timeout` | todas | 60 s; tiempo total de una llamada de `llm_prompt` (cola, espera de ritmo y un reintento); la conexión se limita a 10 s; el antispam usa `x:SpamAi.deadline` **[doc]** |
| `maxConcurrentRequests` | todas | [1–4096], 16; las demás esperan hueco hasta su plazo y si no, «Concurrency or rate limit reached» **[doc]** |
| `rateLimit` | todas | `count` por `period`, repartidas uniformemente; vacío = sin límite **[doc]** |
| `maxOutputTokens` | generativos | [1–1.000.000], 4096; incluye los de razonamiento **[doc]** |
| `temperature` | generativos | [0–2], vacío = no se envía **[doc]** |
| `reasoning` | generativos | `Default`, `Off`, `Low`, `Medium`, `High` (traducción por proveedor en §2, **[bezenti]**) |
| `extraParameters` | generativos | objeto JSON que se mezcla en cada petición; si no es un objeto: «Extra parameters must be a JSON object» **[doc]**. La mezcla es recursiva en objetos **[bezenti]** |
| `logprobs` | OpenAi, Gemini, Ollama | usar las probabilidades de los tokens como confianza del clasificador **[doc]** |
| `keepAlive` | Ollama | cuánto tiempo sigue cargado el modelo **[doc]** |
| `profile` | OpenAi | `OpenAi`, `Azure`, `Vllm`, `LlamaCpp`, `Generic` **[esquema]** |

Errores de carga **[doc]**: «Invalid AI model URL», «Unable to obtain AI model API key», «Extra parameters must be a JSON
object». Al arrancar, el modelo queda sin cargar (los demás funcionan); en una recarga (`x:Action` `ReloadSettings`) la
recarga se rechaza y sigue la configuración anterior. **[bezenti]**: también «Invalid HTTP header name».

**Multi-inquilino [esquema]**: ni `x:AiModel` ni `x:SpamAi` tienen `memberTenantId`; son configuración del operador. Un
administrador de inquilino no los ve ni los toca (fase 2: tipos de objeto no permitidos a inquilinos). Los usuarios de
cualquier inquilino usan los modelos del operador desde Sieve si su rol tiene `interactAi`.

**Costes [esquema]**: el esquema no prevé presupuestos ni límites de coste; los controles son `maxConcurrentRequests`,
`rateLimit`, `timeout`, la compuerta del antispam y el permiso `interactAi` **[doc]**. Los tokens de cada petición quedan
en `ai.request` y en las métricas `ai.input-tokens`/`ai.output-tokens` para calcular el gasto.

## 2. Proveedores

| `@type` | API | URL por defecto | autenticación |
|---|---|---|---|
| `OpenAi` | Chat Completions y compatibles (`/chat/completions`) | `https://api.openai.com/v1` | `Authorization: Bearer` (`api-key` con perfil Azure) |
| `Anthropic` | Messages (`/v1/messages`) | `https://api.anthropic.com` | `x-api-key` + `anthropic-version: 2023-06-01` |
| `Gemini` | `/models/{model}:generateContent` | `https://generativelanguage.googleapis.com/v1beta` | `x-goog-api-key` |
| `Ollama` | `/api/chat` | `http://localhost:11434` | `Authorization: Bearer` |
| `TypeSafe` | `/v1/systemone` (solo antispam) | `https://api.typesafe.ai` | `Authorization: Bearer` |

Tabla **[doc]**; rutas de Anthropic, Gemini, Ollama y TypeSafe **[API]**. Los perfiles `Azure`, `Vllm` y `LlamaCpp`
necesitan `url` (por defecto apuntan a OpenAI) **[doc]**; la consulta de la URL (`?api-version=…` de Azure) se conserva
**[bezenti]**.

Traducción de cada dialecto **[API]**, con las elecciones **[bezenti]** marcadas:

- **OpenAi**. Mensajes `system` + `user`. Límite: `max_completion_tokens` (perfiles OpenAi, Azure) o `max_tokens` (Vllm,
  LlamaCpp, Generic) **[doc: el perfil elige el parámetro]**. Salida estructurada: `response_format` `json_schema`
  estricto (OpenAi, Azure, Vllm), `json_schema` del servidor llama.cpp (LlamaCpp), `response_format` `json_object`
  con el esquema en las instrucciones (Generic) **[bezenti]**. `reasoning` **[bezenti]**: `Low/Medium/High` →
  `reasoning_effort`; `Off` → `reasoning_effort: "none"` (OpenAi, Azure) o `chat_template_kwargs.enable_thinking:
  false` (Vllm, LlamaCpp: plantillas tipo Qwen3); en Generic `Off` no envía nada. `logprobs: true`, `top_logprobs: 5`.
  Respuesta: `choices[0].message.content`, `finish_reason`, `usage.prompt_tokens`, `completion_tokens`,
  `prompt_tokens_details.cached_tokens`, `choices[0].logprobs.content`.
- **Anthropic**. `system`, `messages`, `max_tokens`. Salida estructurada con `output_config.format` (`json_schema`).
  `reasoning` **[bezenti]**: `Low/Medium/High` → `output_config.effort`; `Default` y `Off` no envían nada (los modelos
  actuales no admiten desactivar el razonamiento; los antiguos no razonan por defecto). `temperature` solo si está
  configurada en el modelo (los modelos actuales la rechazan). Sin `logprobs`. Respuesta: bloques `text` (los
  `thinking` se ignoran), `stop_reason` (`refusal` = error), `usage.input_tokens`, `output_tokens`,
  `cache_read_input_tokens`.
- **Gemini**. `systemInstruction`, `contents`, `generationConfig.maxOutputTokens`, `temperature`, `responseMimeType:
  application/json` + `responseJsonSchema`, `responseLogprobs` + `logprobs: 5`. `reasoning` **[bezenti]**: `Off` →
  `thinkingConfig.thinkingBudget: 0`; `Low/Medium/High` → `thinkingConfig.thinkingLevel` (`LOW`/`MEDIUM`/`HIGH`,
  Gemini 3). Las partes con `thought: true` no cuentan. `promptFeedback.blockReason` = error.
- **Ollama**. `messages`, `stream: false`, `options.num_predict`, `options.temperature`, `format` (esquema),
  `keep_alive` (`"<n>s"`), `logprobs` + `top_logprobs`. `reasoning` **[bezenti]**: `Off` → `think: false`,
  `Low/Medium/High` → `think: true`. Respuesta: `message.content`, `done_reason`, `prompt_eval_count`, `eval_count`.
- **TypeSafe** (clasificador alojado, no generativo). `llm_prompt` con TypeSafe devuelve `false` y registra «Operation not
  supported by this model» **[doc]**. Petición **[API]** + **[bezenti]**: `state` = registro de hechos, pregunta
  `category` de tipo `choice` (criterios = categorías: nombre → descripción y ejemplos, instrucciones = `instructions`
  de `x:SpamAi`) y una `noul` por señal (`signal_<nombre>`). Probabilidad = `probabilities[elección]`, confianza =
  `confidence`, señal presente si `noul ≥ 0,5`.

## 3. Límites, reintentos y fallos

- Plazo total por llamada: `timeout` (Sieve) o el resto de `deadline` (antispam); cubre la cola de concurrencia, la
  espera del límite de ritmo y **un** reintento **[doc]**. Conexión ≤ 10 s **[doc]**.
- Reintento **[bezenti]**: uno, ante error de red, 408, 409, 429, 500, 502, 503, 504 y 529, tras `Retry-After` (si cabe en
  el plazo) o una espera aleatoria de 200–600 ms; nunca si no queda plazo.
- Cortacircuitos **[doc]**: se abre tras 5 fallos seguidos, o al instante con 401, 402, 403 o un error de cuota/saldo;
  durante 60 s se salta el modelo y se registra `ai.breaker-open`. El antispam tiene el suyo por nivel
  (`breakerThreshold`, `breakerCooldown`). **[bezenti]**: «cuota/saldo» = mensaje del proveedor con `quota`,
  `insufficient_quota`, `billing`, `credit balance` o `spending limit`; el estado vive en memoria y se reinicia al
  recargar la configuración.
- Un fallo de modelo **nunca** bloquea ni pierde el correo **[doc]**.

## 4. Clasificador antispam con IA (`x:SpamAi`)

Singleton múltiple `Disable` | `Enable` (`x:SpamAiProperties`) **[esquema]**; panel: Settings › Spam Filter › AI
Classifier **[doc]**.

### 4.1 Dónde se ejecuta [doc]

Tarde: después de los análisis integrados, Pyzor, el clasificador estadístico y las reglas que no dependen de etiquetas
`AI_`; antes de las reglas que sí (directamente o a través de etiquetas emitidas por otras reglas así; las que construyen
el nombre de la etiqueta en tiempo de ejecución no se detectan). El reparto de reglas ya estaba en el código libre
(`rules_after_ai`); el enganche es `crates/spam-filter/src/analysis/score.rs` con la puntuación provisional.

### 4.2 Compuerta [doc]

En orden; el primer motivo que aplica salta el mensaje y se registra en `ai.spam-skipped`: `authenticated` (sesión
autenticada), `decided` (una etiqueta de rechazo/descarte ya decidió), `condition` (la expresión `condition` da falso),
`unavailable` (todos los modelos con el cortacircuitos abierto), `no-content` (sin texto ni adjuntos). Si ninguno, `run`.
`condition` usa las variables de `SPAM_AI_VARIABLE` **[esquema]**: `spam_score` (puntuación provisional),
`spam_threshold` (umbral de spam), `spam_probability` (estadístico, `-1` si no corrió) y las del mensaje. Por defecto:
`spam_score > -2 && spam_score < spam_threshold + 6 && (spam_probability < 0 || (spam_probability > 0.15 &&
spam_probability < 0.85))` — solo los dudosos.

### 4.3 Hechos que se envían [doc] (nombres [bezenti])

El modelo nunca ve el mensaje en crudo. Recibe como mensaje del usuario un JSON `{"email": {…}}` con: `from` (`name`,
`address`, `domain`), `envelopeFrom`, `replyTo`, `recipients` (`count`, hasta 10 `domains`), `authentication` (`spf`,
`dkim`, `dmarc`, `arc`, `dkimPassDomains`; con `includeAuth`), `subject`, `isReply` (`In-Reply-To`/`References`),
`listUnsubscribe`, `links` (texto visible y host de destino real tras seguir redirectores; con `includeLinks`, hasta
`maxLinks`), `attachments` (`name`, `contentType`; con `includeAttachments`, hasta `maxAttachments`), `body` (sin citas
ni firma con `trimQuotes`, recortado a `maxInputChars` caracteres) y `bodyTruncated`. Es exactamente el `aiFacts` de
`ClassifySpam` **[doc]**.

### 4.4 Instrucciones, respuesta, probabilidad y confianza

- Instrucciones (system prompt): `instructions` **[esquema]** + lista de categorías (nombre, descripción, ejemplos) +
  señales (pregunta) + formato de respuesta **[bezenti]**.
- Respuesta **[bezenti]**: `{"category": "<nombre>", "confidence": "high|medium|low", "signals": {"<señal>": bool},
  "explanation": "…"}` (explicación solo con `explanation` **[doc]**), pedida con salida estructurada (esquema JSON con
  las categorías como `enum`). Se tolera texto o ```` ```json ```` alrededor. Categoría desconocida o JSON inválido =
  fallo del nivel.
- Probabilidad `p` **[doc]**: con `logprobs`, de las probabilidades de los tokens; sin ellas, de la confianza declarada
  (`verbalHigh` 0,9, `verbalMedium` 0,7, `verbalLow` 0,55). **[bezenti]**: con logprobs, `p = exp(Σ logprob)` de los
  tokens del nombre de la categoría y **confianza** = margen entre el token elegido y la mejor alternativa en el primer
  token del nombre; sin logprobs, confianza = `2p − 1` (margen frente a una única alternativa: alta 0,8, media 0,4,
  baja 0,1). En TypeSafe, `p` y la confianza son las del proveedor.

### 4.5 Etiquetas [doc]

`AI_<CATEGORÍA>_HIGH` si `p ≥ bucketHigh` (0,85), `_MEDIUM` si `p ≥ bucketMedium` (0,65), `_LOW` si no;
`AI_UNCERTAIN` en lugar de la de categoría si la confianza `< uncertainBelow` (0,3); `AI_<SEÑAL>` si la respuesta es sí;
`AI_ERROR` si ningún modelo da veredicto. Nombre en mayúsculas con `_` **[bezenti]** (`bulk_spam` → `BULK_SPAM`).

### 4.6 Puntuaciones por defecto

**[doc]**: `AI_PHISHING_HIGH` 7, `AI_BULK_SPAM_HIGH` 5, `AI_LEGITIMATE_HIGH` −3, `AI_IMPERSONATION` 3, `AI_UNCERTAIN` 0,
`AI_ERROR` 0. **[bezenti]** (el resto, para que las categorías por defecto tengan efecto): `AI_PHISHING_MEDIUM` 4,
`_LOW` 1,5; `AI_MALWARE_HIGH` 7, `_MEDIUM` 4, `_LOW` 1,5; `AI_BULK_SPAM_MEDIUM` 3, `_LOW` 1;
`AI_UNSOLICITED_SALES_HIGH` 2,5, `_MEDIUM` 1,5, `_LOW` 0,5; `AI_LEGITIMATE_MEDIUM` −1,5, `_LOW` −0,5;
`AI_TRANSACTIONAL_HIGH` −2, `_MEDIUM` −1; `AI_NEWSLETTER_HIGH` −1, `_MEDIUM` −0,5; `AI_CREDENTIAL_REQUEST` 1,5;
`AI_REQUESTED` −1. Solo se aplican si `x:SpamAi` está activado y ningún `x:SpamTag` define la etiqueta (un `x:SpamTag`
manda siempre). Categorías y señales propias no tienen puntuación hasta que se les dé **[doc]**.

### 4.7 Niveles y plazo [doc]

`tiers` en orden; el primero se consulta para todo mensaje que pasa la compuerta. Se pasa al siguiente si el veredicto
es poco concluyente (confianza `< escalateBelow` (0,3), incierto, `p` en el cubo bajo o categoría en `escalateOn`) o si
el nivel falló. Vale el veredicto del último nivel que respondió. Todas las peticiones de un mensaje comparten
`deadline` (10 s); lo que siga en curso al vencer, falla. Un nivel con el cortacircuitos abierto se salta.

### 4.8 Modo, cabecera, aprendizaje [doc]

- `Shadow`: se consulta y se registra, pero no se añaden etiquetas ni se aprende; la puntuación no cambia.
- `X-Spam-AI` (con `addHeader`, entre `X-Spam-Result` y `X-Spam-Score`): `phishing; p=0.93; confidence=0.88;
  model=<id del proveedor>; tier=1` + `; uncertain` + `; tier1=<cat>/<p>` por nivel anterior + `; shadow` +
  `; reason="…"`. Error: `error; tier=<n>; error="<descripción>"`. **[bezenti]**: la razón se recorta a 200 caracteres
  sin saltos de línea y con comillas escapadas.
- Aprendizaje (`learn` = `Enable`): muestra para el clasificador estadístico solo si modo Automatic, no incierto,
  `p ≥ minProbability` (0,95), probabilidad estadística entre 0,15 y 0,85, remitente no autenticado y sin
  `TRUSTED_DOMAIN`, la primera etiqueta AI tiene puntuación no nula (su signo da la etiqueta, que debe coincidir con la
  clasificación final) y sorteo con `sampleRate` (0,1).
- `webSearch`: el esquema solo tiene `Disable` **[esquema]**; no se implementa nada más.

### 4.9 `ClassifySpam` [doc]

`x:Action` `ClassifySpam` clasifica sin entregar y devuelve `aiGate` (`run` o el motivo), `aiFacts` (el JSON enviado) y
`aiVerdict` (formato de `X-Spam-AI`, también con `addHeader` desactivado **[bezenti]**).

## 5. Sieve: `llm_prompt(modelo, prompt, temperatura)` [doc]

Extensión `vnd.stalwart.expressions`, dentro de una expresión (`let`). Modelo por `name` (`""` elige el único si solo hay
uno). Prompt como texto, sin instrucciones de sistema; devuelve la respuesta como texto o `false` ante cualquier fallo
(modelo desconocido, sin permiso, tiempo, límites, cortacircuitos, error del proveedor, TypeSafe). Temperatura 0–1,
recortada al rango; si no es un número, manda la del modelo; el resto sale de `x:AiModel`. En scripts de usuario hace
falta `interactAi` (sin él: `false` y `security.unauthorized`); los scripts de sistema son de confianza. Los roles por
defecto User y System Administrator llevan `interactAi` (**[bezenti]**: también Tenant Administrator).

**[bezenti]**: el prompt se recorta a 100.000 caracteres; en Anthropic la temperatura del script solo se envía si el
modelo tiene temperatura configurada. Trampa de Sieve (no es nuestra): `contains([...], reply)` compara números con
texto, y el `false` de un fallo (0) es igual a cualquier texto no numérico; para comprobar la respuesta hay que usar
`contains_ignore_case([...], reply)`, que compara como texto (ver la guía).

## 6. Telemetría

Eventos **[esquema]** con nivel **[doc]**/**[bezenti]**:

| evento | nivel | claves |
|---|---|---|
| `ai.request` | debug | `model` (nombre), `id` (modelo según el proveedor), `elapsed`, `inputTokens`, `outputTokens`, `cachedTokens`, `finishReason`, `details` (id de la petición del proveedor) |
| `ai.llm-response` | trace | `model`, `contents` (primeros 4096 caracteres) |
| `ai.api-error` | warn | `model`, `reason`, `code` (estado HTTP); `details = "retrying"` en el intento que se reintenta |
| `ai.spam-verdict` | info | `category`, `probability`, `confidence`, `tier`, `model`, `tag` (etiquetas), `result` (`shadow`/`automatic`) |
| `ai.spam-skipped` | debug | `reason` (motivo de la compuerta) |
| `ai.breaker-open` | warn | `model`, `reason`, `retryAfter` (enfriamiento) |

Métricas **[esquema]**: contadores `ai.request`, `ai.api-error`, `ai.spam-verdict`, `ai.spam-skipped`,
`ai.breaker-open`; histogramas `ai.request-time`, `ai.input-tokens`, `ai.output-tokens` (de `ai.request`). Claves de traza
**[esquema]**: `model`, `inputTokens`, `outputTokens`, `cachedTokens`, `finishReason`, `category`, `probability`,
`confidence`, `tier`.

Identificadores numéricos **[bezenti]** (huecos libres del código limpio, para no mover ninguno existente): eventos
609–614 (`llm-response`, `api-error`, `request`, `spam-verdict`, `spam-skipped`, `breaker-open`), métricas 328–335
(`request-time`, `input-tokens`, `output-tokens`, `api-error`, `request`, `spam-verdict`, `spam-skipped`,
`breaker-open`), claves 87–95 (en el orden de arriba). **Las fases siguientes no deben reutilizarlos.**

Nunca se registran claves API ni cabeceras; los errores de red no incluyen la URL **[bezenti]**.

## 7. Privacidad: qué sale del servidor

- **Antispam**: solo el registro de hechos (§4.3) — remitente, destinatarios (número y dominios), resultados de
  autenticación, asunto, textos de enlaces y hosts, nombres y tipos de adjuntos y hasta `maxInputChars` caracteres del
  cuerpo sin citas — más las instrucciones. Nunca el mensaje en crudo, ni cabeceras completas, ni el contenido de los
  adjuntos. `ClassifySpam` enseña exactamente lo que se envía **[doc]**.
- **Sieve**: lo que el script ponga en el prompt **[doc]**.
- Si el contenido no puede salir de la casa: Ollama, vLLM, llama.cpp u otro servidor compatible con OpenAI en la red
  local (`url` propia, `apiKey` `None`) **[doc]**. Con proveedores alojados se aplica su política de datos.
- El contenido es de terceros y puede intentar manipular al modelo: las instrucciones le dicen que lo ignore, el
  veredicto nunca decide solo (se suma a la puntuación) y en Sieve la respuesta debe compararse con una lista fija
  **[doc]**.

## 8. Pendiente

- `webSearch` (el esquema solo define `Disable`).
- TypeSafe probado solo contra su documentación y el servidor falso (sin cuenta real).
- El estado del cortacircuitos y del límite de ritmo se reinicia al recargar la configuración.
- Panel web: los objetos están en `/api/schema`; no se ha probado la interfaz.
