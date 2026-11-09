use std::collections::BTreeMap;

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodingAgentPreset {
    pub id: String,
    pub display_name: String,
    pub image_ref: String,
    pub command: Vec<String>,
    pub container_home: String,
    pub runtime_environment: BTreeMap<String, String>,
}

pub fn available_agent_presets() -> Vec<CodingAgentPreset> {
    [
        ("openai-codex", "OpenAI Codex", "codex", "exec"),
        ("claude-code", "Claude Code", "claude", "-p"),
        ("opencode", "OpenCode", "opencode", "run"),
    ]
    .into_iter()
    .map(|(id, display_name, executable, argument)| {
        let mut runtime_environment = BTreeMap::from([
            ("CARGO_HOME".to_owned(), "/home/codespace/.cargo".to_owned()),
            (
                "RUSTUP_HOME".to_owned(),
                "/home/codespace/.rustup".to_owned(),
            ),
        ]);
        if id == "openai-codex" {
            runtime_environment.insert(
                "CODEX_HOME".to_owned(),
                "/home/codespace/.agent-home/.codex".to_owned(),
            );
        }
        CodingAgentPreset {
            id: id.to_owned(),
            display_name: display_name.to_owned(),
            image_ref: format!("ghcr.io/sutr-app/{id}:20261025_0"),
            command: vec![executable.to_owned(), argument.to_owned()],
            container_home: "/home/codespace/.agent-home".to_owned(),
            runtime_environment,
        }
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::available_agent_presets;

    #[test]
    fn available_presets_are_complete_agent_profiles() {
        let presets = available_agent_presets();

        assert!(!presets.is_empty());
        let mut ids = HashSet::new();
        assert!(presets.iter().all(|preset| {
            ids.insert(&preset.id)
                && !preset.id.is_empty()
                && !preset.display_name.is_empty()
                && !preset.image_ref.is_empty()
                && !preset.command.is_empty()
                && preset.container_home.starts_with('/')
        }));
        assert!(presets.iter().all(|preset| {
            let command = preset.command.to_vec();
            crate::native_terminal::launch::NativeAgentKind::from_command(&command).is_some()
        }));
    }
}
