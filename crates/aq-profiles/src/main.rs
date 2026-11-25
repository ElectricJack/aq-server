//! `aq-profile validate <directory>`: compile a directory of role, intelligence
//! profile and execution profile documents, as a server would on a save.
use aq_profiles::roles::{DocumentLimits, Kind, SelectionDefaults, SourceDocument, compile};
use std::path::Path;

/// The Markdown files of `directory/<name>` as documents of `kind`, in name order.
fn read(directory: &Path, name: &str, kind: Kind) -> std::io::Result<Vec<SourceDocument>> {
    let folder = directory.join(name);
    if !folder.is_dir() {
        return Ok(Vec::new());
    }
    let mut paths: Vec<_> = std::fs::read_dir(&folder)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<_>>()?;
    paths.retain(|path| path.extension().is_some_and(|e| e == "md"));
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let file = path.file_name().unwrap_or_default().to_string_lossy();
            Ok(SourceDocument {
                kind,
                label: format!("{name}/{file}"),
                text: std::fs::read_to_string(&path)?,
            })
        })
        .collect()
}
fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let [command, directory] = arguments.as_slice() else {
        eprintln!("usage: aq-profile validate <directory>");
        std::process::exit(2);
    };
    if command != "validate" {
        eprintln!("usage: aq-profile validate <directory>");
        std::process::exit(2);
    }
    let directory = Path::new(directory);
    let documents = read_documents(directory);
    match compile(
        &documents,
        &SelectionDefaults::default(),
        &DocumentLimits::default(),
    ) {
        Ok(snapshot) => println!(
            "valid: {} documents, snapshot {}",
            documents.len(),
            snapshot.digest()
        ),
        Err(problems) => {
            for problem in &problems {
                println!("{problem}");
            }
            std::process::exit(3);
        }
    }
}

fn read_documents(directory: &Path) -> Vec<SourceDocument> {
    let mut documents = Vec::new();
    for (name, kind) in [
        ("roles", Kind::Role),
        ("intelligence", Kind::Intelligence),
        ("execution", Kind::Execution),
    ] {
        match read(directory, name, kind) {
            Ok(read) => documents.extend(read),
            Err(error) => {
                eprintln!("aq-profile: {}: {error}", directory.join(name).display());
                std::process::exit(2);
            }
        }
    }
    documents
}
