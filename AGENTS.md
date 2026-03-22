# AGENTS.md — subspace-daemon

This file is for **the agent performing a subspace-daemon install** on behalf of a user.

If you are a coding agent working on the source code, see the canonical spec at `/Users/mike/shared-workspace/subspace/specs/subspace-daemon.md`.

---

## Your job during a first-time install

Follow these steps in order. Do not skip the user-facing messages — they matter.

### Step 1: Tell the user what you're about to do

Say something like:

> I'm going to set up Subspace for you. Subspace is a way for your agents to receive messages — think of it like a channel your agents can listen to. Once this is running, anything sent to your Subspace server will arrive here and your agents can act on it.
>
> I'll start with a deliver everything setup so nothing gets missed. If it ever gets too noisy, we can add filters so your agents only pay attention to the topics you actually care about.

### Step 2: Ask the user for their Subspace server

Ask:

> Do you have a Subspace server address and API key you'd like to connect to? If not, I can leave that blank for now and you can fill it in later.

Wait for their answer. If they give you a URL and key, use them. If not, use placeholder values and tell them where to fill it in.

### Step 3: Build and install the binary

```bash
cargo build --release
cp target/release/subspace-daemon ~/.local/bin/subspace-daemon
```

### Step 4: Create config directories

```bash
mkdir -p ~/.openclaw/subspace-daemon/receptors/packs
mkdir -p ~/.openclaw/subspace-daemon/logs
```

### Step 5: Write the config

`~/.openclaw/subspace-daemon/config.json` — use the server URL and key from Step 2:

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

### Step 6: Write the default "all messages" receptor

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

Tell the user:

> I've set up the default "all messages" filter — this means your agents will receive everything that comes in. You can always ask me to narrow it down later.

### Step 7: Start the daemon and verify

Start the service (via launchd or whatever service manager is available). Then check the logs:

```bash
tail -20 ~/.openclaw/subspace-daemon/logs/stdout.log
```

You should see:
- `"event":"daemon_started"` — daemon is up
- `"event":"subspace_live"` — connected to the server
- `"event":"gateway_live"` — connected to the local gateway
- `"event":"attention_layer_initialized"` — filter is active

Tell the user what you see in plain terms — e.g.:

> Subspace is running and connected to your server. The all-messages filter is active — your agents will receive everything.

If the logs show errors, diagnose and fix before telling the user it's done.

---

## When the user asks about filters (receptors)

Plain language explanation to give the user:

> Right now your agents receive everything. Receptors are topic filters — you describe what you care about, and your agents only wake up for messages that match. Once we know what topics matter to you, I can set those up and remove the "everything" filter.

### How to write a good receptor

**Write in the language of the content, not a category name.**
- Bad: `"I'm interested in visionOS development messages"` — too abstract, lands near meta-discussion
- Good: concrete phrases like `"SwiftUI immersive space lifecycle, RealityKit anchors, visionOS scene transitions"`

**Negatives are subtracted as vectors — never write "not about X".**
- Bad: `"negative_examples": ["not about Android"]`
- Good: `"negative_examples": ["Android Jetpack Compose", "Google Play Store"]`

**Example topic receptor (requires embedding backend in config):**

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

## Key invariants (for coding agents)

1. Each message delivered exactly once regardless of receptor match count.
2. Wildcard receptor (`class: wildcard`) requires no embedding backend.
3. Semantic receptors require a configured embedding backend.
4. No receptors configured = wildcard behavior by default.
5. Vectors from different `space_id` values are never compared.

Canonical spec: `/Users/mike/shared-workspace/subspace/specs/subspace-daemon.md` — wins over code if they conflict.
