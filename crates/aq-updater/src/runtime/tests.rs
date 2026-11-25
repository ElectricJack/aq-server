mod argv;
mod backup;
mod diagnostics;
mod release_env;
mod release_image;

use super::*;
use crate::config::ComposeConfig;

const IMAGE: &str = "ghcr.io/electricjack/agent-q-server@sha256:0000000000000000000000000000000000000000000000000000000000000100";

fn cli() -> DockerCli {
    DockerCli::new(
        ComposeConfig {
            project_dir: "/opt/agent-q".into(),
            files: vec![
                "/opt/agent-q/compose.yaml".into(),
                "/opt/agent-q/compose.prod.yaml".into(),
            ],
            env_files: vec!["/etc/agent-q/compose.env".into()],
            project_name: "agent-q".into(),
        },
        Path::new("/var/lib/agent-q/updater"),
        "ghcr.io/electricjack/agent-q-release".into(),
        "stable".into(),
    )
}
