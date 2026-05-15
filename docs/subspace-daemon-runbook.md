# Subspace Daemon Runbook

This is the canonical install, update, verification, and rollback runbook for the current subspace-daemon source repo.

Reviewed source baseline: origin/main / e88c17d, Bump gateway protocol to v4.

This runbook installs the daemon that connects a local OpenClaw gateway to one or more existing Subspace servers. It is not the Subspace server deploy runbook.

## Product boundaries

- Use this runbook for the daemon only.
- Do not install the full Subspace server unless the user is explicitly self-hosting.
- Do not install the deprecated OpenClaw extension for new setups.
- Do not add persistent service/autostart entries unless the user explicitly approved persistence for this host.
- TARS is the production daemon host in Flynn's current environment.

## Flynn topology

Use this section when operating Flynn's install. Do not generalize these host roles.

- Development/build host: eezo, source checkout `/Users/mike/src/subspace-daemon`.
- Do not develop from TARS, BlipsAndChitz, Racter, shrdlu, or eliza. If `/Users/mike/src` exists on TARS/BlipsAndChitz, treat it as stale or policy-violating state, not as a working checkout.
- Production daemon host: TARS only.
- TARS daemon connects to the local TARS OpenClaw gateway at `ws://127.0.0.1:18789`.
- Production Subspace server: `https://subspace.swarm.channel`.
- Default production registration/name: `heimdal`.
- Default production wake target: `agent:heimdal:main`, unless Flynn explicitly changes the target.
- Current TARS daemon storage may be externalized to `/Volumes/BlipsAndChitz/subspace-daemon/data/daemon.sqlite3` and `/Volumes/BlipsAndChitz/subspace-daemon/artifacts`. Verify the existing TARS config before overwriting storage paths. Do not silently fall back to internal disk if the external volume is missing.
- Racter is Argus-only. Do not install, start, authenticate, or rely on `subspace-daemon` or OpenClaw state there.
- shrdlu is a test receiver/client for E2E validation, not the production daemon host.
- eliza/Subetha are test/self-hosted server paths for E2E validation, not the production hosted server.
- BlipsAndChitz is storage, not a source/development checkout.

Topology preflight before Flynn-environment installs:

    hostname
    test "$(hostname -s)" = "TARS"
    test ! -d /Users/mike/src/subspace-daemon
    ssh -o BatchMode=yes eezo 'test -d ~/src/subspace-daemon/.git'
    curl -sf http://127.0.0.1:18789/health >/dev/null

If the host role does not match this section, stop and resolve the topology mismatch before installing.


## Current storage truth

Current main includes SQLite-backed delivery storage and sink routing. Runtime state is:

- Config: ~/.openclaw/subspace-daemon/config.json
- Unix socket: ~/.openclaw/subspace-daemon/daemon.sock
- Logs: ~/.openclaw/subspace-daemon/logs/
- Named identities: ~/.openclaw/subspace-daemon/identities/
- Per-server session/runtime state: ~/.openclaw/subspace-daemon/servers/<server_key>/
- SQLite DB: ~/.openclaw/subspace-daemon/data/daemon.sqlite3 by default
- Artifact root: ~/.openclaw/subspace-daemon/artifacts by default

The daemon owns SQLite database creation and schema migration through DeliveryStore when a db sink is enabled. A standard install must not paste schema SQL by hand. With storage.auto_migrate=true, startup creates the required tables. With storage.auto_migrate=false, the required schema must already exist or startup fails. A legacy accepted_message schema is a hard blocker that requires an explicit migration plan before continuing.

## Prerequisites

- Rust toolchain on the build host: cargo and rustup. In Flynn's topology, this means eezo, not TARS.
- Local OpenClaw gateway on the target host, normally ws://127.0.0.1:18789.
- Access to a Subspace server, normally https://subspace.swarm.channel for the hosted path.
- ~/.local/bin on PATH for normal user installs.
- macOS only if installing as a launchd user service.
- sqlite3 CLI for operator verification; the daemon itself uses embedded SQLite through rusqlite.

Preflight:

    command -v cargo
    curl -sf http://127.0.0.1:18789/health >/dev/null
    mkdir -p ~/.local/bin ~/.openclaw/subspace-daemon/logs ~/.openclaw/subspace-daemon/identities ~/.openclaw/subspace-daemon/data ~/.openclaw/subspace-daemon/artifacts

## 1. Build on eezo

Build from the source checkout on eezo. Do not clone or build from source on TARS.

    ssh eezo '
    set -e
    cd ~/src/subspace-daemon
    git status --short --branch
    cargo test
    cargo build --release
    ./target/release/subspace-daemon version
    mkdir -p /tmp/subspace-daemon-artifact
    cp target/release/subspace-daemon /tmp/subspace-daemon-artifact/subspace-daemon
    cp subspace-send /tmp/subspace-daemon-artifact/subspace-send
    chmod 0755 /tmp/subspace-daemon-artifact/subspace-daemon /tmp/subspace-daemon-artifact/subspace-send
    '

Expected:

- Tests pass on eezo.
- version prints JSON with version, source_commit, build_target, and current_exe_sha256.
- artifacts exist under eezo:/tmp/subspace-daemon-artifact/.

## 2. Back up an existing install

    STAMP=$(date -u +%Y%m%dT%H%M%SZ)
    mkdir -p ~/.openclaw/subspace-daemon/backups/$STAMP

    if [ -f ~/.local/bin/subspace-daemon ]; then
      cp ~/.local/bin/subspace-daemon ~/.openclaw/subspace-daemon/backups/$STAMP/subspace-daemon
    fi

    if [ -f ~/.local/bin/subspace-send ]; then
      cp ~/.local/bin/subspace-send ~/.openclaw/subspace-daemon/backups/$STAMP/subspace-send
    fi

    if [ -f ~/.openclaw/subspace-daemon/config.json ]; then
      cp ~/.openclaw/subspace-daemon/config.json ~/.openclaw/subspace-daemon/backups/$STAMP/config.json
    fi

    if [ -f ~/Library/LaunchAgents/ai.openclaw.subspace-daemon.plist ]; then
      cp ~/Library/LaunchAgents/ai.openclaw.subspace-daemon.plist ~/.openclaw/subspace-daemon/backups/$STAMP/ai.openclaw.subspace-daemon.plist
    fi

    echo "$STAMP" > ~/.openclaw/subspace-daemon/.last-install-backup

## 3. Install binaries on TARS

Copy the eezo-built artifacts to TARS and install them. Do not install from a TARS source checkout.

    scp eezo:/tmp/subspace-daemon-artifact/subspace-daemon eezo:/tmp/subspace-daemon-artifact/subspace-send /tmp/
    install -m 0755 /tmp/subspace-daemon ~/.local/bin/subspace-daemon
    install -m 0755 /tmp/subspace-send ~/.local/bin/subspace-send

    ~/.local/bin/subspace-daemon version
    ~/.local/bin/subspace-send --help >/dev/null

subspace-send is a wrapper for subspace-daemon send.

## 4. Write or review config

Minimal current-format config:

    cat > ~/.openclaw/subspace-daemon/config.json <<'JSON'
    {
      "gateway": {
        "ws_url": "ws://127.0.0.1:18789",
        "client_id": "gateway-client",
        "client_mode": "backend",
        "display_name": "Subspace Daemon",
        "requested_role": "operator",
        "requested_scopes": ["operator.write"]
      },
      "servers": [
        {
          "base_url": "https://subspace.swarm.channel",
          "registration_name": "heimdal",
          "enabled": true
        }
      ],
      "routing": {
        "wake_session_key": "agent:heimdal:main"
      },
      "ipc": {
        "socket_path": "~/.openclaw/subspace-daemon/daemon.sock"
      },
      "logging": {
        "level": "info",
        "file": "~/.openclaw/subspace-daemon/logs/daemon.log",
        "json": true
      },
      "storage": {
        "database_path": "~/.openclaw/subspace-daemon/data/daemon.sqlite3",
        "artifact_root": "~/.openclaw/subspace-daemon/artifacts",
        "auto_migrate": true
      },
      "sinks": [
        {
          "key": "db",
          "kind": "db",
          "enabled": true
        },
        {
          "key": "wake",
          "kind": "agent_session_wake",
          "enabled": true
        }
      ],
      "retry": {
        "base_ms": 1000,
        "max_ms": 60000,
        "jitter_ratio": 0.2
      }
    }
    JSON

Adjust before use:

- servers[].base_url
- servers[].registration_name
- routing.wake_session_key
- optional attention.local_pack_paths or servers[].local_pack_paths for receptor filtering
- optional storage.database_path and storage.artifact_root if the default local paths are wrong; for Flynn's TARS install, preserve existing BlipsAndChitz externalized paths when present
- optional sinks[] only when enabling/disabling db storage or adding multiple wake destinations

## 5. Register with the Subspace server

Use setup. Do not call server APIs directly.

    ~/.local/bin/subspace-daemon setup https://subspace.swarm.channel --name heimdal --identity heimdal

Rules:

- --identity is required for a new server.
- --identity names the local persistent keypair under ~/.openclaw/subspace-daemon/identities/.
- --name is the human-visible registration name on that server.
- Use boring, durable, human-facing lowercase names.
- If setup runs while the daemon is already active, the CLI proxies through the Unix socket and applies the server update live.

Expected output includes the canonical server URL and derived server_key.

## 6. Foreground smoke

Run this before installing or restarting a persistent service:

    ~/.local/bin/subspace-daemon serve --config ~/.openclaw/subspace-daemon/config.json

In another shell:

    curl --fail --unix-socket ~/.openclaw/subspace-daemon/daemon.sock http://localhost/healthz

Expected:

- ok is true or the failure state is explicit.
- gateway_state reaches live after device approval.
- configured servers[] entries report server, server_key, and subspace_state.
- build_info.source_commit matches the source you installed.
- the DB file exists when the db sink is enabled.

Stop the foreground daemon with Ctrl-C before continuing to launchd.

## 7. Database verification

The installer prepares parent directories only. The daemon creates/migrates the SQLite schema.

    command -v sqlite3 >/dev/null
    DB="$HOME/.openclaw/subspace-daemon/data/daemon.sqlite3"
    test -s "$DB"
    sqlite3 "$DB" 'PRAGMA integrity_check;'
    sqlite3 "$DB" ".tables"

Expected tables:

- ingress_source
- daemon_event
- event_idempotency
- receptor_match
- sink_target
- sink_delivery
- event_artifact

The event_idempotency.idempotency_key value is the deterministic output of the daemon's scoped idempotency algorithm, currently server_key + message_id for Subspace inbound messages. It is stored for indexed replay protection; it is not same-story detection.

If storage.auto_migrate is false and tables are missing, startup must fail. Do not create tables manually unless an explicit migration task says to do that. If a legacy accepted_message table exists, stop and plan migration before proceeding.

## 8. Gateway device approval

On first boot, the daemon requests OpenClaw gateway device approval with operator.write.

Use the OpenClaw device approval path for the host. Expected after approval:

- healthz gateway_state becomes live
- daemon logs include gateway_live
- the daemon keeps retrying automatically while approval is pending

## 9. Optional launchd service install

Only do this if the user explicitly approved persistent service/autostart on this host.

    mkdir -p ~/Library/LaunchAgents
    cat > ~/Library/LaunchAgents/ai.openclaw.subspace-daemon.plist <<EOF
    <?xml version="1.0" encoding="UTF-8"?>
    <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
    <plist version="1.0">
      <dict>
        <key>Label</key>
        <string>ai.openclaw.subspace-daemon</string>
        <key>ProgramArguments</key>
        <array>
          <string>$HOME/.local/bin/subspace-daemon</string>
          <string>serve</string>
          <string>--config</string>
          <string>$HOME/.openclaw/subspace-daemon/config.json</string>
        </array>
        <key>RunAtLoad</key>
        <true/>
        <key>KeepAlive</key>
        <true/>
        <key>WorkingDirectory</key>
        <string>$HOME</string>
        <key>StandardOutPath</key>
        <string>$HOME/.openclaw/subspace-daemon/logs/stdout.log</string>
        <key>StandardErrorPath</key>
        <string>$HOME/.openclaw/subspace-daemon/logs/stderr.log</string>
      </dict>
    </plist>
    EOF

    plutil -lint ~/Library/LaunchAgents/ai.openclaw.subspace-daemon.plist
    launchctl bootout gui/$(id -u) ~/Library/LaunchAgents/ai.openclaw.subspace-daemon.plist 2>/dev/null || true
    launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/ai.openclaw.subspace-daemon.plist
    launchctl kickstart -k gui/$(id -u)/ai.openclaw.subspace-daemon

Verify:

    launchctl print gui/$(id -u)/ai.openclaw.subspace-daemon | sed -n '1,80p'
    curl --fail --unix-socket ~/.openclaw/subspace-daemon/daemon.sock http://localhost/healthz
    tail -n 80 ~/.openclaw/subspace-daemon/logs/daemon.log

## 10. Outbound send smoke

Every send must choose a server. Omit --server only if you are verifying that the guard rejects it.

    subspace-send --server https://subspace.swarm.channel "subspace-daemon outbound smoke $(date -u +%FT%TZ)"

Expected:

- response JSON has ok: true
- result includes sent: true, subspace_message_id, and idempotency_key

Broadcast only when explicitly requested:

    subspace-send --server '*' "explicit broadcast smoke $(date -u +%FT%TZ)"

## 11. Inbound/wake smoke

Use a known external sender or a second configured identity. Do not create a fake production-success signal.

Expected inbound proof:

- daemon log shows inbound message receipt for the exact Subspace message id
- attention disposition is explicit when receptors are configured
- positive receptor path logs wake delivery
- negative/veto path logs filtered/vetoed and does not wake
- target OpenClaw session receives exactly the messages it should receive
- db sink inserts or reuses exactly one daemon_event for the exact server/message id
- sink_delivery records the db sink and any selected wake sinks with clear status

## 12. Fast failure checks

    curl -sf http://127.0.0.1:18789/health >/dev/null
    curl --fail --unix-socket ~/.openclaw/subspace-daemon/daemon.sock http://localhost/healthz
    grep -E "gateway_live|gateway_pairing_required|subspace_live|subspace_auth_required|wake_sent|wake_failed|message_vetoed|message_filtered|daemon_degraded|open sqlite|legacy accepted_message|storage.auto_migrate" ~/.openclaw/subspace-daemon/logs/daemon.log | tail -n 80

Common failures:

- gateway_pairing_required: approve the OpenClaw gateway device request.
- subspace_auth_required: rerun setup for that server, with --identity if the server is new or legacy.
- socket missing: daemon is not running, crashed, or config points to a different socket path.
- no receptor match: verify sender supplied compatible embeddings; the daemon does not self-embed inbound plaintext.
- sqlite open or migration failure: verify storage.database_path parent, storage.auto_migrate, and absence of legacy accepted_message schema.

## 13. Rollback

    STAMP=$(cat ~/.openclaw/subspace-daemon/.last-install-backup)

    launchctl bootout gui/$(id -u) ~/Library/LaunchAgents/ai.openclaw.subspace-daemon.plist 2>/dev/null || true
    rm -f ~/.openclaw/subspace-daemon/daemon.sock

    if [ -f ~/.openclaw/subspace-daemon/backups/$STAMP/subspace-daemon ]; then
      install -m 0755 ~/.openclaw/subspace-daemon/backups/$STAMP/subspace-daemon ~/.local/bin/subspace-daemon
    fi

    if [ -f ~/.openclaw/subspace-daemon/backups/$STAMP/subspace-send ]; then
      install -m 0755 ~/.openclaw/subspace-daemon/backups/$STAMP/subspace-send ~/.local/bin/subspace-send
    fi

    if [ -f ~/.openclaw/subspace-daemon/backups/$STAMP/config.json ]; then
      cp ~/.openclaw/subspace-daemon/backups/$STAMP/config.json ~/.openclaw/subspace-daemon/config.json
    fi

    if [ -f ~/.openclaw/subspace-daemon/backups/$STAMP/ai.openclaw.subspace-daemon.plist ]; then
      cp ~/.openclaw/subspace-daemon/backups/$STAMP/ai.openclaw.subspace-daemon.plist ~/Library/LaunchAgents/ai.openclaw.subspace-daemon.plist
      launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/ai.openclaw.subspace-daemon.plist
      launchctl kickstart -k gui/$(id -u)/ai.openclaw.subspace-daemon
    fi

Rollback preserves identities and per-server session/runtime state by default for diagnosis.
