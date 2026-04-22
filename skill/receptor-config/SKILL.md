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

## Agent Decision Guidance

When you are configuring Subspace for a user, treat receptors as the user's standing attention policy, not as a temporary preference for your current task. Receptors decide which inbound messages are worth waking an agent for before any expensive agent work happens.

Default to the narrowest mode that matches the user's intent:

- Use no receptors or `servers[].local_pack_paths: []` when the user wants all messages from that server to wake the agent.
- Use a `wildcard` receptor when the user wants to keep a receptor pack active, especially vetoes, but still receive every non-vetoed message.
- Use normal receptors when the user can name durable topics, projects, domains, or workstreams they want surfaced.
- Use `intersection` when the desired signal is the overlap of concepts, not either concept alone.
- Use `project` when the user names a specific repo, initiative, customer, ticket family, or ongoing body of work.
- Use `veto` only for global never-deliver policy, such as spam, promotions, or categories the user explicitly never wants to wake the agent.

Before writing a receptor pack, infer only from durable user context. Do not overfit receptors to the install session, the current debugging task, the hostname, your own agent role, or examples the user gave only to explain the mechanism.

If the user's desired attention policy is unclear, ask one focused question:

```text
Should Subspace wake this agent for every message, only durable topics you name, or every non-vetoed message while blocking a few categories?
```

Then choose:

- "every message" -> no receptors, an empty per-server pack override, or a `wildcard` receptor if vetoes should remain active.
- "durable topics" -> normal `broad`, `intersection`, or `project` receptors.
- "block a few categories" -> `veto` receptors, usually with pass-through or wildcard behavior for everything else.

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
