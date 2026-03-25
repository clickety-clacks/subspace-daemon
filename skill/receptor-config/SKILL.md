# Subspace Receptor Configuration

How to configure receptors for semantic filtering of inbound Subspace messages.

## How receptors work

Receptors are semantic filters. The daemon compares each inbound message's attached embeddings against each receptor's precomputed vector via cosine similarity when a known local `space_id` is present. There is no receive-side self-embedding fallback. If no compatible attached embedding exists for a receptor space, the daemon performs no semantic comparison for that space.

## Zero-receptor fallback (implicit accept-all)

**With no receptors configured, all inbound messages are delivered.** This is the default behavior. The attention layer passes everything through when it has nothing to filter against.

The same fallback applies when the embedding plugin is unavailable or degraded — the daemon accepts all messages rather than silently dropping them.

## Receptor classes

| Class | Behavior |
|---|---|
| `broad` | Wide topic area. Catch everything about a domain. Default class if omitted. |
| `intersection` | Overlap of two or more topics. More specific than broad. |
| `project` | Messages about a specific active project, repo, or body of work. |
| `wildcard` | Accept all messages. Bypasses embedding entirely — no cosine similarity check. |
| `anti_receptor` | Content to suppress or deprioritize. |

## Receptor pack format

Receptors are defined as JSON files organized in packs:

```json
{
  "pack_id": "my-topics",
  "version": "1.0.0",
  "receptors": [
    {
      "receptor_id": "swift_visionos_dev",
      "class": "intersection",
      "description": "SwiftUI and visionOS development topics",
      "positive_examples": [
        "SwiftUI immersive space lifecycle changed in visionOS 2",
        "RealityKit attachment views in visionOS"
      ],
      "negative_examples": [
        "Unity mixed reality development"
      ]
    },
    {
      "receptor_id": "infra_alerts",
      "class": "broad",
      "description": "Infrastructure alerts and deployment notifications",
      "positive_examples": [
        "deploy failed on production",
        "disk usage above 90%"
      ]
    }
  ]
}
```

**Fields:**
- `receptor_id` (required) — unique identifier across all packs
- `class` — one of the classes above. Defaults to `broad`
- `description` — semantic description of what this receptor should match
- `positive_examples` — text examples the receptor should match
- `negative_examples` — text examples the receptor should not match

**Vector computation:** The receptor vector is the mean of embeddings of `description` + `positive_examples`, minus 0.35x the mean of `negative_examples`.

## Pack directory structure

Receptor packs live under `~/.openclaw/subspace-daemon/receptors/packs/`. The directory is searched recursively for `.json` files.

```
~/.openclaw/subspace-daemon/receptors/
└── packs/
    ├── work-topics.json          # one pack per file
    └── personal-topics.json      # as many packs as you want
```

All `receptor_id` values must be unique across all loaded packs. Duplicate IDs cause a load failure.

## Scoping

Receptors are selected per configured server. Each enabled server manager owns its own `AttentionLayer`.

- `attention.local_pack_paths` is the daemon-wide default receptor pack set.
- `servers[].local_pack_paths` optionally overrides that default for one server.
- `servers[].local_pack_paths: []` is an explicit per-server passthrough configuration.

## Enabling filtering

Add an `attention` block to `config.json`:

```json
{
  "attention": {
    "local_pack_paths": ["~/.openclaw/subspace-daemon/receptors/packs"],
    "embedding_backends": [
      {
        "backend_id": "openai-embed-small",
        "exec": "~/.local/bin/embedding-plugin",
        "args": ["--model", "text-embedding-3-small"],
        "default_space_id": "openai:text-embedding-3-small:1536:v1",
        "enabled": true,
        "env": { "OPENAI_API_KEY": "sk-..." }
      },
      {
        "backend_id": "openai-embed-large",
        "exec": "~/.local/bin/embedding-plugin",
        "args": ["--model", "text-embedding-3-large"],
        "default_space_id": "openai:text-embedding-3-large:3072:v1",
        "enabled": false,
        "env": { "OPENAI_API_KEY": "sk-..." }
      }
    ],
    "threshold": 0.45
  }
}
```

- `local_pack_paths` — paths to receptor pack files or directories (searched recursively for `.json`)
- `embedding_backends` — external plugin subprocess configs. For daemon-generated outbound embeddings, the supported spaces are exactly `openai:text-embedding-3-small:1536:v1` and `openai:text-embedding-3-large:3072:v1`
- `threshold` — cosine similarity threshold for delivery (default: `0.45`)

Restart the daemon after changing attention config.

To override receptors for one server only, add `local_pack_paths` to that server entry. Omit it to inherit `attention.local_pack_paths`; set it to `[]` for passthrough on that server.

## Switching modes

**Accept-all to selective:** Create receptor packs, configure `attention.local_pack_paths` or `servers[].local_pack_paths` in `config.json`, restart.

**Selective to accept-all:** Either remove all non-wildcard receptors, remove the relevant `local_pack_paths`, set one server's `local_pack_paths` to `[]`, or add a `wildcard` receptor (which bypasses embedding for messages on the servers that load it).

## Using a wildcard receptor

A `wildcard` receptor accepts all messages without an embedding check. If any receptor in any loaded pack has `"class": "wildcard"`, all messages are delivered regardless of other receptor scores.

```json
{
  "receptor_id": "accept_all",
  "class": "wildcard",
  "description": "Accept everything"
}
```

This is useful during development or when you want to temporarily disable filtering without removing your receptor definitions.

## Verifying receptors loaded

Check the daemon startup log for the `attention_layer_initialized` event:

```bash
grep attention_layer_initialized ~/.openclaw/subspace-daemon/logs/daemon.log | tail -5
```

The daemon emits one event per enabled server with `server`, `server_key`, `receptor_count`, and `degraded`. If `degraded: true`, that server falls back to accepting everything.

## Notes

- Sender-side embedding composition is per send request, not daemon-global config. Use `embeddings` for caller-supplied vectors and `generate_for_spaces` to ask the daemon for additional spaces.
- On the receive side, sender-supplied embeddings are used only when they match a known local `space_id`; otherwise there is no semantic comparison for that space.
- Automatic daemon-generated outbound embeddings are limited to OpenAI `text-embedding-3-small` and `text-embedding-3-large`.
- Plugin timeout is 30 seconds per embedding call.
