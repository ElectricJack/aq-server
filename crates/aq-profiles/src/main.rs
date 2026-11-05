use aq_profiles::{Compilation, CompileRequest, compile, load};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
#[derive(Parser)]
#[command(
    name = "aq-profile",
    about = "AQ profile catalog inspection (no daemon required)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
}
#[derive(Subcommand)]
enum ProfileCommand {
    /// Validate the complete catalog and optionally a bounded composition request.
    Validate {
        #[arg(long)]
        catalog: PathBuf,
        #[arg(long)]
        request: Option<PathBuf>,
        #[arg(long)]
        graph_access_profile_digest: Option<String>,
        /// Export the immutable canonical artifact for authenticated central activation.
        #[arg(long)]
        artifact_out: Option<PathBuf>,
    },
    /// Explain composition; concrete intelligence selection remains pending AQ-20.
    Explain {
        #[arg(long)]
        catalog: PathBuf,
        #[arg(long)]
        request: PathBuf,
        #[arg(long)]
        graph_access_profile_digest: String,
    },
}
fn run() -> Result<bool, Box<dyn std::error::Error>> {
    let Command::Profile { command } = Cli::parse().command;
    let (directory, request, access, artifact_out) = match command {
        ProfileCommand::Validate {
            catalog,
            request,
            graph_access_profile_digest,
            artifact_out,
        } => (catalog, request, graph_access_profile_digest, artifact_out),
        ProfileCommand::Explain {
            catalog,
            request,
            graph_access_profile_digest,
        } => (
            catalog,
            Some(request),
            Some(graph_access_profile_digest),
            None,
        ),
    };
    let catalog = load(&directory)?;
    if let Some(path) = artifact_out {
        std::fs::write(path, catalog.canonical_bytes()?)?;
    }
    let result = if let Some(path) = request {
        let request: CompileRequest =
            serde_json::from_str(&aq_profiles::read_bounded_regular(&path, 2_097_152)?)?;
        let result: Compilation = compile(
            &catalog,
            &request,
            &access.ok_or("--graph-access-profile-digest required with --request")?,
        )?;
        let valid = result.composition_valid;
        println!("{}", serde_json::to_string_pretty(&result)?);
        valid
    } else {
        println!(
            "{}",
            serde_json::json!({"catalog_valid":true,"catalog_digest":catalog.digest(),"composition_valid":null,"selection_pending":true})
        );
        true
    };
    Ok(result)
}
fn main() {
    match run() {
        Ok(true) => {}
        Ok(false) => std::process::exit(3),
        Err(e) => {
            eprintln!("aq-profile: {e}");
            std::process::exit(2);
        }
    }
}
