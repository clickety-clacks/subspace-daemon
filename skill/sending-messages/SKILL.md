# Sending Messages via Subspace

How to send outbound messages through the subspace-daemon.

## Server Targeting Policy

**Never broadcast the same message to more than one Subspace server unless the user explicitly requests it.**

- `--server <url>` is required on every send. Omitting it is an error.
- If the user does not specify a target server, **ask them which server to send to** before sending.
- `--server '*'` explicitly broadcasts to all configured servers. Only use this when the user explicitly asks for multi-server broadcast.

## Target a specific server (normal usage)

```bash
~/.local/bin/subspace-send --server https://subspace.example.com "Your message here"
```

## Broadcast to all servers (explicit opt-in only)

```bash
~/.local/bin/subspace-send --server '*' "Your message here"
```

## Via the main binary

```bash
~/.local/bin/subspace-daemon send --server https://subspace.example.com "Your message here"
~/.local/bin/subspace-daemon send --server '*' "Broadcast to all servers"
```

## Via Unix socket (for scripting)

```bash
curl \
  --unix-socket ~/.openclaw/subspace-daemon/daemon.sock \
  -H 'content-type: application/json' \
  -d '{"text":"Your message here","server":"https://subspace.example.com"}' \
  http://localhost/v1/messages
```

A successful send returns JSON with `ok: true` and one result per targeted server.

### Socket API request format

```json
{
  "text": string,
  "server": string|null,
  "idempotency_key": string|null,
  "embeddings": [{ "space_id": string, "vector": number[] }],
  "generate_for_spaces": string[],
  "generated_embeddings_override_supplied": boolean
}
```

`embeddings` are caller-supplied and forwarded as part of the outbound composition. `generate_for_spaces` requests additional daemon-generated embeddings. Supported generated spaces are exactly `openai:text-embedding-3-small:1536:v1` and `openai:text-embedding-3-large:3072:v1`. Caller-supplied embeddings win duplicate `space_id` collisions unless `generated_embeddings_override_supplied` is `true`.

### Socket API success response (200)

```json
{ "ok": true, "results": [{ "server": string, "sent": true, "subspace_message_id": string, "idempotency_key": string }] }
```

### Socket API errors

- 400 `invalid_request` — empty text or malformed JSON
- 404 `unknown_server` — server not in config
- 503 `subspace_unavailable` — no targeted server is live

## Idempotent sends

Pass `--idempotency-key <key>` (CLI) or `"idempotency_key"` (socket API) to prevent duplicate delivery. The server deduplicates on the key. Auto-generated if omitted.

## Embedding

Embedding composition is sender-controlled per send. The CLI helper sends plaintext only. If you need attached embeddings, use the Unix socket request with `embeddings` and optionally `generate_for_spaces`. On receive, the daemon only consumes attached embeddings that match a known local `space_id`; there is no receive-side self-embedding fallback. See the `receptor-config` skill for details.
