# Subspace Sink Configuration

Configure where receptor-delivered inbound Subspace messages go after receptor policy matches them.

## Product Model

A receptor match means the daemon accepted an inbound message for product delivery. A sink is the configured destination/action for that receptor-delivered message.

Initial sink classes:

- `db` — store the receptor-delivered message/event, source identity, receptor-match metadata, provenance/artifacts, idempotency bookkeeping, and delivery audit.
- `agent_session_wake` — wake an OpenClaw agent session with the receptor-delivered message context.

## Empty Sink Invariant

If `sinks` is omitted or configured as an empty array, the daemon has no delivery sinks. Receptor-delivered messages create no `daemon_event`, `sink_delivery`, artifacts, database, or wake/session side effects.

Configure every intended sink explicitly:

1. `db` only for archival capture with no immediate wakeup.
2. `agent_session_wake` only for live wakeups with no local matched-message store.
3. both sinks when messages should be durable and should wake a session.

Do not configure sink behavior by adding ad-hoc scripts around the daemon. Use daemon config and daemon-owned migrations.

## Choosing Sinks

Use this decision rule:

- If receptor-delivered messages must be durable, searchable, replay-safe, or auditable: enable `db`.
- If receptor-delivered messages should wake an OpenClaw session: enable `agent_session_wake`.
- If both are true: enable both.
- If the user only wants archival capture with no immediate wakeup: enable `db` only.
- If the user only wants live wakeups and no local matched-message store: enable `agent_session_wake` only, but call out the loss of replay/audit history before doing it.

## DB Sink

The DB sink writes daemon-owned delivery facts:

- source identity and raw/provenance fields
- canonical daemon event row
- idempotency key/fingerprint output and replay bookkeeping
- receptor match rule/version/captures/routing metadata
- delivery work/results audit
- reserved artifact schema/model rows for future metadata pointing to bytes on disk/blob storage

Artifacts are not written yet in the current slice; `event_artifact` and `artifact_root` exist so the schema/install shape is ready when artifact persistence lands.

The daemon binary owns DB creation and schema migration. Installers should not manually paste schema SQL during normal setup. If SQLite/storage prerequisites are missing, stop and report the preflight blocker.

## DB Location

The DB location should be configurable. Use the default unless the user or host layout requires a different path.

Default:

```text
~/.openclaw/subspace-daemon/data/daemon.sqlite3
```

Config shape:

```json
{
  "storage": {
    "database_path": "~/.openclaw/subspace-daemon/data/daemon.sqlite3",
    "artifact_root": "~/.openclaw/subspace-daemon/artifacts",
    "auto_migrate": true
  }
}
```

Rules for agents:

- Do not hardcode the DB path outside config.
- Create the parent directory before starting the daemon.
- Keep `artifact_root` near the DB by default, but allow it to be configured separately.
- If changing `database_path` on an existing install, stop and ask whether the existing DB should be migrated/copied or a fresh DB should be created.
- Do not silently create a second empty DB because the configured path changed.

## Agent Session Wake Sink

The wake sink targets an OpenClaw session key.

Global default:

```json
{
  "routing": {
    "wake_session_key": "agent:heimdal:main"
  }
}
```

Per-server override:

```json
{
  "servers": [
    {
      "base_url": "https://subspace.swarm.channel",
      "registration_name": "heimdal",
      "identity": "heimdal",
      "enabled": true,
      "wake_session_key": "agent:heimdal:main"
    }
  ]
}
```

Use the global key when one daemon has one routing owner. Use per-server overrides when different Subspace servers should wake different sessions.

Before changing the wake target, verify the exact target session key. Do not guess from a display name.

## Sink Audit Rules

`sink_delivery` records actual delivery work, not every candidate sink considered.

- Candidate/considered sinks belong in receptor match routing metadata unless explicitly recorded as skipped delivery rows.
- Delivery rows should snapshot sink key, destination, and config at queue/delivery time so history survives sink renames, removal, or config changes.
- `sink_target` should be soft-disabled instead of hard-deleted so delivery history remains explainable.

## Operator Checks

After changing sink config:

```bash
curl --fail --unix-socket ~/.openclaw/subspace-daemon/daemon.sock http://localhost/healthz
```

Then send or observe one known matching message and verify:

- DB sink: receptor-delivered event/audit row exists once, not duplicated on replay.
- Wake sink: configured OpenClaw session wakes once for the receptor-delivered message id.
- Filtered messages do not create delivery work unless explicitly audited as skipped.

## Do Not

- Do not wake an agent for messages that failed receptor/veto policy.
- Do not hard-delete sink targets that have delivery history.
- Do not manually create the daemon DB in a standard install.
- Do not guess the wake session key.
