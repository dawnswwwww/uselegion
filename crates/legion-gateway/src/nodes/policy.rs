//! Node command policy based on platform defaults and gateway configuration.

use legion_core::config::NodesConfig;

/// Check whether a command is allowed for a node with the given platform.
pub fn is_allowed(config: &NodesConfig, platform: &str, command: &str) -> bool {
    config.is_command_allowed(platform, command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use legion_core::config::NodesConfig;

    #[test]
    fn base_commands_allowed_by_default() {
        let config = NodesConfig::default();
        assert!(is_allowed(&config, "ios", "camera.list"));
        assert!(is_allowed(&config, "android", "device.info"));
        assert!(is_allowed(&config, "macos", "system.notify"));
    }

    #[test]
    fn dangerous_commands_blocked_by_default() {
        let config = NodesConfig::default();
        assert!(!is_allowed(&config, "ios", "camera.snap"));
        assert!(!is_allowed(&config, "android", "sms.send"));
        assert!(!is_allowed(&config, "macos", "screen.record"));
    }

    #[test]
    fn allow_commands_overrides_dangerous() {
        let config = NodesConfig {
            allow_commands: vec!["camera.snap".to_string()],
            ..Default::default()
        };
        assert!(is_allowed(&config, "ios", "camera.snap"));
    }

    #[test]
    fn deny_commands_wins_over_allow() {
        let config = NodesConfig {
            allow_commands: vec!["camera.snap".to_string()],
            deny_commands: vec!["camera.snap".to_string()],
            ..Default::default()
        };
        assert!(!is_allowed(&config, "ios", "camera.snap"));
    }

    #[test]
    fn canvas_blocked_on_linux() {
        let config = NodesConfig::default();
        assert!(!is_allowed(&config, "linux", "canvas.present"));
        assert!(is_allowed(&config, "windows", "canvas.present"));
    }
}
