use std::path::Path;

pub fn render_launchd_plist(binary_path: &Path, config_path: &Path, home: &Path) -> String {
    let user = home
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let path = format!(
        "{}/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        home.display()
    );
    let socket_path = home.join(".openclaw/subspace-daemon/daemon.sock");
    let tmux_log_path = home.join(".local/state/subspace-daemon/tmux-daemon.log");
    let daemon_cmd = format!(
        "env -i HOME='{}' USER='{}' PATH='{}' '{}' serve --config '{}' 2>&1 | tee -a '{}'",
        home.display(),
        user,
        path,
        binary_path.display(),
        config_path.display(),
        tmux_log_path.display()
    );
    let supervisor_cmd = format!(
        "TMUX=/opt/homebrew/bin/tmux; SESSION=subspace-daemon-live; ANCHOR=subspace-daemon-anchor; SOCKET='{}'; CMD=\"{}\"; START_GRACE=180; FAILS=0; LAST_START=0; ensure_anchor() {{ \"$TMUX\" has-session -t \"$ANCHOR\" 2>/dev/null || \"$TMUX\" new-session -d -s \"$ANCHOR\" 'sleep 3650d'; }}; start_session() {{ ensure_anchor; /bin/rm -f \"$SOCKET\"; \"$TMUX\" new-session -d -s \"$SESSION\" \"$CMD\"; LAST_START=$(/bin/date +%s); FAILS=0; }}; ensure_anchor; while true; do NOW=$(/bin/date +%s); if ! \"$TMUX\" has-session -t \"$SESSION\" 2>/dev/null; then start_session; sleep 15; continue; fi; if /usr/bin/curl --max-time 10 --fail --unix-socket \"$SOCKET\" http://localhost/healthz >/dev/null 2>&1; then FAILS=0; elif [ $((NOW-LAST_START)) -lt \"$START_GRACE\" ]; then :; else FAILS=$((FAILS+1)); if [ \"$FAILS\" -ge 3 ]; then \"$TMUX\" kill-session -t \"$SESSION\" 2>/dev/null || true; start_session; fi; fi; sleep 30; done",
        socket_path.display(),
        daemon_cmd,
    );

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
  <dict>
    <key>Label</key>
    <string>ai.openclaw.subspace-daemon</string>
    <key>ProgramArguments</key>
    <array>
      <string>/bin/zsh</string>
      <string>-lc</string>
      <string>{}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>WorkingDirectory</key>
    <string>{}</string>
    <key>EnvironmentVariables</key>
    <dict>
      <key>HOME</key>
      <string>{}</string>
      <key>USER</key>
      <string>{}</string>
      <key>PATH</key>
      <string>{}</string>
    </dict>
    <key>StandardOutPath</key>
    <string>{}/.openclaw/subspace-daemon/logs/stdout.log</string>
    <key>StandardErrorPath</key>
    <string>{}/.openclaw/subspace-daemon/logs/stderr.log</string>
  </dict>
</plist>
"#,
        xml_escape(&supervisor_cmd),
        home.display(),
        home.display(),
        user,
        path,
        home.display(),
        home.display()
    )
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn renders_expected_label() {
        let rendered = render_launchd_plist(
            Path::new("/usr/local/bin/subspace-daemon"),
            Path::new("/Users/mike/.openclaw/subspace-daemon/config.json"),
            Path::new("/Users/mike"),
        );
        assert!(rendered.contains("ai.openclaw.subspace-daemon"));
        assert!(rendered.contains("<string>/bin/zsh</string>"));
        assert!(rendered.contains("<string>-lc</string>"));
        assert!(rendered.contains("subspace-daemon-live"));
        assert!(rendered.contains("subspace-daemon-anchor"));
        assert!(rendered.contains("/bin/rm -f"));
        assert!(rendered.contains("/opt/homebrew/bin/tmux"));
        assert!(rendered.contains("/usr/local/bin/subspace-daemon"));
        assert!(rendered.contains("START_GRACE=180"));
        assert!(rendered.contains("FAILS="));
        assert!(rendered.contains("--unix-socket"));
        assert!(rendered.contains("<key>HOME</key>"));
        assert!(rendered.contains("<string>/Users/mike</string>"));
        assert!(rendered.contains("<key>USER</key>"));
        assert!(rendered.contains("<string>mike</string>"));
        assert!(rendered.contains("/Users/mike/.local/bin:/opt/homebrew/bin"));
        assert!(rendered.contains("2&gt;&amp;1"));
        assert!(!rendered.contains("\\\""));
    }
}
