import importlib.util
import sqlite3
import sys
import tempfile
import unittest
from datetime import UTC, datetime, timedelta
from pathlib import Path


MODULE_PATH = Path(__file__).resolve().parents[1] / "support" / "news_pipeline_freshness_monitor.py"
SPEC = importlib.util.spec_from_file_location("news_pipeline_freshness_monitor", MODULE_PATH)
monitor = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = monitor
SPEC.loader.exec_module(monitor)


class NewsPipelineFreshnessMonitorTests(unittest.TestCase):
    def test_classifies_missing_argus_publish_separately(self):
        ok, root = monitor.classify(
            datetime(2026, 6, 23, tzinfo=UTC),
            None,
            None,
            None,
            {"ok": True, "gateway_state": "live", "servers": [{"server_key": "https_subspace_swarm_channel_443", "subspace_state": "live"}]},
            timedelta(hours=24),
            timedelta(hours=3),
            "https_subspace_swarm_channel_443",
        )
        self.assertFalse(ok)
        self.assertEqual(root, "argus_not_publishing")

    def test_classifies_runtime_down_before_consumer_stale(self):
        argus = monitor.PublishAttempt("2026-06-23T10:00:00Z", "2026-06-23T10:01:00Z", "succeeded", "m1")
        daemon = monitor.DaemonEvent(7, "old", "2026-06-18T18:23:59Z", "2026-06-18 18:23:59")
        ok, root = monitor.classify(
            datetime(2026, 6, 23, 11, tzinfo=UTC),
            argus,
            daemon,
            None,
            None,
            timedelta(hours=24),
            timedelta(hours=3),
            "https_subspace_swarm_channel_443",
        )
        self.assertFalse(ok)
        self.assertEqual(root, "daemon_runtime_down")

    def test_classifies_successful_publish_missing_from_daemon_as_stale_chain(self):
        argus = monitor.PublishAttempt("2026-06-23T10:00:00Z", "2026-06-23T10:01:00Z", "succeeded", "m1")
        daemon = monitor.DaemonEvent(7, "old", "2026-06-18T18:23:59Z", "2026-06-18 18:23:59")
        ok, root = monitor.classify(
            datetime(2026, 6, 23, 11, tzinfo=UTC),
            argus,
            daemon,
            None,
            {"ok": True, "gateway_state": "live", "servers": [{"server_key": "https_subspace_swarm_channel_443", "subspace_state": "live"}]},
            timedelta(hours=24),
            timedelta(hours=3),
            "https_subspace_swarm_channel_443",
        )
        self.assertFalse(ok)
        self.assertEqual(root, "swarm_to_daemon_stale")

    def test_classifies_matching_fresh_daemon_event_as_healthy(self):
        argus = monitor.PublishAttempt("2026-06-23T10:00:00Z", "2026-06-23T10:01:00Z", "succeeded", "m1")
        daemon = monitor.DaemonEvent(8, "m1", "2026-06-23T10:01:05Z", "2026-06-23 10:01:05")
        ok, root = monitor.classify(
            datetime(2026, 6, 23, 11, tzinfo=UTC),
            argus,
            daemon,
            daemon,
            {"ok": True, "gateway_state": "live", "servers": [{"server_key": "https_subspace_swarm_channel_443", "subspace_state": "live"}]},
            timedelta(hours=24),
            timedelta(hours=3),
            "https_subspace_swarm_channel_443",
        )
        self.assertTrue(ok)
        self.assertEqual(root, "healthy")

    def test_classifies_unhealthy_daemon_health_as_runtime_down(self):
        argus = monitor.PublishAttempt("2026-06-23T10:00:00Z", "2026-06-23T10:01:00Z", "succeeded", "m1")
        daemon = monitor.DaemonEvent(8, "m1", "2026-06-23T10:01:05Z", "2026-06-23 10:01:05")
        ok, root = monitor.classify(
            datetime(2026, 6, 23, 11, tzinfo=UTC),
            argus,
            daemon,
            daemon,
            {"ok": False, "gateway_state": "connecting", "servers": [{"server_key": "https_subspace_swarm_channel_443", "subspace_state": "connecting"}]},
            timedelta(hours=24),
            timedelta(hours=3),
            "https_subspace_swarm_channel_443",
        )
        self.assertFalse(ok)
        self.assertEqual(root, "daemon_runtime_down")

    def test_daemon_queries_scope_to_server_key(self):
        with tempfile.TemporaryDirectory() as tmp:
            db = Path(tmp) / "daemon.sqlite3"
            conn = sqlite3.connect(db)
            conn.executescript(
                """
                CREATE TABLE ingress_source (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    server TEXT NOT NULL,
                    server_key TEXT NOT NULL UNIQUE
                );
                CREATE TABLE daemon_event (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    ingress_source_id INTEGER NOT NULL,
                    message_id TEXT NOT NULL,
                    message_timestamp TEXT NOT NULL,
                    author_name TEXT NOT NULL,
                    accepted_at TEXT NOT NULL
                );
                INSERT INTO ingress_source (server, server_key) VALUES ('wrong', 'wrong_key');
                INSERT INTO ingress_source (server, server_key) VALUES ('swarm', 'https_subspace_swarm_channel_443');
                INSERT INTO daemon_event (ingress_source_id, message_id, message_timestamp, author_name, accepted_at)
                VALUES (1, 'm1', '2026-06-23T10:01:00Z', 'argus-racter-publisher', '2026-06-23 10:01:00');
                INSERT INTO daemon_event (ingress_source_id, message_id, message_timestamp, author_name, accepted_at)
                VALUES (2, 'm2', '2026-06-23T10:02:00Z', 'argus-racter-publisher', '2026-06-23 10:02:00');
                """
            )
            conn.close()

            target = monitor.SqlTarget(str(db))
            latest = monitor.latest_daemon_event(target, "argus-racter-publisher", "https_subspace_swarm_channel_443")
            missing = monitor.daemon_has_message(target, "m1", "argus-racter-publisher", "https_subspace_swarm_channel_443")

            self.assertEqual(latest.message_id, "m2")
            self.assertIsNone(missing)


if __name__ == "__main__":
    unittest.main()
