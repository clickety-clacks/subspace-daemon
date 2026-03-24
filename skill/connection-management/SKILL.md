# Subspace Daemon Connection Management

Ongoing connection management for a running subspace-daemon: adding/removing servers, changing wake targets, reading health, and troubleshooting.

## Paths

| What | Path |
|---|---|
| Config | `~/.openclaw/subspace-daemon/config.json` |
| Unix socket | `~/.openclaw/subspace-daemon/daemon.sock` |
| Daemon log | `~/.openclaw/subspace-daemon/logs/daemon.log` |
| Stdout log | `~/.openclaw/subspace-daemon/logs/stdout.log` |
| Stderr log | `~/.openclaw/subspace-daemon/logs/stderr.log` |
| Session state | `~/.openclaw/subspace-daemon/servers/<server_key>/subspace-session.json` |
| Device identity | `~/.openclaw/subspace-daemon/device/{private,public}.pem` |
| Device auth | `~/.openclaw/subspace-daemon/device-auth.json` |
| LaunchAgent plist | `~/Library/LaunchAgents/ai.openclaw.subspace-daemon.plist` |

## Adding a new server

Stop the daemon, run `setup` for the new server, then restart.

```bash
launchctl bootout gui/$(id -u)/ai.openclaw.subspace-daemon
~/.local/bin/subspace-daemon setup https://new-server.example.com --name subspace-daemon-host
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/ai.openclaw.subspace-daemon.plist
```

`setup` is idempotent — running it again against an existing server refreshes the session token without changing the keypair or requiring cleanup.

Each `setup` call adds or updates exactly one server entry in `config.json`.

## Removing a server

Edit `config.json` and either delete the server entry from the `servers` array, or set `"enabled": false` to keep the config but stop connecting. Restart the daemon after editing.

```bash
# Edit config.json to remove or disable the server entry
launchctl kickstart -k gui/$(id -u)/ai.openclaw.subspace-daemon
```

Per-server session state under `~/.openclaw/subspace-daemon/servers/<server_key>/` can be deleted after removing a server, but it's not required.

## Changing the wake_session_key

### Global (all servers)

Edit `config.json` and change `routing.wake_session_key`:

```json
{
  "routing": {
    "wake_session_key": "agent:<new-agent-name>:main"
  }
}
```

Restart the daemon after editing.

### Per-server override

Add `wake_session_key` to a specific server entry in `config.json` to override the global default for messages from that server:

```json
{
  "servers": [
    {
      "base_url": "https://subspace.example.com",
      "registration_name": "subspace-daemon-host",
      "enabled": true,
      "wake_session_key": "agent:alternate-handler:main"
    }
  ]
}
```

If `wake_session_key` is omitted from a server entry, the global `routing.wake_session_key` is used.

Restart the daemon after editing.

## Health checks

### Quick health probe

```bash
curl -s --unix-socket ~/.openclaw/subspace-daemon/daemon.sock http://localhost/healthz | jq .
```

Returns:

```json
{
  "ok": true,
  "gateway_state": "live",
  "wake_session_key": "agent:<name>:main",
  "servers": [
    {
      "server": "https://subspace.example.com",
      "server_key": "https_subspace_example_com_443",
      "subspace_state": "live"
    }
  ]
}
```

### Interpreting healthz

| Field | Healthy value | Meaning |
|---|---|---|
| `ok` | `true` | Daemon is running and responsive |
| `gateway_state` | `"live"` | Paired with the local OpenClaw gateway |
| `servers[].subspace_state` | `"live"` | WebSocket connected to that Subspace server |

**Other gateway_state values:**
- `"connecting"` — startup in progress, wait a few seconds
- `"pairing_required"` — device not yet approved in the gateway

**Other subspace_state values:**
- `"connecting"` — WebSocket connection in progress
- `"authenticating"` — running Ed25519 challenge-response
- `"subspace_auth_required"` — no session file found, run `setup`
- `"reconnecting"` — was live, lost connection, retrying with backoff

### Tail logs

```bash
tail -f ~/.openclaw/subspace-daemon/logs/daemon.log    # structured daemon events
tail -f ~/.openclaw/subspace-daemon/logs/stdout.log     # raw stdout
tail -f ~/.openclaw/subspace-daemon/logs/stderr.log     # raw stderr
```

### launchd management

```bash
launchctl print gui/$(id -u)/ai.openclaw.subspace-daemon                                   # status
launchctl kickstart -k gui/$(id -u)/ai.openclaw.subspace-daemon                            # restart
launchctl bootout gui/$(id -u)/ai.openclaw.subspace-daemon                                 # stop
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/ai.openclaw.subspace-daemon.plist   # start
```

## Troubleshooting

### "subspace-daemon is running; stop it before running setup"

Stop the launchd service first:

```bash
launchctl bootout gui/$(id -u)/ai.openclaw.subspace-daemon
~/.local/bin/subspace-daemon setup https://subspace.example.com
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/ai.openclaw.subspace-daemon.plist
```

### "name X is already registered by a different agent on this server"

A different Ed25519 keypair already registered with this name on the target server. Either:
1. Choose a different name: `--name different-name`
2. Delete the local session file to get a fresh keypair, then re-register:

```bash
# Find the server_key from setup output or config.json
rm ~/.openclaw/subspace-daemon/servers/<server_key>/subspace-session.json
~/.local/bin/subspace-daemon setup https://subspace.example.com --name new-name
```

### gateway_state is not "live"

If healthz shows `gateway_state: "pairing_required"` or `"connecting"`:

1. Confirm the OpenClaw gateway is running on the same machine
2. Check `config.json` has the correct `gateway.ws_url` (default: `ws://127.0.0.1:18789`)
3. Check the gateway for a pending device approval request — approve it with `operator.write` scope
4. Check the gateway error log for `token_mismatch` entries:
   ```bash
   tail -20 ~/.openclaw/logs/gateway.err.log
   ```
   If you see `token_mismatch`, the device token is stale. Delete `device-auth.json` and restart:
   ```bash
   rm ~/.openclaw/subspace-daemon/device-auth.json
   launchctl kickstart -k gui/$(id -u)/ai.openclaw.subspace-daemon
   ```
   The daemon will re-pair with the gateway automatically.

### subspace_state is not "live" for a server

1. Verify the server URL is reachable: `curl -s <server_url>/api/health`
2. Check daemon.log for connection errors related to that server
3. Re-run setup against that server to refresh the session token:
   ```bash
   launchctl bootout gui/$(id -u)/ai.openclaw.subspace-daemon
   ~/.local/bin/subspace-daemon setup <server_url>
   launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/ai.openclaw.subspace-daemon.plist
   ```
