//! Closed local-result recipe metadata. The caller must already own the exact
//! admitted result channel and readonly client installation. This module has no
//! filesystem, credential, grant, process or authority operations.
use super::*;
use std::path::Component;

pub const RESULT_LOCAL_RECIPE_ID: &str = "codex-0.166-result-local-v1";
pub const RESULT_LOCAL_RECIPE_DEFINITION: &[u8] =
    include_bytes!("../artifacts/result-local-v1.json");
/// Exact public recipe declaration bytes, not runtime qualification or authority.
pub fn result_local_recipe_artifact() -> aq_session_protocol::ArtifactRef {
    aq_session_protocol::ArtifactRef {
        id: aq_session_protocol::Text::new("aq.codex-result-local-recipe")
            .expect("constant recipe id"),
        version: aq_session_protocol::Text::new("1").expect("constant recipe version"),
        digest: aq_session_protocol::Digest::of(RESULT_LOCAL_RECIPE_DEFINITION),
    }
}

/// Lexical paths only. `root` holds worker-owned binding/presence/ack files;
/// only its fixed `submissions` child becomes writable in the tool sandbox.
pub struct ResultChannel<'a> {
    pub root: &'a Path,
    pub client: &'a Path,
}
fn path(value: &Path) -> Result<&str> {
    ensure!(
        value.is_absolute()
            && value.components().count() > 1
            && value.components().count() <= 64
            && value
                .components()
                .all(|p| matches!(p, Component::RootDir | Component::Normal(_)))
            && value.as_os_str().len() <= 2048,
        "local-result recipe requires bounded absolute lexical paths"
    );
    let text = value.to_str().context("local-result path UTF8")?;
    ensure!(!text.contains('\0'), "local-result path NUL");
    // Path::components normalizes internal '.'; reject that spelling too so the
    // recipe records exactly the route checked by the retained owner.
    ensure!(
        !text.ends_with('/') && !text.split('/').skip(1).any(|p| p.is_empty() || p == "."),
        "local-result path must use a normalized route"
    );
    Ok(text)
}
fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

/// Distinct opt-in recipe; `map` and its old byte representation are unchanged.
/// A Scope/result contract/client digest is verified and bound by the actual
/// worker launch owner, never inferred from these paths or this method succeeding.
pub fn map_with_result_channel(
    settings: Settings<'_>,
    paths: Paths<'_>,
    channel: ResultChannel<'_>,
) -> Result<Recipe> {
    validate_channel_paths(&paths, &channel)?;
    apply_channel(map(settings, paths)?, channel, false)
}
pub fn map_interactive_with_result_channel(
    settings: Settings<'_>,
    paths: Paths<'_>,
    channel: ResultChannel<'_>,
) -> Result<Recipe> {
    validate_channel_paths(&paths, &channel)?;
    apply_channel(map_interactive(settings, paths)?, channel, true)
}
fn validate_channel_paths(paths: &Paths<'_>, channel: &ResultChannel<'_>) -> Result<()> {
    path(channel.root)?;
    path(channel.client)?;
    for protected in [paths.workspace, paths.private_home, paths.executable] {
        path(protected)?;
        ensure!(
            !overlaps(channel.root, protected),
            "result root overlaps a protected recipe path"
        );
    }
    ensure!(
        !overlaps(channel.client, channel.root)
            && !overlaps(channel.client, paths.workspace)
            && !overlaps(channel.client, paths.private_home),
        "result client overlaps writable or private recipe roots"
    );
    Ok(())
}
fn apply_channel(
    mut recipe: Recipe,
    channel: ResultChannel<'_>,
    interactive: bool,
) -> Result<Recipe> {
    let ResultChannel { root, client } = channel;
    let root = path(root)?;
    let client = path(client)?;
    let submission = Path::new(root).join("submissions");

    let positions: Vec<_> = recipe
        .arguments
        .windows(2)
        .enumerate()
        .filter(|(_, args)| {
            args[0] == "--config" && args[1] == "sandbox_workspace_write.writable_roots=[]"
        })
        .map(|(index, _)| index + 1)
        .collect();
    ensure!(
        positions.len() == 1,
        "closed Codex writable-root setting absent or duplicated"
    );
    recipe.arguments[positions[0]] = format!(
        "sandbox_workspace_write.writable_roots={}",
        toml::Value::Array(vec![toml::Value::String(path(&submission)?.into())])
    );
    let mut additional = Vec::new();
    for (name, value) in [
        ("AQ_AGENT_RESULT_DIR", root),
        ("AQ_AGENT_RESULT_COMMAND", client),
    ] {
        recipe.environment.insert(name.into(), value.into());
        additional.push("--config".into());
        additional.push(format!(
            "shell_environment_policy.set.{name}={}",
            toml::Value::String(value.into())
        ));
    }
    let end = recipe
        .arguments
        .len()
        .checked_sub(1)
        .context("closed Codex stdin marker absent")?;
    ensure!(
        recipe.arguments[end] == if interactive { "--" } else { "-" },
        "closed Codex stdin marker changed"
    );
    recipe.arguments.splice(end..end, additional);
    ensure!(
        recipe.arguments.len() <= 128 && recipe.arguments.iter().all(|value| value.len() <= 4096),
        "local-result recipe argument bound"
    );
    Ok(recipe)
}

#[cfg(test)]
mod tests;

pub fn interactive_result_local_recipe_artifact() -> aq_session_protocol::ArtifactRef {
    aq_session_protocol::ArtifactRef {
        id: aq_session_protocol::Text::new("aq.codex-interactive-result-local-recipe").unwrap(),
        version: aq_session_protocol::Text::new("1").unwrap(),
        digest: aq_session_protocol::Digest::of(include_bytes!(
            "../artifacts/interactive-result-local-v1.json"
        )),
    }
}
