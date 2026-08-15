# Stability AI Image Generation Backend — `dev.mcpg.backend.llm.stability`

> class `backend` · `native` · package `mcpg-plugin-backend-llm-stability` · artifact `libmcpg_plugin_backend_llm_stability.so` · Apache-2.0

Exposes Stability AI's Stable Image API as an MCP tool. A binding pins one of
Stability's three SKUs — Core, SD3 or Ultra — plus the default framing and
output format; a caller supplies a prompt and gets back an
`mcpg-resource://<id>` URI pointing at the generated image, which the gateway
serves through an ordinary MCP `resources/read`. Reach for it when image
generation should be a governed, audited MCP capability with the API key held
by the gateway instead of handed to clients.

## What it does
- Registers one backend entity, `dev.mcpg.backend.stability.image`, whose
  `BackendPlugin::kind()` is `stability.image`; bindings select it with
  `backend.kind: stability_image`.
- POSTs `multipart/form-data` (not JSON) to
  `{base_url}/v2beta/stable-image/generate/{model}`, where `model` is the
  operator-chosen route: Stability's SKUs are exposed through the `model` field
  so a tool definition looks the same as it does for the OpenAI and Gemini image
  bindings.
- Sends `Accept: application/json` so Stability returns the image base64-encoded
  in a small envelope, giving the engine bytes in hand with no follow-up fetch.
- Pushes those bytes into the gateway's content store and returns an
  `mcpg-resource://<id>` URI, so tool results stay small.
- Translates an operator or caller `size` — `WxH` pixels or a bare `W:H` ratio —
  into one of Stability's nine accepted aspect ratios, and refuses anything that
  does not reduce to one of them with a message naming the supported set.
- Surfaces a `finish_reason: CONTENT_FILTERED` response as a clear bad-request
  error rather than an empty success, so an operator sees *why* no bytes came
  back.
- Rejects a request for more than one image up front: the API returns one image
  per call.
- Retries rate-limit, 5xx and network failures with exponential backoff.
- Declares the `network_outbound` capability — required in every mode, since
  every call is an outbound HTTPS request to Stability.

## Configuration

Load the artifact once from the flat top-level `plugins:` list, then declare one
binding per capability under `mcp.capabilities.tools[]` (or `.prompts[]` /
`.resources[]`) with `backend.kind: stability_image`. Everything else inside the
`backend:` block is the plugin's own spec, forwarded verbatim and validated by
the plugin at boot — a `model` typo fails gateway startup with the accepted
values listed, not on the first call.

```yaml
plugins:
  - id: dev.mcpg.backend.llm.stability
    class: backend
    source:
      oci: ghcr.io/mcpg-dev/source-code/plugins/backend-llm-stability:protocol-1

mcp:
  capabilities:
    tools:
      - name: art.generate
        description: Generate an illustration from a prompt.
        input_schema:
          type: object
          properties:
            prompt: { type: string }
            size:   { type: string, description: "WxH or W:H" }
          required: [prompt]
        backend:
          kind: stability_image
          api_key: "${env.STABILITY_API_KEY}"
          model: core                 # core | sd3 | ultra
          timeout_ms: 60000
          defaults:
            size: "1024x1024"
            output_format: webp
            negative_prompt: "blurry, low quality"
```

| Field | Type | Default | Description |
|---|---|---|---|
| `api_key` | string | *(required)* | Sent as `Authorization: Bearer …`. Supply `${env.NAME}` or a `scheme://` URI bound to a `secret_provider` plugin (for example `vault://secret/stability#key`); the gateway substitutes the literal value at config load. An empty resolved value is rejected. |
| `base_url` | string | `https://api.stability.ai` | Override only for a forwarding proxy or a test fixture. The adapter appends `/v2beta/stable-image/generate/{model}`. |
| `model` | string | *(required)* | Route selector: `core`, `sd3` or `ultra`, matched case-insensitively. Anything else is refused at boot. |
| `timeout_ms` | integer | `60000` | Per-call timeout. |
| `connect_timeout_ms` | integer | `5000` | TCP connect timeout. |
| `defaults.size` | string | *(unset)* | Default framing, as `WxH` pixels or a bare `W:H` ratio. A per-call `size` argument overrides it. |
| `defaults.n` | integer | *(unset)* | Default image count; the engine falls back to `1` when neither the binding nor the call sets it, and this API accepts only `1`. |
| `defaults.negative_prompt` | string | *(unset)* | Default negative prompt. Accepted on the `core` and `sd3` routes; dropped with a warning on `ultra`. |
| `defaults.output_format` | string | `png` | `png`, `jpeg` or `webp`. Also decides the MIME type recorded with the stored bytes. |
| `retry.max_attempts` | integer | `3` | Attempts per upstream call. |
| `retry.initial_backoff_ms` | integer | `200` | First backoff. |
| `retry.max_backoff_ms` | integer | `2000` | Backoff ceiling. |

## Operations

One operation, one argument shape. `prompt` is required and must be non-empty;
everything else falls back to the binding's `defaults`.

| Argument | Type | Description |
|---|---|---|
| `prompt` | string | *(required)* The generation prompt. |
| `size` | string | `WxH` or `W:H`; reduced to an accepted aspect ratio. |
| `seed` | integer | Passed through for reproducible generations. |
| `negative_prompt` | string | Overrides `defaults.negative_prompt`. |
| `output_format` | string | Overrides `defaults.output_format`. |
| `quality` / `style` / `n` | — | Accepted, as arguments or under `defaults`, for parity with the other image bindings; the Stability request carries no quality or style, and `n` above `1` is refused. |

The accepted aspect ratios are `21:9`, `16:9`, `3:2`, `5:4`, `1:1`, `4:5`,
`2:3`, `9:16` and `9:21`. A pixel `size` is reduced to its ratio before
matching, so `1024x1024` becomes `1:1`; a size that reduces to something outside
the set is rejected with the supported list in the error message.

## Response envelope

```jsonc
{
  "images": [
    {
      "image_uri": "mcpg-resource://<id>",
      "mime_type": "image/webp"
    }
  ]
}
```

`images` is always an array, even though this API returns exactly one image. The
bytes live in the gateway's content store; clients fetch them with an MCP
`resources/read` against `image_uri`, so nothing large travels inline in the
tool result.

## Security

- The API key is held in a redacting wrapper — `Debug` renders `***`, so it
  cannot leak through logs or error strings. A key that resolves to an empty
  value is rejected at boot rather than producing unauthenticated calls.
- `base_url` and `model` are operator config, never caller arguments, so a
  caller can neither redirect the binding at another host nor switch to a
  costlier SKU.
- Content-filter refusals are surfaced as errors, not silently swallowed, so a
  filtered generation is visible in logs and audit rather than looking like an
  empty result.

## Observability

Each call emits `mcpg_image_calls_total`, labelled with `backend` (the binding
name), `provider` (`stability`), `model` and `status`, alongside the shared image
engine's `mcpg_image_call_seconds` histogram and `mcpg_image_generated_total`
counter, which carry the same labels minus `status`.

## MCP surfaces & composition

### As a child tool

A `stability_image` binding can appear in a chat binding's `tools.allowed`,
letting a model generate an illustration mid-turn and reference the resulting
`mcpg-resource://` URI in its answer.

```yaml
        backend:
          kind: openai_chat
          api_key: "${env.OPENAI_API_KEY}"
          model: gpt-4o-mini
          prompt:
            system: Call `art.generate` when the user asks for a picture.
            user: "{{ input.question }}"
          tools:
            allowed: [art.generate]   # a binding backed by stability_image
```

### Schemas & annotations

Declare the binding-level `input_schema` so clients know `prompt` is required
and `size` is available; it is also what the gateway validates arguments
against. Image generation is a paid, non-idempotent side effect — annotate it
honestly:

```yaml
        annotations: { read_only: false, open_world: true }
```

## Build
Default feature set is OFF (avoids `mcpg_plugin_register` linker
collisions in the workspace build); opt in to the cdylib:

```bash
cargo build -p mcpg-plugin-backend-llm-stability --features cdylib-export --release   # → target/release/libmcpg_plugin_backend_llm_stability.so
```

## Sign & load (production)
Sign the artifact, pin/verify via the entry's `signature:` block, and honour
revocations. See <https://mcpg.dev/docs/security/plugin-security>.

## See also
- Backend binding reference: <https://mcpg.dev/docs/reference/backends>
- Full gateway config schema: <https://mcpg.dev/docs/reference/configuration>
- Sibling image backends: `libs/plugins/backend/llms/openai`, `libs/plugins/backend/llms/gemini`
- Provider-agnostic image engine and content store: `libs/plugins/backend/llms/shared`
