use super::*;

#[test]
fn snapshot_digest_ignores_order_and_insignificant_whitespace() {
    let documents = default_documents();
    let snapshot = compile(&documents, &defaults(), &limits()).unwrap();
    assert_eq!(snapshot.digest().len(), 64);
    assert_eq!(
        snapshot.digest(),
        aq_session_protocol::Digest::of(snapshot.canonical_bytes()).as_str()
    );
    let mut reordered = documents.clone();
    reordered.reverse();
    assert_eq!(
        compile(&reordered, &defaults(), &limits()).unwrap(),
        snapshot
    );

    let spaced: Vec<_> = documents
        .iter()
        .map(|d| SourceDocument {
            kind: d.kind,
            label: format!("other-{}", d.label),
            text: d
                .text
                .replace("operations: [task.read, ", "operations:   [ task.read ,")
                .replace("---\n## ", "---\n\n\n## ")
                .replace("\n- ", "\n\n  - ")
                .replace("| anthropic |", "|anthropic   |")
                .replace("|---|---|---|---|", "| --- | :-- | --: | - |")
                .replace("name: ", "name:    ")
                .replace('\n', "\r\n")
                + "\r\n\r\n",
        })
        .collect();
    // Indenting list items also touched the role's `## Rules` prose, which is content.
    let digest_of = |sources: &[SourceDocument]| {
        compile(sources, &defaults(), &limits())
            .unwrap()
            .digest()
            .to_string()
    };
    let profiles_only = |sources: &[SourceDocument]| -> Vec<SourceDocument> {
        sources
            .iter()
            .map(|d| match d.kind {
                Kind::Role => documents
                    .iter()
                    .find(|o| d.label.ends_with(&o.label))
                    .unwrap()
                    .clone(),
                _ => d.clone(),
            })
            .collect()
    };
    assert_eq!(digest_of(&profiles_only(&spaced)), snapshot.digest());
    let role_spaced: Vec<_> = documents
        .iter()
        .map(|d| SourceDocument {
            text: d
                .text
                .replace("operations: [task.read, ", "operations:   [ task.read ,")
                .replace("---\n## Role\n", "---\n\n## Role   \n\n\n")
                .replace('\n', "\r\n")
                + "\r\n",
            ..d.clone()
        })
        .collect();
    assert_eq!(digest_of(&role_spaced), snapshot.digest());
}
#[test]
fn snapshot_digest_changes_with_any_content() {
    let base = validate(set(), &defaults(), &limits()).unwrap();
    assert_eq!(
        base,
        compile(&default_documents(), &defaults(), &limits()).unwrap()
    );
    let edits: Vec<fn(&mut DocumentSet)> = vec![
        |s| s.roles[0].role_md.push('.'),
        |s| s.roles[0].rules_md.push('.'),
        |s| s.roles[0].name.push('x'),
        |s| s.roles[0].description.push('x'),
        |s| s.roles[1].enabled = false,
        |s| s.roles[0].operations.reverse(),
        |s| s.roles[1].operations.push("wait.register".into()),
        |s| s.intelligence[3].candidates[0].reasoning = "max".into(),
        |s| s.intelligence[3].candidates[0].model.push('x'),
        |s| s.intelligence[3].selection.max_attempts = Some(3),
        |s| s.intelligence[3].id.push('x'),
        |s| s.execution[0].limits.wall_time_ms += 1,
        |s| s.execution[0].confinement = Confinement::ContainerPermitted,
        |s| s.execution[0].environment = env("aq-ms1-docker-local-claudex"),
        |s| {
            s.execution[0]
                .credentials
                .insert("claude".into(), "other".into());
        },
    ];
    let mut seen = std::collections::BTreeSet::from([base.digest().to_string()]);
    for (index, edit) in edits.iter().enumerate() {
        let mut s = set();
        edit(&mut s);
        let changed = validate(s, &defaults(), &limits()).unwrap();
        assert!(seen.insert(changed.digest().to_string()), "edit {index}");
    }
    // Outer whitespace of a prompt is not content.
    let mut s = set();
    s.roles[0].role_md = format!("\r\n{}\n\n", s.roles[0].role_md);
    assert_eq!(
        validate(s, &defaults(), &limits()).unwrap().digest(),
        base.digest()
    );
}
#[test]
fn snapshot_reads_back_only_from_its_canonical_bytes() {
    let snapshot = compile(&default_documents(), &defaults(), &limits()).unwrap();
    let bytes = snapshot.canonical_bytes();
    assert_eq!(
        Snapshot::from_canonical_bytes(bytes, &limits()).unwrap(),
        snapshot
    );
    let text = std::str::from_utf8(bytes).unwrap();
    // No entry has a version and nothing is named `id@version`.
    assert!(!text.contains('@'));
    for kind in ["roles", "intelligence", "execution"] {
        let value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        for entry in value[kind].as_object().unwrap().values() {
            assert!(entry.get("version").is_none());
        }
    }
    let refused = |bytes: &str| {
        Snapshot::from_canonical_bytes(bytes.as_bytes(), &limits()).unwrap_err()[0].to_string()
    };
    assert_eq!(
        refused(&format!(" {text}")),
        "snapshot: bytes are not canonical"
    );
    assert_eq!(
        refused(&text.replace("\"schema_version\":1", "\"schema_version\":2")),
        "snapshot: has an unknown schema version"
    );
    assert!(refused("{}").starts_with("snapshot: is not readable: "));
    assert_eq!(
        refused(&text.replace("\"execution\":\"sandbox\"", "\"execution\":\"gone\"")),
        "role `developer`: execution: no execution profile `gone` exists"
    );
}
