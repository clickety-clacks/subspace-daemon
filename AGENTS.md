# AGENTS.md — subspace-daemon

This file is for AI coding agents working in this repository.

## What this repo is

`subspace-daemon` is a standalone Rust daemon that connects to one or more Subspace servers via WebSocket and delivers inbound messages to a local OpenClaw agent (Heimdal) through the gateway.

It ships as a single binary and runs as a persistent launchd/systemd service.

## Before you start

Read the canonical spec:

```
/Users/mike/shared-workspace/subspace/specs/subspace-daemon.md
```

That spec is the source of truth for architecture, invariants, and non-goals.
If something in the code conflicts with the spec, the spec wins — flag the conflict.

## Receptor authoring

If you are writing or modifying receptor packs, read this first:

```
docs/receptor-authoring.md
```

Receptors are semantic vectors. Getting them wrong means catching everything or nothing.
The guide covers the key mistakes: categorical descriptions, elephant-problem negatives, and intersection receptor pitfalls.

## Repo structure

```
src/
  config.rs        — config file schema and server_key derivation
  main.rs          — CLI entry point (serve / setup / send)
  supervisor.rs    — per-server WebSocket manager, delivery router
  gateway/         — gateway auth, device pairing, chat.send
  subspace/        — Subspace auth, firehose WebSocket client
  ipc.rs           — Unix socket IPC for outbound sends
  runtime_store.rs — per-server state persistence
  state_lock.rs    — exclusive lock between serve and setup
  logging.rs       — structured JSON logging
docs/
  receptor-authoring.md  — how to write effective receptor packs
scratch/                 — ephemeral review notes, not source of truth
```

## Key invariants

1. Each message is delivered exactly once regardless of how many receptors match.
2. Receptor matching is optional; plaintext delivery still works without any receptors configured.
3. A wildcard receptor (`class: wildcard`) accepts all messages — use it to get firehose behavior.
4. Plaintext messages must remain fully valid; semantic metadata is additive.
5. Vectors from different `space_id` values are never implicitly compared.

## Uncommitted changes to be aware of

Check `git status` before assuming a clean state. There are known uncommitted edits in:
- `src/gateway/client.rs`
- `README.md`

Do not stash, reset, or discard these without explicit instruction.

## Testing

The daemon has no automated test suite yet. Verification is currently manual:
- Build with `cargo build`
- Run with `~/.local/bin/subspace-daemon serve`
- Test sends via eezo `subspace-ext` session (not TARS-local, which self-filters)
