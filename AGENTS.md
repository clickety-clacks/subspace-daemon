# AGENTS.md — subspace-daemon

This file is for **the agent performing a subspace-daemon install** on behalf of a user.

If you are a coding agent working on the source code, the relevant file is the canonical spec at `/Users/mike/shared-workspace/subspace/specs/subspace-daemon.md`.

---

## Your job during a first-time install

You are setting up subspace-daemon so the user's agents can receive messages from a Subspace server. Before you touch any config files, **tell the user what they're getting**:

> Subspace is a message bus your agents listen to. When someone sends a message to a Subspace server you're subscribed to, it arrives at this daemon and gets routed to your agents — so they can read it, react to it, or surface it to you.
>
> By default we'll configure it to deliver everything. If the space gets noisy later, your agents can narrow it down to just the topics you care about using receptors. For now, you'll receive all messages.

Then proceed with the install below.

---

## Default install steps

### 1. Build and place the binary

```bash
cargo build --release
cp target/release/subspace-daemon ~/.local/bin/subspace-daemon
```

### 2. Create the config directory

```bash
mkdir -p ~/.openclaw/subspace-daemon/receptors/packs
mkdir -p ~/.openclaw/subspace-daemon/logs
```

### 3. Write the default config

`~/.openclaw/subspace-daemon/config.json`:

```json
{
  "servers": [
    {
      "url": "http://<subspace-server>",
      "api_key": "<user-api-key>"
    }
  ],
  "delivery": {
    "session_key": "<user-agent-session-key>"
  },
  "attention": {
    "receptors_path": "~/.openclaw/subspace-daemon/receptors",
    "embedding_backends": []
  }
}
```

Fill in the server URL, API key, and the session key of the agent that should receive messages (e.g. `agent:heimdal:main`).

### 4. Write the default wildcard receptor pack

`~/.openclaw/subspace-daemon/receptors/packs/default.json`:

```json
{
  "pack_id": "default",
  "description": "Deliver all messages. Replace with topic receptors if the space gets noisy.",
  "receptors": [
    {
      "id": "all_messages",
      "class": "wildcard"
    }
  ]
}
```

The wildcard receptor requires no embedding backend — it accepts everything.

### 5. Install and start the launchd service

Use whatever service management tooling is available (OpenClaw's daemon manager, or manual plist). The daemon binary is:

```
~/.local/bin/subspace-daemon serve --config ~/.openclaw/subspace-daemon/config.json
```

### 6. Tell the user setup is complete

> Subspace is running. Your agents will now receive messages from [server]. The default setup delivers all messages.
>
> If this space gets busy and you want your agents to only pay attention to specific topics, ask your agent to configure receptors — semantic filters that wake your agents only for messages that match what you care about.

---

## When the user asks about receptors

Explain it like this:

> Receptors are topic filters. Instead of delivering every message, the daemon embeds each message and checks whether it's semantically close to what you've defined as your interests.
>
> The wildcard you have now delivers everything. When you're ready to narrow it down, tell your agent what topics matter — it will write the receptor definitions and remove the wildcard.

### How to write a good receptor

**Use the language of the content, not a category label.**
- ❌ `"description": "Messages about visionOS development"` — lands near meta-discussion, not actual dev messages
- ✅ `"description": "SwiftUI immersive space lifecycle, RealityKit anchors, visionOS scene transitions"` — lands in the right cluster

**Negatives are vectors, not keywords. Never write "not about X".**
The word NOT doesn't work — the model still encodes X. Write the negative examples as actual content from the unwanted domain:
- ❌ `"negative_examples": ["not about Android"]`
- ✅ `"negative_examples": ["Android Jetpack Compose", "Google Play Store submission"]`

**Intersection receptors are precise and powerful.**
A receptor for `swift × visionOS` catches the overlap without catching generic Swift questions or generic visionOS discussion.

### Example topic receptor (requires embedding backend)

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

To use semantic receptors you need an embedding backend in `config.json`. The reference backend script is `~/.local/bin/subspace-embed-openai` (OpenAI `text-embedding-3-small`). See the canonical spec for backend config details.

### When to remove the wildcard

Once the user has topic receptors covering what they care about, remove `default.json` (or delete the wildcard entry from it). The daemon will then only wake agents for matched messages.

---

## Key facts for coding agents

Repo structure, invariants, and test procedures:

```
src/
  config.rs        — config schema
  main.rs          — CLI (serve / setup / send)
  supervisor.rs    — WebSocket manager, delivery router
  gateway/         — gateway auth, chat.send
  subspace/        — Subspace WebSocket client
  attention/       — receptor matching, embedding plugin, scoring
  ipc.rs           — Unix socket IPC
docs/
  receptor-authoring.md  — detailed receptor authoring guide
```

Invariants:
1. Each message delivered exactly once regardless of receptor match count.
2. Wildcard receptor requires no embedding backend.
3. Semantic receptors require a configured embedding backend.
4. No receptors configured = wildcard behavior by default.
5. Vectors from different `space_id` values are never compared.

Canonical spec: `/Users/mike/shared-workspace/subspace/specs/subspace-daemon.md` — wins over code if they conflict.
