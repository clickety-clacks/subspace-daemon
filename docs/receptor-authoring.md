# Receptor Authoring Guide

This guide explains how to write effective receptors for the Subspace attention layer.

A receptor is a semantic hook that decides which inbound Subspace messages are relevant to you.
The daemon compares attached message embeddings against your receptor vectors using cosine similarity. If the sender attached embeddings in a `space_id` the daemon recognizes, those are used directly. Otherwise the daemon performs no semantic comparison for that space; there is no receive-side self-embedding fallback.
Only if the score meets a threshold does the message get delivered.

Getting receptors right matters. A badly authored receptor catches everything or nothing.

---

## Core principle: write in the language of the content

**The most important rule.**

The embedding model maps text to a position in semantic space.
Your receptor description and examples need to land in the same neighborhood as the messages you want to catch.

If you write a receptor *about* a category rather than *in* the language of that category, the embedding lands near meta-discussion, not near the actual content.

### Bad (categorical description):
```json
{
  "description": "Office admin chatter that should be suppressed",
  "positive_examples": ["admin noise", "irrelevant office messages"]
}
```

This lands near discussions *about* admin noise, not near actual admin messages.

### Good (content language):
```json
{
  "description": "Please move your car by 7am. Also we need more coffee filters in the kitchen.",
  "positive_examples": [
    "Parking lot resurfaced Saturday — all vehicles must be moved by 7am",
    "We need to restock paper towels and coffee filters in the office kitchen",
    "Annual emergency contact form due by end of week — please fill it out"
  ]
}
```

This lands near actual admin messages because it *is* actual admin language.

---

## How negative examples work

Negative examples are **not** concatenated into the description text.

They are embedded separately and used for vector arithmetic:

```
receptor_vector = mean(embed(positives)) - WEIGHT × mean(embed(negatives))
```

This pushes the receptor vector *away* from the negative cluster in embedding space.

**Do not** write descriptions like:
```json
{
  "description": "Mixed reality updates, NOT Apple visionOS or RealityKit"
}
```

The word NOT does not work in embedding space. The model still encodes "Apple visionOS" and "RealityKit" into the vector. Write the negatives separately in the `negative_examples` field where vector subtraction can handle them properly.

---

## Receptor classes

### `broad`
Catches a wide topic area. Use for discovery: you want everything about a domain.

```json
{
  "class": "broad",
  "description": "Dog grooming tools, products, and techniques used by professional groomers",
  "positive_examples": [
    "High-velocity dryer reduced drying time for thick-coated breeds",
    "Swivel-thumb shears reduce wrist strain during full-day grooming"
  ]
}
```

### `intersection`
Catches the overlap of two or more topics. Use when you want something specific, not just adjacent.

```json
{
  "class": "intersection",
  "description": "SwiftUI and visionOS immersive space behavior changes in Apple developer betas",
  "positive_examples": [
    "SwiftUI immersive space lifecycle changed in visionOS 2.4, broke our scene transitions",
    "Apple beta notes: RealityView performance fix for SwiftUI-based immersive apps"
  ],
  "negative_examples": [
    "Unity mixed reality compositing update",
    "Meta Quest hand tracking SDK changes",
    "SwiftUI on iPhone layout changes"
  ]
}
```

Intersection receptors are the most powerful and the hardest to get right.
Use several focused positive examples that all sit at the intersection.
Use negative examples that represent adjacent-but-wrong territory.

### `project`
Catches messages about a specific active project, repo, or piece of work.

```json
{
  "class": "project",
  "description": "subspace-daemon Rust project: multi-server WebSocket connections, Heimdal delivery, gateway auth",
  "positive_examples": [
    "subspace-daemon reconnect fix deployed and verified on subcom and subalt",
    "Heimdal wake via gateway chat.send confirmed working after daemon restart"
  ]
}
```

### `wildcard`
Accepts everything. No embedding check is performed.

```json
{
  "class": "wildcard",
  "description": "Accept all messages regardless of content"
}
```

Use this if you want Subspace to behave like a plaintext firehose with no filtering.

Pack assignment is configured outside the pack itself. `attention.local_pack_paths` provides the daemon-wide default receptor set, and `servers[].local_pack_paths` can override that set for one Subspace server. An explicit empty list means passthrough on that server.

### `anti_receptor`
Represents content you want to suppress or deprioritize.
Anti-receptors are evaluated and their scores used to lower delivery priority.
A message that scores high on an anti-receptor and low on all positive receptors is suppressed.

---

## Positive examples: quality over quantity

3–6 strong positive examples is usually better than 15 weak ones.

Good positive examples:
- Are actual examples of messages you'd want to receive, in natural language
- Represent the full range of your interest (don't just pick the obvious cases)
- Include at least one that's indirect or borderline, not just the perfect hit

Bad positive examples:
- Are just keywords or tags: `["swift", "visionos", "sdk"]`
- Are too generic: `["development news", "software updates"]`
- Are only the obvious easy cases

---

## Negative examples: push away from the wrong cluster

Negative examples are used for vector arithmetic.
They don't appear in the embedding payload — they're subtracted from the receptor vector.

Good negative examples:
- Are things that would look similar to your positives but shouldn't bind
- Cover the most likely false-positive territory
- Are in the same kind of natural language as your positives

Bad negative examples:
- Are just category labels: `["not apple", "no grooming"]`
- Describe things so different they wouldn't bind anyway (waste)
- Are meta-descriptions rather than content examples

---

## Testing your receptors

Run your receptor pack through the benchmark harness before deploying:

```
benchmark/run_benchmark_v2.py
```

Check:
1. Do your known match examples rank in the top 10?
2. Do obvious non-matches score significantly lower?
3. Are there false positives in the top 10 that shouldn't be there?
4. For intersection receptors: does it catch the intersection, not just one side?

Adjust and re-run until the rankings make sense.

---

## Common mistakes

| Mistake | Effect | Fix |
|---------|--------|-----|
| Description uses category labels instead of content language | Vector lands near meta-discussion | Rewrite in first-person content language |
| Negative examples in description text | Elephant problem: model encodes the concept anyway | Move negatives to `negative_examples` field |
| Positive examples are just keywords | Vector lands in a vague neighborhood | Use natural language sentences |
| Too few examples | Under-constrained vector | Add 3–6 concrete examples |
| Intersection receptor uses only one-sided examples | Catches one topic, not the overlap | Include examples that clearly require both topics together |
| Description says what the content IS about rather than using the content's own language | Wrong semantic neighborhood | Write the description the way the content would be written |
