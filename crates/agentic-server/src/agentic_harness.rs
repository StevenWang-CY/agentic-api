use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub struct HarnessEnv {
    pub environment: BTreeMap<String, String>,
    pub environment_remove: Vec<String>,
    pub files: Vec<PathBuf>,
    pub summary: String,
}

pub const CLAUDE_CANONICAL_MODEL: &str = "claude-sonnet-4-5-20250929";

/// Write an isolated Codex home for an Agentic API session.
///
/// # Errors
///
/// Returns an I/O error when the temporary home or generated files cannot be written.
pub fn prepare_codex_home(
    root: &Path,
    gateway_url: &str,
    model: &str,
    api_key: Option<&str>,
) -> Result<HarnessEnv, io::Error> {
    fs::create_dir_all(root)?;
    let gateway_url = format!("{}/v1", gateway_url.trim_end_matches('/'));
    let catalog_path = root.join("model_catalog.json");
    let config_path = root.join("config.toml");
    let catalog = serde_json::json!({
        "models": [{
            "slug": model,
            "display_name": model,
            "supported_in_api": true,
            "visibility": "list",
            "priority": 0,
            "input_modalities": ["text"],
            "default_reasoning_level": "medium",
            "supported_reasoning_levels": [
                {"effort": "low", "description": "Fast responses"},
                {"effort": "medium", "description": "Balanced responses"},
                {"effort": "high", "description": "Deep reasoning"}
            ],
            "supports_reasoning_summaries": true,
            // The gateway accepts parallel_tool_calls=true but serializes tool calls
            // upstream (#190, #197), so do not advertise parallel execution to Codex.
            "supports_parallel_tool_calls": false,
            // apply_patch_tool_type is intentionally omitted: Codex only supports
            // "freeform", which the gateway cannot normalize while preserving
            // constrained decoding. Codex falls back to editing via the shell tool.
            "web_search_tool_type": "text",
            "shell_type": "local",
            "context_window": 32768,
            "max_context_window": 262_144,
            "base_instructions": "",
            "support_verbosity": false,
            "supports_image_detail_original": false,
            "use_responses_lite": false,
            "supports_search_tool": false,
            "include_skills_usage_instructions": false,
            "truncation_policy": {"limit": 32768, "mode": "tokens"},
            "experimental_supported_tools": []
        }]
    });
    let catalog_bytes = serde_json::to_vec_pretty(&catalog).map_err(io::Error::other)?;
    fs::write(&catalog_path, catalog_bytes).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "failed to write Codex model catalog {}: {error}",
                catalog_path.display()
            ),
        )
    })?;

    let requires_auth = api_key.is_some();
    let config = format!(
        "model = \"{}\"\nmodel_provider = \"agentic-api\"\nmodel_catalog_json = \"{}\"\n\n\
[model_providers.agentic-api]\nname = \"Agentic API\"\nbase_url = \"{}\"\n\
wire_api = \"responses\"\nrequires_openai_auth = {requires_auth}\nsupports_websockets = true\n",
        toml_escape(model),
        toml_escape(&catalog_path.display().to_string()),
        toml_escape(&gateway_url),
    );
    fs::write(&config_path, config).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to write Codex config {}: {error}", config_path.display()),
        )
    })?;

    let mut environment = BTreeMap::new();
    environment.insert("CODEX_HOME".to_owned(), root.display().to_string());
    if let Some(api_key) = api_key {
        environment.insert("OPENAI_API_KEY".to_owned(), api_key.to_owned());
    }

    Ok(HarnessEnv {
        environment,
        environment_remove: Vec::new(),
        files: vec![config_path, catalog_path],
        summary: format!("Codex home: {} (model: {model})", root.display()),
    })
}

/// Write an isolated Claude Code configuration for an Agentic API session.
///
/// # Errors
///
/// Returns an I/O error when the temporary configuration directory or settings file cannot be written.
pub fn prepare_claude_home(
    root: &Path,
    gateway_url: &str,
    model: &str,
    api_key: Option<&str>,
) -> Result<HarnessEnv, io::Error> {
    fs::create_dir_all(root)?;
    let settings_path = root.join("settings.json");
    let settings = serde_json::json!({
        "modelOverrides": {
            CLAUDE_CANONICAL_MODEL: model,
        }
    });
    let settings_bytes = serde_json::to_vec_pretty(&settings).map_err(io::Error::other)?;
    fs::write(&settings_path, settings_bytes).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "failed to write Claude Code settings {}: {error}",
                settings_path.display()
            ),
        )
    })?;

    let auth_token = api_key.unwrap_or("agentic-api-local");
    let mut environment = BTreeMap::from([
        (
            "ANTHROPIC_BASE_URL".to_owned(),
            gateway_url.trim_end_matches('/').to_owned(),
        ),
        ("ANTHROPIC_API_KEY".to_owned(), auth_token.to_owned()),
        ("ANTHROPIC_AUTH_TOKEN".to_owned(), auth_token.to_owned()),
        ("CLAUDE_CONFIG_DIR".to_owned(), root.display().to_string()),
        ("CLAUDE_CODE_MAX_CONTEXT_TOKENS".to_owned(), "32768".to_owned()),
        ("CLAUDE_CODE_MAX_OUTPUT_TOKENS".to_owned(), "2048".to_owned()),
        ("MAX_THINKING_TOKENS".to_owned(), "0".to_owned()),
    ]);
    if let Some(api_key) = api_key {
        environment.insert("OPENAI_API_KEY".to_owned(), api_key.to_owned());
    }
    Ok(HarnessEnv {
        environment,
        environment_remove: [
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_FOUNDRY",
            "CLAUDE_CODE_USE_ANTHROPIC_AWS",
            "CLAUDE_CODE_USE_MANTLE",
            "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST",
            "ANTHROPIC_VERTEX_PROJECT_ID",
            "CLOUD_ML_REGION",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "ANTHROPIC_MODEL",
            "ANTHROPIC_SMALL_FAST_MODEL",
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        files: vec![settings_path],
        summary: format!(
            "Claude Code config: {} (gateway: {}, model: {model})",
            root.display(),
            gateway_url.trim_end_matches('/')
        ),
    })
}

fn toml_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\"', "\\\"")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{prepare_claude_home, prepare_codex_home};

    #[test]
    fn codex_config_is_isolated_and_contains_gateway_provider() {
        let root = unique_temp_dir("codex");
        let env = prepare_codex_home(&root, "http://127.0.0.1:3000", "Qwen/test", None).expect("config");
        let config = fs::read_to_string(root.join("config.toml")).expect("config file");

        assert!(config.contains("[model_providers.agentic-api]"));
        assert!(config.contains("base_url = \"http://127.0.0.1:3000/v1\""));
        assert!(config.contains("wire_api = \"responses\""));
        assert!(config.contains("requires_openai_auth = false"));
        assert!(config.contains("model = \"Qwen/test\""));
        assert!(!env.summary.contains("secret"));

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn claude_home_is_isolated_and_maps_the_canonical_model() {
        let root = unique_temp_dir("claude");
        let env = prepare_claude_home(&root, "http://127.0.0.1:3000", "Qwen/test", Some("secret-key"))
            .expect("Claude config");
        let settings = fs::read_to_string(root.join("settings.json")).expect("settings file");
        let settings: serde_json::Value = serde_json::from_str(&settings).expect("valid settings JSON");

        assert_eq!(
            env.environment.get("ANTHROPIC_BASE_URL"),
            Some(&"http://127.0.0.1:3000".to_owned())
        );
        assert_eq!(
            env.environment.get("CLAUDE_CONFIG_DIR"),
            Some(&root.display().to_string())
        );
        assert_eq!(
            env.environment.get("CLAUDE_CODE_MAX_CONTEXT_TOKENS"),
            Some(&"32768".to_owned())
        );
        assert_eq!(
            env.environment.get("CLAUDE_CODE_MAX_OUTPUT_TOKENS"),
            Some(&"2048".to_owned())
        );
        assert_eq!(env.environment.get("MAX_THINKING_TOKENS"), Some(&"0".to_owned()));
        assert_eq!(env.environment.get("ANTHROPIC_API_KEY"), Some(&"secret-key".to_owned()));
        assert_eq!(
            env.environment.get("ANTHROPIC_AUTH_TOKEN"),
            Some(&"secret-key".to_owned())
        );
        assert_eq!(settings["modelOverrides"]["claude-sonnet-4-5-20250929"], "Qwen/test");
        assert!(env.environment_remove.contains(&"CLAUDE_CODE_USE_VERTEX".to_owned()));
        assert!(
            env.environment_remove
                .contains(&"CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST".to_owned())
        );
        assert!(
            env.environment_remove
                .contains(&"CLAUDE_CODE_USE_ANTHROPIC_AWS".to_owned())
        );
        assert!(env.environment_remove.contains(&"CLAUDE_CODE_USE_MANTLE".to_owned()));
        assert!(
            env.environment_remove
                .contains(&"ANTHROPIC_VERTEX_PROJECT_ID".to_owned())
        );
        assert!(env.environment_remove.contains(&"CLOUD_ML_REGION".to_owned()));
        assert!(env.environment_remove.contains(&"ANTHROPIC_MODEL".to_owned()));
        assert!(!env.summary.contains("secret-key"));

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn claude_home_uses_a_local_placeholder_without_inherited_auth() {
        let root = unique_temp_dir("claude-no-key");
        let env = prepare_claude_home(&root, "http://127.0.0.1:3000", "Qwen/test", None).expect("Claude config");

        assert_eq!(
            env.environment.get("ANTHROPIC_API_KEY"),
            Some(&"agentic-api-local".to_owned())
        );
        assert_eq!(
            env.environment.get("ANTHROPIC_AUTH_TOKEN"),
            Some(&"agentic-api-local".to_owned())
        );
        assert!(!env.environment.contains_key("OPENAI_API_KEY"));

        fs::remove_dir_all(root).expect("cleanup");
    }

    fn unique_temp_dir(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("agentic-api-{name}-{}", std::process::id()));
        fs::create_dir_all(&path).expect("temp dir");
        path
    }
}
