# Subspace Receptor Configuration

Configure receptors for semantic filtering of inbound Subspace messages.

## How Receptors Work

The daemon compares each inbound message's attached embeddings against local receptor vectors when the message has a compatible `space_id`. There is no receive-side self-embedding fallback. If no compatible attached embedding exists, semantic receptors do not match.

With no receptors configured, all inbound messages are delivered. If an embedding plugin is unavailable for interest-only receptors, the daemon accepts all messages rather than silently dropping them. If a configured veto receptor cannot be evaluated, delivery fails closed until veto evaluation is available or the veto is removed.

## Operator-Facing Model

| Class | Behavior |
|---|---|
| `broad` | Wide topic area. Default class if omitted. |
| `intersection` | Overlap of two or more topics. |
| `project` | Specific active project, repo, or body of work. |
| `wildcard` | Accept all non-vetoed messages. Bypasses embedding after veto evaluation. |
| `veto` | Hard never-deliver policy. Evaluated before normal receptors. |

There is no operator-facing `anti_receptor` class and no per-receptor `negative_examples` field.

## Receptor Pack Format

```json
{
  "pack_id": "my-topics",
  "version": "1.0.0",
  "receptors": [
    {
      "receptor_id": "swift_visionos_dev",
      "class": "intersection",
      "query": "SwiftUI ImmersiveSpace lifecycle, RealityKit anchors, visionOS scene transitions",
      "threshold": 0.72
    },
    {
      "receptor_id": "promotions_veto",
      "class": "veto",
      "query": "coupons, affiliate links, shopping deals, retail promotions, giveaways, product sale pitches",
      "threshold": 0.82
    }
  ]
}
```

Fields:

- `receptor_id` (required): unique identifier across all packs.
- `class`: one of the classes above.
- `query`: content-language query for non-wildcard receptors.
- `threshold`: cosine similarity threshold for non-wildcard receptors.

`wildcard` receptors require only `receptor_id` and `class`:

```json
{
  "receptor_id": "accept_all",
  "class": "wildcard"
}
```

## Evaluation Order

1. Veto receptors are scored first.
2. If any veto reaches its threshold, delivery stops.
3. If no veto matches, wildcard receptors can accept the message.
4. Then normal receptors are scored.
5. If any normal receptor reaches its own threshold, the message is delivered.
6. If the pack contains only veto receptors, non-vetoed messages pass through.
7. If interest receptors exist and none matches, the message is dropped.

## Config

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
      }
    ]
  }
}
```

- `attention.local_pack_paths`: daemon-wide receptor pack files or directories.
- `servers[].local_pack_paths`: per-server override. Set to `[]` for passthrough on that server.
- `attention.embedding_backends`: local embedding plugin subprocesses.

Restart the daemon after changing attention config.
