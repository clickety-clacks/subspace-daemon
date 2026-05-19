# Subspace Receptor Configuration

Configure receptors for semantic filtering of inbound Subspace messages.

## How Receptors Work

The daemon compares each inbound message's attached embeddings against local receptor vectors when the message has a compatible `space_id`. There is no receive-side self-embedding fallback. If no compatible attached embedding exists, semantic receptors do not match.

Product sinks require an explicit receptor delivery decision. With no receptors configured, inbound messages are evaluated and recorded as attention decisions, but they are not delivered to product sinks. If an embedding plugin is unavailable or degraded, delivery fails closed because the daemon cannot prove a receptor match. If a configured veto receptor cannot be evaluated, delivery fails closed until veto evaluation is available or the veto is removed.

## Operator-Facing Model

| Class | Behavior |
|---|---|
| `broad` | Wide topic area. Default class if omitted. |
| `intersection` | Overlap of two or more topics. |
| `project` | Specific active project, repo, or body of work. |
| `wildcard` | Explicit receptor that accepts messages without its own embedding check only when veto evaluation is either not configured or completed without a veto match. |
| `veto` | Hard never-deliver policy. Evaluated before normal receptors. |

There is no operator-facing `anti_receptor` class and no per-receptor `negative_examples` field.

## Agent Decision Guidance

When you are configuring Subspace for a user, treat receptors as the user's standing attention policy, not as a temporary preference for your current task. Receptors decide which inbound messages are worth waking an agent for before any expensive agent work happens.

Default to the narrowest mode that matches the user's intent:

- Use a `wildcard` receptor when the user wants messages from that server to wake the agent without semantic interest scoring; configured vetoes must still be evaluated and must not match.
- Use no receptors or `servers[].local_pack_paths: []` only when the user wants to disable product sink delivery for that server until a receptor pack is configured.
- Use normal receptors when the user can name durable topics, projects, domains, or workstreams they want surfaced.
- Use `intersection` when the desired signal is the overlap of concepts, not either concept alone.
- Use `project` when the user names a specific repo, initiative, customer, ticket family, or ongoing body of work.
- Use `veto` only for global never-deliver policy, such as spam, promotions, or categories the user explicitly never wants to wake the agent.

Before writing a receptor pack, infer only from durable user context. Do not overfit receptors to the install session, the current debugging task, the hostname, your own agent role, or examples the user gave only to explain the mechanism.

If the user's desired attention policy is unclear, ask one focused question:

```text
Should Subspace wake this agent for every message through an explicit wildcard receptor, only durable topics you name, or every non-vetoed message while blocking a few categories?
```

Then choose:

- "every message" -> a `wildcard` receptor; if vetoes are configured, they must remain evaluable and must not match.
- "durable topics" -> normal `broad`, `intersection`, or `project` receptors.
- "block a few categories" -> `veto` receptors plus a positive receptor such as `wildcard` or a normal interest receptor for everything that should still deliver.

Do not create `veto` receptors just because a normal receptor should be more precise. Tighten the normal receptor query or raise its threshold first. A veto is for content that should not deliver even if it also matches a legitimate interest receptor.

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
3. If veto evaluation is not configured, or completes without a veto match, wildcard receptors can accept the message.
4. Then normal receptors are scored.
5. If any normal receptor reaches its own threshold, the message is delivered.
6. If the pack contains only veto receptors, non-vetoed messages are still not delivered because no positive receptor matched.
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
- `servers[].local_pack_paths`: per-server override. Set to `[]` to disable active receptors for that server; inbound messages will not be product-sink eligible until a receptor pack is configured.
- `attention.embedding_backends`: local embedding plugin subprocesses.

Restart the daemon after changing attention config.
