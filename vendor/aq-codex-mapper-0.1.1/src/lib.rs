//! Pure closed Codex 0.166 recipe metadata. Mapping grants no execution,
//! installation, policy, secret, renderer or runtime qualification.
use anyhow::{Context, Result, ensure};
use aq_session_protocol::{HarnessSettings, ModelSettings};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub const CODEX_CLI_VERSION: &str = "0.166.0";

/// Resolved metadata only; the caller owns selection, policy and authority joins.
#[derive(Clone, Copy)]
pub struct Settings<'a> {
    pub harness: &'a HarnessSettings,
    /// The CLI recipe does not enforce max_output_tokens; the launch-owned
    /// scoped transport and provider qualification remain separate requirements.
    pub model: &'a ModelSettings,
    pub terminal_input_enabled: bool,
}
/// Lexical recipe paths, never evidence of installation or confinement.
pub struct Paths<'a> {
    pub executable: &'a Path,
    pub workspace: &'a Path,
    pub private_home: &'a Path,
}
/// No startup, credential or authority bytes; no filesystem or process methods.
pub struct Recipe {
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
}

/// Check this exact mapper's supported settings, not a complete host plan.
pub fn validate_settings(settings: Settings<'_>) -> Result<()> {
    validate_settings_mode(settings, false)
}
pub fn validate_interactive_settings(settings: Settings<'_>) -> Result<()> {
    validate_settings_mode(settings, true)
}
fn validate_settings_mode(settings: Settings<'_>, interactive: bool) -> Result<()> {
    ensure!(
        settings.harness.required_capabilities.len() <= 64,
        "Codex capability bound"
    );
    ensure!(
        settings.harness.harness.as_str() == "codex"
            && settings.harness.cli_version.as_str() == CODEX_CLI_VERSION
            && settings.model.provider.as_str() == "openai"
            && settings.model.spending_budget.is_none()
            && matches!(settings.model.reasoning.as_str(), "low" | "medium" | "high")
            && settings.terminal_input_enabled == interactive,
        "Codex exec unsupported settings"
    );
    for cap in &settings.harness.required_capabilities {
        ensure!(
            matches!(
                cap.id.as_str(),
                "aq.profiled-launch"
                    | "aq.terminal"
                    | "aq.harness.startup-context"
                    | "aq.harness.confined-worktree"
                    | "aq.harness.stop"
            ) && cap.version == 1,
            "Codex exec unsupported mandatory capability"
        );
    }
    Ok(())
}

pub fn map(settings: Settings<'_>, paths: Paths<'_>) -> Result<Recipe> {
    map_mode(settings, paths, false)
}
/// V1's interactive TUI launch: the exact verified startup is one bounded argv
/// value; the host independently verifies it and leaves stdin on the PTY.
pub fn map_interactive(settings: Settings<'_>, paths: Paths<'_>) -> Result<Recipe> {
    map_mode(settings, paths, true)
}
fn map_mode(settings: Settings<'_>, paths: Paths<'_>, interactive: bool) -> Result<Recipe> {
    validate_settings_mode(settings, interactive)?;
    let Paths {
        executable,
        workspace,
        private_home,
    } = paths;
    ensure!(
        executable.is_absolute() && workspace.is_absolute() && private_home.is_absolute(),
        "Codex recipe requires absolute paths"
    );
    ensure!(
        [executable, workspace, private_home]
            .iter()
            .all(|path| !path.as_os_str().as_encoded_bytes().contains(&0)),
        "Codex recipe path contains NUL"
    );
    let executable = executable.to_owned();
    let mut arguments = if interactive {
        vec![
            "--no-daemon".into(),
            "--sandbox".into(),
            "workspace-write".into(),
            "-m".into(),
            settings.model.model.as_str().into(),
            "--cd".into(),
            workspace.to_str().context("Codex workspace UTF8")?.into(),
        ]
    } else {
        vec![
            "--no-daemon".into(),
            "exec".into(),
            "--ignore-user-config".into(),
            "--ignore-rules".into(),
            "--ephemeral".into(),
            "--json".into(),
            "--color".into(),
            "never".into(),
            "--sandbox".into(),
            "workspace-write".into(),
            "--model".into(),
            settings.model.model.as_str().into(),
            "--cd".into(),
            workspace.to_str().context("Codex workspace UTF8")?.into(),
        ]
    };
    let overrides = [
        format!(
            "model_reasoning_effort={}",
            toml::Value::String(settings.model.reasoning.as_str().into())
        ),
        "approval_policy=\"never\"".into(),
        "project_doc_max_bytes=0".into(),
        "project_doc_fallback_filenames=[]".into(),
        "sandbox_workspace_write.writable_roots=[]".into(),
        "sandbox_workspace_write.exclude_slash_tmp=true".into(),
        "sandbox_workspace_write.exclude_tmpdir_env_var=true".into(),
        "sandbox_workspace_write.network_access=false".into(),
        "shell_environment_policy.inherit=\"none\"".into(),
        // Codex's tool sandbox helper also needs PATH for trusted OS helpers.
        // This is an explicit fixed value, never inherited operator context.
        "shell_environment_policy.set.PATH=\"/usr/bin:/bin\"".into(),
        "features.shell_snapshot=false".into(),
        "features.shell_snapshot_v2=false".into(),
        "allow_login_shell=false".into(),
        "shell_environment_policy.experimental_use_profile=false".into(),
        "agents.enabled=false".into(),
        "features.plugins=false".into(),
        "features.recommended_plugins=false".into(),
        "features.remote_plugin=false".into(),
        "features.plugin_discovery_prefetch=false".into(),
        "features.plugin_hooks=false".into(),
        "features.plugin_sharing=false".into(),
        "features.apps=false".into(),
        "features.enable_mcp_apps=false".into(),
        "features.codex_apps_mcp_2026_07_28=false".into(),
        "features.external_agent_memory_import=false".into(),
        "features.memory_tool=false".into(),
        "memories.use_memories=false".into(),
        "memories.generate_memories=false".into(),
        "web_search=\"disabled\"".into(),
        format!(
            "projects.{}.trust_level={}",
            toml::Value::String(workspace.to_str().unwrap().into()),
            toml::Value::String(if interactive { "trusted" } else { "untrusted" }.into())
        ),
    ];
    for value in overrides {
        arguments.push("--config".into());
        arguments.push(value);
    }
    if interactive {
        arguments.extend([
            "--config".into(),
            "cli_auth_credentials_store=\"file\"".into(),
            "--".into(),
        ]);
    } else {
        arguments.push("-".into());
    }
    let mut environment = BTreeMap::from([(
        "CODEX_HOME".into(),
        private_home.to_str().context("Codex home UTF8")?.into(),
    )]);
    if interactive {
        environment.insert("HOME".into(), private_home.to_str().unwrap().into());
    }
    Ok(Recipe {
        executable,
        arguments,
        environment,
    })
}

/// Config metadata for exact numeric IPv4 loopback. It neither binds nor probes.
#[derive(Clone, Copy)]
pub struct LoopbackResponsesEndpoint {
    port: u16,
}
impl LoopbackResponsesEndpoint {
    pub fn new(port: u16) -> Result<Self> {
        ensure!(port > 0, "Codex scoped endpoint requires nonzero port");
        Ok(Self { port })
    }
}
pub fn responses_overrides(endpoint: LoopbackResponsesEndpoint) -> Vec<String> {
    let origin = format!("http://127.0.0.1:{}", endpoint.port);
    vec![
        format!(
            "openai_base_url={}",
            toml::Value::String(format!("{origin}/v1"))
        ),
        "features.api_key_model_discovery=false".into(),
        "features.responses_websockets=false".into(),
        "features.responses_websockets_v2=false".into(),
        "analytics.enabled=false".into(),
        "otel.exporter=\"none\"".into(),
        "otel.trace_exporter=\"none\"".into(),
        "otel.metrics_exporter=\"none\"".into(),
    ]
}

mod result_local;
pub use result_local::{
    RESULT_LOCAL_RECIPE_DEFINITION, RESULT_LOCAL_RECIPE_ID, ResultChannel,
    interactive_result_local_recipe_artifact, map_interactive_with_result_channel,
    map_with_result_channel, result_local_recipe_artifact,
};

#[cfg(test)]
mod tests;

mod interactive;
pub use interactive::{
    COMPOSER_CLEAR, InteractiveViewport, READY_DELAY_MS, READY_PROMPT_PREFIX,
    SKIP_ESCAPE_BEFORE_ENTER, classify_current_viewport,
};
