#!/usr/bin/env python3
"""Read-only Argus -> Subspace -> daemon DB freshness monitor."""

from __future__ import annotations

import argparse
import json
import os
import shlex
import subprocess
import sys
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from typing import Any


DEFAULT_ARGUS_DB = "/var/lib/argus/argus.sqlite3"
DEFAULT_DAEMON_DB = "~/.openclaw/subspace-daemon/data/daemon.sqlite3"
DEFAULT_FRESHNESS_MINUTES = 180
DEFAULT_PRODUCER_WINDOW_MINUTES = 24 * 60
DEFAULT_AUTHOR_NAME = "argus-racter-publisher"
DEFAULT_DAEMON_SERVER_KEY = "https_subspace_swarm_channel_443"
DEFAULT_DAEMON_SOCKET = "~/.openclaw/subspace-daemon/daemon.sock"
DEFAULT_ALERT_COMMAND = "~/.local/bin/notify --session agent:main:main --"


@dataclass(frozen=True)
class SqlTarget:
    path: str
    host: str | None = None


@dataclass(frozen=True)
class PublishAttempt:
    attempted_at: str | None
    completed_at: str | None
    status: str | None
    message_id: str | None

    @property
    def event_time(self) -> datetime | None:
        return parse_timestamp(self.completed_at) or parse_timestamp(self.attempted_at)


@dataclass(frozen=True)
class DaemonEvent:
    row_id: int | None
    message_id: str | None
    message_timestamp: str | None
    accepted_at: str | None

    @property
    def event_time(self) -> datetime | None:
        return parse_timestamp(self.accepted_at) or parse_timestamp(self.message_timestamp)


def parse_timestamp(value: str | None) -> datetime | None:
    if not value:
        return None
    normalized = value.strip()
    if normalized.endswith("Z"):
        normalized = normalized[:-1] + "+00:00"
    if " " in normalized and "T" not in normalized:
        normalized = normalized.replace(" ", "T") + "+00:00"
    parsed = datetime.fromisoformat(normalized)
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=UTC)
    return parsed.astimezone(UTC)


def sqlite_json(target: SqlTarget, query: str) -> list[dict[str, Any]]:
    db_path = os.path.expanduser(target.path)
    sqlite_cmd = ["sqlite3", "-readonly", "-json", db_path, query]
    if target.host:
        remote = " ".join(shlex.quote(part) for part in sqlite_cmd)
        cmd = ["ssh", target.host, remote]
    else:
        cmd = sqlite_cmd
    result = subprocess.run(cmd, check=True, capture_output=True, text=True)
    if not result.stdout.strip():
        return []
    return json.loads(result.stdout)


def latest_argus_success(target: SqlTarget) -> PublishAttempt | None:
    rows = sqlite_json(
        target,
        """
        SELECT attempted_at, completed_at, status, subspace_message_id AS message_id
        FROM publish_attempts
        WHERE status = 'succeeded' AND subspace_message_id IS NOT NULL
        ORDER BY COALESCE(completed_at, attempted_at) DESC, rowid DESC
        LIMIT 1
        """,
    )
    if not rows:
        return None
    row = rows[0]
    return PublishAttempt(row.get("attempted_at"), row.get("completed_at"), row.get("status"), row.get("message_id"))


def latest_daemon_event(target: SqlTarget, author_name: str, server_key: str) -> DaemonEvent | None:
    rows = sqlite_json(
        target,
        """
        SELECT de.id AS row_id, de.message_id, de.message_timestamp, de.accepted_at
        FROM daemon_event de
        JOIN ingress_source src ON src.id = de.ingress_source_id
        WHERE de.author_name = {author} AND src.server_key = {server_key}
        ORDER BY de.accepted_at DESC, de.id DESC
        LIMIT 1
        """.format(author=sql_literal(author_name), server_key=sql_literal(server_key)),
    )
    if not rows:
        return None
    row = rows[0]
    return DaemonEvent(row.get("row_id"), row.get("message_id"), row.get("message_timestamp"), row.get("accepted_at"))


def daemon_has_message(target: SqlTarget, message_id: str, author_name: str, server_key: str) -> DaemonEvent | None:
    rows = sqlite_json(
        target,
        """
        SELECT de.id AS row_id, de.message_id, de.message_timestamp, de.accepted_at
        FROM daemon_event de
        JOIN ingress_source src ON src.id = de.ingress_source_id
        WHERE de.author_name = {author} AND de.message_id = {message_id} AND src.server_key = {server_key}
        ORDER BY de.accepted_at DESC, de.id DESC
        LIMIT 1
        """.format(
            author=sql_literal(author_name),
            message_id=sql_literal(message_id),
            server_key=sql_literal(server_key),
        ),
    )
    if not rows:
        return None
    row = rows[0]
    return DaemonEvent(row.get("row_id"), row.get("message_id"), row.get("message_timestamp"), row.get("accepted_at"))


def sql_literal(value: str) -> str:
    return "'" + value.replace("'", "''") + "'"


def daemon_health(host: str | None, socket_path: str) -> dict[str, Any] | None:
    socket = os.path.expanduser(socket_path)
    curl_cmd = ["curl", "-fsS", "--unix-socket", socket, "http://localhost/healthz"]
    if host:
        remote = " ".join(shlex.quote(part) for part in curl_cmd)
        cmd = ["ssh", host, remote]
    else:
        cmd = curl_cmd
    result = subprocess.run(cmd, capture_output=True, text=True)
    if result.returncode != 0:
        return None
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError:
        return None


def classify(
    now: datetime,
    argus: PublishAttempt | None,
    daemon_latest: DaemonEvent | None,
    daemon_match: DaemonEvent | None,
    health: dict[str, Any] | None,
    producer_window: timedelta,
    freshness_window: timedelta,
    daemon_server_key: str,
) -> tuple[bool, str]:
    if not daemon_runtime_healthy(health, daemon_server_key):
        return False, "daemon_runtime_down"
    if argus is None or argus.event_time is None or now - argus.event_time > producer_window:
        return False, "argus_not_publishing"
    if daemon_match is None:
        return False, "swarm_to_daemon_stale"
    if daemon_match.event_time is None or daemon_match.event_time < argus.event_time:
        return False, "swarm_to_daemon_stale"
    if now - daemon_match.event_time > freshness_window:
        return False, "swarm_to_daemon_stale"
    if daemon_latest and daemon_latest.event_time and now - daemon_latest.event_time > freshness_window:
        return False, "swarm_to_daemon_stale"
    return True, "healthy"


def daemon_runtime_healthy(health: dict[str, Any] | None, daemon_server_key: str) -> bool:
    if health is None or health.get("ok") is not True:
        return False
    servers = health.get("servers")
    if not isinstance(servers, list):
        return False
    return any(
        isinstance(server, dict)
        and server.get("server_key") == daemon_server_key
        and server.get("subspace_state") == "live"
        for server in servers
    )


def build_report(args: argparse.Namespace) -> dict[str, Any]:
    now = datetime.now(UTC)
    argus_target = SqlTarget(args.argus_db, args.argus_host)
    daemon_target = SqlTarget(args.daemon_db, args.daemon_host)
    health = daemon_health(args.daemon_host, args.daemon_socket)
    errors: dict[str, str] = {}
    try:
        argus = latest_argus_success(argus_target)
    except (subprocess.CalledProcessError, json.JSONDecodeError) as exc:
        argus = None
        errors["argus"] = safe_error(exc)
    try:
        daemon_latest = latest_daemon_event(daemon_target, args.author_name, args.daemon_server_key)
        daemon_match = (
            daemon_has_message(daemon_target, argus.message_id, args.author_name, args.daemon_server_key)
            if argus and argus.message_id
            else None
        )
    except (subprocess.CalledProcessError, json.JSONDecodeError) as exc:
        daemon_latest = None
        daemon_match = None
        errors["daemon_db"] = safe_error(exc)
    if "daemon_db" in errors:
        ok, root_cause = False, "daemon_runtime_down"
    else:
        ok, root_cause = classify(
            now,
            argus,
            daemon_latest,
            daemon_match,
            health,
            timedelta(minutes=args.producer_window_minutes),
            timedelta(minutes=args.freshness_minutes),
            args.daemon_server_key,
        )
    return {
        "ok": ok,
        "root_cause": root_cause,
        "checked_at": now.isoformat().replace("+00:00", "Z"),
        "freshness_window_minutes": args.freshness_minutes,
        "producer_window_minutes": args.producer_window_minutes,
        "latest_argus_publish": argus.__dict__ if argus else None,
        "latest_daemon_event": daemon_latest.__dict__ if daemon_latest else None,
        "matched_daemon_event": daemon_match.__dict__ if daemon_match else None,
        "daemon_health": summarize_health(health),
        "daemon_server_key": args.daemon_server_key,
        "read_errors": errors,
        "caused_by": "T1393 monitoring miss",
    }


def safe_error(exc: BaseException) -> str:
    if isinstance(exc, subprocess.CalledProcessError):
        stderr = (exc.stderr or "").strip().splitlines()
        return stderr[-1][:240] if stderr else f"command exited {exc.returncode}"
    return exc.__class__.__name__


def summarize_health(health: dict[str, Any] | None) -> dict[str, Any] | None:
    if health is None:
        return None
    return {
        "gateway_state": health.get("gateway_state"),
        "servers": [
            {
                "server": server.get("server"),
                "server_key": server.get("server_key"),
                "subspace_state": server.get("subspace_state"),
            }
            for server in health.get("servers", [])
            if isinstance(server, dict)
        ],
    }


def send_alert(command: str, report: dict[str, Any]) -> None:
    message = (
        "T1394 news pipeline freshness FAILED "
        f"root_cause={report['root_cause']} "
        f"checked_at={report['checked_at']} "
        f"argus_message_id={message_id(report.get('latest_argus_publish'))} "
        f"daemon_row_id={row_id(report.get('latest_daemon_event'))} "
        f"daemon_message_id={message_id(report.get('latest_daemon_event'))}"
    )
    subprocess.run([*shlex.split(os.path.expanduser(command)), message], check=True)


def message_id(row: Any) -> str | None:
    return row.get("message_id") if isinstance(row, dict) else None


def row_id(row: Any) -> int | None:
    return row.get("row_id") if isinstance(row, dict) else None


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--argus-db", default=DEFAULT_ARGUS_DB)
    parser.add_argument("--argus-host")
    parser.add_argument("--daemon-db", default=DEFAULT_DAEMON_DB)
    parser.add_argument("--daemon-host")
    parser.add_argument("--daemon-server-key", default=DEFAULT_DAEMON_SERVER_KEY)
    parser.add_argument("--daemon-socket", default=DEFAULT_DAEMON_SOCKET)
    parser.add_argument("--author-name", default=DEFAULT_AUTHOR_NAME)
    parser.add_argument("--freshness-minutes", type=int, default=DEFAULT_FRESHNESS_MINUTES)
    parser.add_argument("--producer-window-minutes", type=int, default=DEFAULT_PRODUCER_WINDOW_MINUTES)
    parser.add_argument("--alert-command", default=DEFAULT_ALERT_COMMAND)
    parser.add_argument("--no-alert", action="store_true", help="read-only proof mode: report failure without sending an alert")
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    report = build_report(args)
    print(json.dumps(report, indent=2, sort_keys=True))
    if report["ok"]:
        return 0
    if not args.no_alert:
        send_alert(args.alert_command, report)
    return 2


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
