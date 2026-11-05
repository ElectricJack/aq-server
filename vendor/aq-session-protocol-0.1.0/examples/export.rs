//! Deterministic artifact exporter; run from repository root.
use aq_session_protocol::*;
use serde_json::json;
use std::{fs, path::Path};
#[allow(dead_code)]
fn main() {
    let schema = format!(
        "{}\n",
        serde_json::to_string_pretty(&contract_schema()).unwrap()
    );
    let fixture = format!("{}\n", serde_json::to_string_pretty(&fixtures()).unwrap());
    for root in [
        Path::new("protocol/session-v1"),
        Path::new("crates/aq-session-protocol/artifacts"),
    ] {
        fs::create_dir_all(root).unwrap();
        fs::write(root.join("contract.schema.json"), &schema).unwrap();
        fs::write(root.join("fixtures.json"), &fixture).unwrap();
    }
}
pub fn fixtures() -> serde_json::Value {
    let artifact = json!({"id":"fixture","version":"1","digest":Digest::of(b"fixture")});
    let uid = "00000000-0000-0000-0000-000000000001";
    let terminal = json!({"session":{"kind":"remote","node_id":uid,"boot_id":uid,"attempt_id":uid,"assignment_generation":1,"session_id":uid},"terminal_id":uid,"epoch":1});
    let lease = json!({"terminal":terminal,"principal_id":"human:fixture","lease_id":uid,"generation":1,"issued_at_ms":1000,"expires_at_ms":15000,"execution_expires_at_ms":20000});
    let input = json!({"operation_id":uid,"lease":lease,"issued_at_ms":1000,"deadline_ms":2000,"bytes":[104,105]});
    let manifest = json!({"schema_version":1,"catalog":artifact,"modules":[artifact],"agent_profile":artifact,"intelligence_profile":artifact,"execution_profile":artifact,"bindings":{"session":terminal["session"],"project_id":uid,"project_revision":1,"graph_id":uid,"task_id":uid,"task_revision":1,"attempt_id":uid,"assignment_generation":1,"source":artifact,"outcome":artifact,"graph_access_profile":artifact},"model":{"provider":"fixture","model":"synthetic","reasoning":"standard","max_output_tokens":100,"spending_budget":{"currency":"USD","basis":"task_cumulative_estimated_cost","limit_microunits":1000}},"harness":{"harness":"synthetic","cli_version":"1","adapter":artifact,"environment":artifact,"required_capabilities":[{"id":"aq.harness.startup-context","version":1},{"id":"aq.harness.confined-worktree","version":1},{"id":"aq.harness.stop","version":1}]},"credentials":[{"credential_profile":artifact,"purpose":"provider"}],"permissions":{"requested":[],"effective":[],"required":[],"omissions":[]},"context":{"references":[{"artifact":artifact,"provenance":"catalog","delivery":"startup","bytes":7,"tokens":2,"required":true}],"startup_byte_budget":1024,"startup_token_budget":100,"reserved_output_tokens":100,"context_window_tokens":1000},"resources":{"memory_bytes":1048576,"cpu_millis":1000,"process_limit":4,"wall_time_ms":20000,"stop_grace_ms":1000},"terminal":{"host":"tmux","input_enabled":true,"max_input_bytes":4096,"max_pending_input_bytes":32768,"max_frame_bytes":65536,"replay_bytes":1048576},"selection_explanation":artifact});
    let m: LaunchManifest = serde_json::from_value(manifest.clone()).unwrap();
    let digest = canonical_digest(&m).unwrap();
    let binding = json!({"session":terminal["session"],"schema_version":1,"offer_id":uid,"attempt_id":uid,"assignment_generation":1,"execution_spec_digest":Digest::of(b"execution"),"launch_manifest_digest":digest,"graph_access_profile_digest":Digest::of(b"fixture")});
    let offer = json!({"binding":binding,"manifest":manifest,"required_capabilities":[{"id":"aq.profiled-launch","version":1},{"id":"aq.terminal","version":1},{"id":"aq.harness.startup-context","version":1},{"id":"aq.harness.confined-worktree","version":1},{"id":"aq.harness.stop","version":1}]});
    let commands = json!({"schema_version":1,"node_id":uid,"boot_id":uid,"batch_id":uid,"cursor":1,"commands":[{"kind":"input","body":input}]});
    let snapshot = json!({"snapshot_id":uid,"capture_request_id":uid,"terminal":terminal,"through_sequence":2,"columns":80,"rows":24,"renderer":artifact,"state":[104,105],"digest":Digest::of(b"hi")});
    let frame = json!({"terminal":terminal,"sequence":3,"bytes":[33],"byte_length":1,"digest":Digest::of(b"!")});
    let output = json!({"schema_version":1,"node_id":uid,"boot_id":uid,"batch_id":uid,"cursor":1,"frames":[frame],"snapshots":[snapshot],"gaps":[],"receipts":[]});
    let mut extra = manifest.clone();
    extra["secret_value"] = json!("forbidden");
    let mut float = manifest.clone();
    float["model"]["spending_budget"]["limit_microunits"] = json!(1.5);
    let mut no_tmux = manifest.clone();
    no_tmux["terminal"]["host"] = json!("shell");
    let named = |name: &str| json!({"id":name,"version":"1","digest":Digest::of(name.as_bytes())});
    let mut container = manifest.clone();
    container["harness"]["required_capabilities"][1]["id"] =
        json!(CONTAINER_CONFINEMENT_CAPABILITY);
    let mut both = container.clone();
    both["harness"]["required_capabilities"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":NATIVE_CONFINEMENT_CAPABILITY,"version":1}));
    both["selection_explanation"] = named("both_confinements");
    let mut none = manifest.clone();
    none["harness"]["required_capabilities"]
        .as_array_mut()
        .unwrap()
        .remove(1);
    none["selection_explanation"] = named("no_confinement");
    json!({"version":1,"valid_documents":[{"kind":"launch","body":offer},{"kind":"acceptance","body":{"binding":binding,"advertised_capabilities":[{"id":"aq.profiled-launch","version":1},{"id":"aq.terminal","version":1},{"id":"aq.harness.startup-context","version":1},{"id":"aq.harness.confined-worktree","version":1},{"id":"aq.harness.stop","version":1}]}},{"kind":"grant","body":{"binding":binding,"grant_id":uid,"expires_at_ms":2000}},{"kind":"journal","body":{"binding":binding,"grant_id":uid}},{"kind":"commands","body":commands},{"kind":"output","body":output}],"manifest_digest":digest,"invalid_manifests":[extra,float,no_tmux,both,none],"container_manifest":container,"input":input,"snapshot":snapshot,"frame":frame,"semantics":{"same_id_changed_bytes":"conflict","expired_input":"expired","injecting_crash":"delivery_unknown_no_replay","snapshot_covers_through_sequence":2,"first_live_sequence":3,"unknown_required_capability":"unsupported"}})
}
