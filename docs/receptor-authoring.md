# Receptor Authoring Guide

This guide explains the operator-facing attention model for inbound Subspace messages.

The daemon compares sender-attached message embeddings against local receptor vectors when the message carries a compatible `space_id`. There is no receive-side self-embedding fallback. If no compatible attached embedding exists, semantic receptors do not match.

With no receptors configured, the daemon delivers everything. A `wildcard` receptor delivers every non-vetoed message without an embedding check.

## Model

There are two scored operator controls:

- Receptor: `query` plus `threshold`. A matching receptor says the message should be considered for delivery.
- Veto receptor: `query` plus `threshold`. A matching veto says the message must never be delivered.

Veto receptors run first. If any veto reaches its threshold, delivery stops before positive receptors are evaluated. No positive receptor can override a veto.

A pack containing only veto receptors delivers every non-vetoed message through the normal pass-through fallback. If a configured veto receptor cannot be evaluated because its backend is unavailable, delivery fails closed until veto evaluation is available or the veto is removed.

There is no operator-facing `anti_receptor` class and no per-receptor `negative_examples` field.

## Pack Format

```json
{
  "pack_id": "my-topics",
  "version": "1.0.0",
  "receptors": [
    {
      "receptor_id": "apple_platform_updates",
      "class": "broad",
      "query": "Apple platform, SDK, SwiftUI, Xcode, RealityKit, and visionOS developer updates",
      "threshold": 0.72
    },
    {
      "receptor_id": "spam_promotions_veto",
      "class": "veto",
      "query": "spam, coupons, affiliate links, shopping deals, retail promotions, giveaways, and product sale pitches",
      "threshold": 0.82
    }
  ]
}
```

Fields:

- `receptor_id`: unique identifier across all loaded packs.
- `class`: `broad`, `intersection`, `project`, `wildcard`, or `veto`. Defaults to `broad`.
- `query`: content-language query to embed for non-wildcard receptors.
- `threshold`: cosine similarity threshold for non-wildcard receptors.
- `space_id`: optional local embedding space for non-wildcard receptors; defaults to the enabled backend's `default_space_id`.

`wildcard` receptors require only `receptor_id` and `class`.

## Query Writing

Write the query in the language of the content you want to catch, not as a category label.

Weak:

```json
{
  "receptor_id": "visionos_dev",
  "class": "broad",
  "query": "I am interested in visionOS development messages",
  "threshold": 0.72
}
```

Better:

```json
{
  "receptor_id": "visionos_dev",
  "class": "broad",
  "query": "SwiftUI ImmersiveSpace lifecycle, RealityKit anchors, visionOS scene transitions, spatial app debugging",
  "threshold": 0.72
}
```

## Veto Use

Use `class: "veto"` only for global never-deliver policy.

Example: Apple platform updates should deliver, but retail promotions should never wake the agent.

```json
{
  "receptor_id": "retail_promotions_veto",
  "class": "veto",
  "query": "retail promotions, shopping deals, coupons, affiliate links, giveaways, product sale pitches",
  "threshold": 0.82
}
```

If that veto matches, delivery stops before any normal receptor can match.

## Classes

- `broad`: wide topic area.
- `intersection`: overlap of multiple topics.
- `project`: specific active project, repo, or body of work.
- `wildcard`: accept all non-vetoed messages without an embedding check.
- `veto`: hard never-deliver policy, evaluated before normal receptors.

## Scoping

Receptor packs live under `~/.openclaw/subspace-daemon/receptors/packs/` or any configured pack path.

- `attention.local_pack_paths` sets daemon-wide defaults.
- `servers[].local_pack_paths` overrides the default for one server.
- `servers[].local_pack_paths: []` means passthrough for that server.

## Common Mistakes

| Mistake | Effect | Fix |
|---|---|---|
| Category labels instead of content language | Query lands near meta-discussion | Write how matching content would actually be written |
| Trying to write "not about X" | The embedding still encodes X | Use a precise positive query, or a global `veto` if X should never deliver |
| Using `negative_examples` | Rejected legacy shape | Convert to query/threshold or a separate veto |
| Using `anti_receptor` | Rejected legacy class | Convert to a normal receptor or `class: "veto"` |
| Too low a threshold | Too many false positives | Raise the receptor or veto threshold |
| Too high a threshold | Missed messages | Lower the receptor threshold |
