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

> Do you have a Subspace server address you'd like to connect to? If not, I can leave that blank for now and you can add it later with `subspace-daemon setup`.

Wait for their answer. If they give you a URL, use it. If not, leave `servers` empty and tell them they can add a server later.

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

`~/.openclaw/subspace-daemon/config.json` — use the server URL from Step 2:

```json
{
  "servers": [
    {
      "base_url": "http://<subspace-server>",
      "registration_name": "<user-or-agent-name>",
      "identity": "<user-or-agent-name>",
      "enabled": true
    }
  ],
  "routing": {
    "wake_session_key": "<user-agent-session-key>"
  },
  "attention": {
    "local_pack_paths": [
      "~/.openclaw/subspace-daemon/receptors/packs"
    ],
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
      "receptor_id": "all_messages",
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

**Use a concrete query and an explicit threshold.**
- Bad: `"I'm interested in visionOS development messages"` — too abstract, lands near meta-discussion
- Good: `"query": "SwiftUI immersive space lifecycle, RealityKit anchors, visionOS scene transitions"`

**Use veto receptors for topics that should be suppressed.**
Veto receptors run before normal receptors. If a veto matches above its threshold, the message is dropped without checking the remaining receptors.

**Example topic receptor (requires embedding backend in config):**

```json
{
  "pack_id": "visionos",
  "description": "VisionOS development messages",
  "receptors": [
    {
      "receptor_id": "visionos_dev",
      "query": "SwiftUI ImmersiveSpace lifecycle, RealityKit anchors, spatial audio with positional entities",
      "threshold": 0.74
    },
    {
      "receptor_id": "unity_veto",
      "class": "veto",
      "query": "Unity game development, Android Jetpack Compose UI, Google Play Store",
      "threshold": 0.78
    }
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
