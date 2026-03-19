use std::path::Path;

pub fn render_launchd_plist(binary_path: &Path, config_path: &Path, home: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
  <dict>
    <key>Label</key>
    <string>ai.openclaw.subspace-daemon</string>
    <key>ProgramArguments</key>
    <array>
      <string>{}</string>
      <string>serve</string>
      <string>--config</string>
      <string>{}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>WorkingDirectory</key>
    <string>{}</string>
    <key>StandardOutPath</key>
    <string>{}/.openclaw/subspace-daemon/logs/stdout.log</string>
    <key>StandardErrorPath</key>
    <string>{}/.openclaw/subspace-daemon/logs/stderr.log</string>
  </dict>
</plist>
"#,
        binary_path.display(),
        config_path.display(),
        home.display(),
        home.display(),
        home.display()
    )
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
        assert!(rendered.contains("/usr/local/bin/subspace-daemon"));
        assert!(!rendered.contains("\\\""));
    }
}
