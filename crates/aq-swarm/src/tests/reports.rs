use super::*;
#[test]
fn machine_fixtures_round_trip_and_validate() {
    for text in [LINUX, WINDOWS] {
        let report: MachineReport = serde_json::from_str(text).unwrap();
        assert_eq!(report.validate(), Ok(()));
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::from_str::<serde_json::Value>(text).unwrap()
        );
    }
}
#[test]
fn machine_validate_rejects_each_rule() {
    let base: MachineReport = serde_json::from_str(LINUX).unwrap();
    let case = |edit: &dyn Fn(&mut MachineReport)| {
        let mut r = base.clone();
        edit(&mut r);
        r.validate()
    };
    type Edit = fn(&mut MachineReport);
    let invalid: &[(Edit, &str)] = &[
        (|r| r.schema_version = 2, "schema_version"),
        (|r| r.machine.key = Some(String::new()), "key"),
        (|r| r.machine.key = Some("Upper".into()), "key"),
        (|r| r.machine.key = Some("-lead".into()), "key"),
        (|r| r.machine.key = Some("a".repeat(64)), "key"),
        (|r| r.machine.label = Some(String::new()), "label"),
        (|r| r.machine.label = Some("a\nb".into()), "label"),
        (|r| r.machine.label = Some("a".repeat(65)), "label"),
        (|r| r.logical_cpus = 0, "logical_cpus"),
        (|r| r.logical_cpus = 4097, "logical_cpus"),
        (|r| r.memory_total_bytes = 0, "memory"),
        (|r| r.memory_used_bytes = r.memory_total_bytes + 1, "memory"),
        (|r| r.cpu_busy_bps = 10_001, "cpu_busy_bps"),
        (|r| r.disk_free_bytes = None, "disk"),
        (
            |r| r.disk_free_bytes = Some(r.disk_total_bytes.unwrap() + 1),
            "disk",
        ),
    ];
    for &(edit, field) in invalid {
        assert_eq!(case(&edit), Err(field));
    }
}
#[test]
fn memory_used_bps_rounds_down() {
    let mut r: MachineReport = serde_json::from_str(LINUX).unwrap();
    r.memory_total_bytes = 3;
    r.memory_used_bytes = 1;
    assert_eq!(r.memory_used_bps(), 3333);
}
#[test]
fn worker_machine_schema_matches_protocol() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../protocol/worker-machine/v1/schema.json"
    );
    let want = serde_json::to_string_pretty(&schemars::schema_for!(MachineReport)).unwrap() + "\n";
    if std::env::var_os("AQ_WRITE_PROTOCOL").is_some() {
        std::fs::write(path, &want).unwrap();
    }
    assert_eq!(std::fs::read_to_string(path).unwrap(), want);
}
