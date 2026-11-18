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
    // aq-worker W1. The recipe digest is not pinned yet: the worker's recipe
    // body was not available when this was written.
    DockerLocalProfile {
        harness: "codex",
        cli_version: "0.166.0",
        adapter: "aq.codex-docker-result-local-adapter",
        profile: "codex-0.166-docker-local-result-local-v1",
        recipe_id: "aq.codex-docker-local-result-local-recipe",
        recipe_digest: None,
    },
];
