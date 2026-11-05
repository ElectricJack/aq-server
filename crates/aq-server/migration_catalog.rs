//! Build-time catalog. The sorted migration directory is the only manifest.
use std::fs;
use std::path::{Path, PathBuf};

pub fn discover(directory: &Path) -> Result<Vec<(u32, PathBuf)>, String> {
    let mut paths = fs::read_dir(directory)
        .map_err(|e| format!("read {}: {e}", directory.display()))?
        .map(|entry| entry.map(|e| e.path()).map_err(|e| e.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    paths.retain(|path| path.extension().is_some_and(|ext| ext == "sql"));
    paths.sort();
    validate(&paths)
}

/// Validate in manifest order, never silently reorder a supplied manifest.
pub fn validate(paths: &[PathBuf]) -> Result<Vec<(u32, PathBuf)>, String> {
    if paths.is_empty() {
        return Err("migration catalog is empty".into());
    }
    let mut entries = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let (prefix, slug) = name.split_once('_').unwrap_or(("", ""));
        if prefix.len() != 4
            || !prefix.bytes().all(|b| b.is_ascii_digit())
            || !slug.ends_with(".sql")
            || slug.len() <= 4
            || !slug[..slug.len() - 4]
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            return Err(format!("{name}: expected NNNN_lowercase_description.sql"));
        }
        let number: u32 = prefix.parse().map_err(|e| format!("{name}: {e}"))?;
        let expected = index as u32 + 1;
        if number != expected {
            return Err(format!(
                "{name}: expected migration {expected:04}, found {number:04}; \
                 duplicate, missing or out-of-order migration number"
            ));
        }
        entries.push((number, path.clone()));
    }
    Ok(entries)
}

pub fn rust_source(entries: &[(u32, PathBuf)]) -> String {
    let mut source = String::from("pub(crate) const MIGRATIONS: &[(i32, &str)] = &[\n");
    for (number, path) in entries {
        source.push_str(&format!(
            "    ({number}, include_str!({:?})),\n",
            path.to_str().expect("UTF-8 migration path")
        ));
    }
    source.push_str("];\n");
    let latest = entries.last().expect("validated nonempty catalog").0;
    source.push_str(&format!(
        "pub const STATE_FORMAT_VERSION: u32 = {latest};\n"
    ));
    source.push_str("pub const READABLE_STATE_FORMATS: &[u32] = &[");
    for (number, _) in entries {
        source.push_str(&format!("{number},"));
    }
    source.push_str("];\n");
    source
}
