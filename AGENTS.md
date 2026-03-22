# AGENTS.md — subspace-daemon

This file is for AI coding agents working in this repository.

---

## What this repo is

`subspace-daemon` is a standalone Rust daemon that connects to one or more Subspace servers and delivers inbound messages to local OpenClaw agents via the gateway.

It ships as a single binary and runs as a persistent launchd/systemd service.

---

## First-time install: what to tell the user

When you set up subspace-daemon for a user for the first time, explain the following in plain language before proceeding:

> **Subspace** is a message bus your agents listen to. When someone (or something) sends a message to a Subspace server you're subscribed to, it arrives at this daemon and gets routed to your agents.
>
> **By default**, subspace-daemon is configured with an "all messages" receptor — a wildcard that collects every message and delivers it. This is the right starting point. You'll see everything.
>
> **In noisy Subspace spaces**, where many unrelated messages flow, you'll want to replace or supplement the wildcard with *receptors* — semantic filters that wake your agents only for messages that match your interests. Your agents can configure those over time as the signal-to-noise ratio becomes clear.
>
> For now, we'll set up the default wildcard config so you start receiving immediately.

---

## Default install config

The default `config.json` for a new install should include a wildcard receptor so all messages are delivered. No embedding backend is required for wildcard operation.

```json
{
  "servers": [
    {
      "url": "http://<subspace-server>",
      "api_key": "<your-api-key>"
    }
  ],
  "delivery": {
    "session_key": "<your-agent-session-key>"
  },
  "attention": {
    "receptors_path": "~/.openclaw/subspace-daemon/receptors",
    "embedding_backends": []
  }
}
```

And a default receptor pack at `~/.openclaw/subspace-daemon/receptors/packs/default.json`:

```json
{
  "pack_id": "default",
  "description": "Default all-messages receptor — delivers everything. Replace with topic receptors when the space gets noisy.",
  "receptors": [
    {
      "id": "all_messages",
      "description": "Accept all messages",
      "class": "wildcard"
    }
  ]
}
```

This gets the user receiving immediately without requiring an embedding backend.

---

## When to recommend switching to topic receptors

Tell the user to consider replacing the wildcard when:
- They're subscribed to a high-volume space with many unrelated topics
- Their agents are waking up on noise rather than signal
- They have a clear domain they care about (e.g. visionOS dev, finance, a specific project)

When they're ready, help them design receptors (see below) and remove the wildcard once the topic receptors are covering what matters.

---

## Receptor authoring

Read `docs/receptor-authoring.md` before writing or modifying any receptor.

The short version:

**Write examples in the language of the content, not the category.**
- ❌ Bad: `"description": "Messages about Apple visionOS development"`
- ✅ Good: `"description": "SwiftUI immersive space lifecycle, RealityKit anchors, visionOS scene transitions"`

**Use negatives for exclusion, never the word NOT.**
Negatives are embedded and subtracted as vectors. Writing "not about X" encodes X into the receptor.
- ❌ Bad: `"negative_examples": ["not about Android", "not about gaming"]`
- ✅ Good: `"negative_examples": ["Android app development", "Unity game engine"]`

**Intersection receptors are powerful and precise.**
`swift × visionOS` catches Swift+visionOS overlap without catching generic Swift or generic visionOS messages.

**A wildcard receptor catches everything and requires no embedding backend:**
```json
{
  "id": "all_messages",
  "class": "wildcard"
}
```

**A semantic receptor requires an embedding backend in config:**
```json
{
  "id": "visionos_dev",
  "description": "SwiftUI visionOS RealityKit immersive space development",
  "positive_examples": [
    "How do I persist a WorldAnchor across app sessions?",
    "SwiftUI ImmersiveSpace lifecycle onAppear not firing",
    "RealityKit spatial audio with positional entities"
  ],
  "negative_examples": [
    "Android Jetpack Compose UI",
    "Unity game development",
    "macOS AppKit desktop apps"
  ]
}
```

Full receptor authoring guide: `docs/receptor-authoring.md`

---

## Canonical spec

```
/Users/mike/shared-workspace/subspace/specs/subspace-daemon.md
```

Spec wins over code if they conflict. Flag the conflict, don't silently resolve it.

---

## Repo structure

```
src/
  config.rs        — config schema and server_key derivation
  main.rs          — CLI entry point (serve / setup / send)
  supervisor.rs    — WebSocket manager, delivery router
  gateway/         — gateway auth, device pairing, chat.send
  subspace/        — Subspace auth, firehose WebSocket client
  attention/       — receptor matching, embedding plugin, scoring
  ipc.rs           — Unix socket IPC for outbound sends
  runtime_store.rs — per-server state persistence
  state_lock.rs    — exclusive lock between serve and setup
  logging.rs       — structured JSON logging
docs/
  receptor-authoring.md  — how to write effective receptor packs
```

---

## Key invariants

1. Each message is delivered exactly once regardless of how many receptors match.
2. Wildcard receptor (`class: wildcard`) requires no embedding backend — use for default installs.
3. Semantic receptors require an embedding backend configured in `config.json`.
4. Plaintext messages remain valid without any semantic metadata.
5. Vectors from different `space_id` values are never compared.
6. If no receptors are configured, daemon defaults to wildcard behavior.

---

## Testing

No automated test suite yet. Manual verification:
- Build: `cargo build --release`
- Run: `~/.local/bin/subspace-daemon serve`
- Test inbound: send from eezo `subspace-ext` session (not TARS-local — self-filters)
- Test outbound: `~/.local/bin/subspace-daemon send "message text"`
