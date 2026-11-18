//! The docker-local sandbox executors: a harness the node always runs inside a
//! node-owned container, driven without terminal input in the long phase. One
//! table names each exact adapter, result profile and recipe, so manifest
//! admission and the server's result startup cannot disagree.

/// One docker-local executor. Matching is by exact value, never by prefix.
pub struct DockerLocalProfile {
    pub harness: &'static str,
    pub cli_version: &'static str,
    /// The result adapter's artifact id (not the interactive Codex adapter).
    pub adapter: &'static str,
    /// The result-local profile its adapter definition names.
    pub profile: &'static str,
    pub recipe_id: &'static str,
    /// Exact recipe digest. `None` pins the recipe by id and version only.
    pub recipe_digest: Option<&'static str>,
}

pub const DOCKER_LOCAL_PROFILES: [DockerLocalProfile; 2] = [
    // MS1: aq-worker `docker_local_recipe_artifact()` (`aq-worker ms1-definitions`).
    DockerLocalProfile {
        harness: "claude",
        cli_version: "2.1.318",
        adapter: "aq.claude-result-local-adapter",
        profile: "claude-2.1.318-docker-local-result-local-v1",
        recipe_id: "aq.claude-docker-local-result-local-recipe",
        recipe_digest: Some("b30030fb9192268da6079f59d650bcbad5dae3b2e34bf28af2b9e96e9fad8765"),
    },
    // aq-worker W1: `aq_codex_mapper::docker_local::recipe_artifact()`
    // (`codex_recipe` in `aq-worker ms1-definitions`).
    DockerLocalProfile {
        harness: "codex",
        cli_version: "0.166.0",
        adapter: "aq.codex-docker-result-local-adapter",
        profile: "codex-0.166-docker-local-result-local-v1",
        recipe_id: "aq.codex-docker-local-result-local-recipe",
        recipe_digest: Some("64d136db5bfcb9a9c35f5930d0c37dc3647c04f293f7f95e6ea395e4bfd9b150"),
    },
];
