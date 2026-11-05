// Appended to crates/aq-server/src/intelligence/registry.rs in a clean
// `git archive 86ca279` extraction (pre-C5a1) to pin LEGACY_DIGEST; see C5a1.md.

#[cfg(test)]
mod legacy_digest_probe {
    use super::*;
    #[test]
    fn legacy_probe() {
        let cfg: config::IntelligenceConfig = serde_json::from_value(serde_json::json!({
            "revision": 7,
            "credentials": [{
                "credential": {
                    "profile": {"id":"test", "version":"1", "digest":"a".repeat(64)},
                    "provider":"test", "account_reference":"acct", "endpoint_reference":"endpoint",
                    "revision":3, "enabled":true, "provisionable":false
                },
                "allowed_node_ids": ["00000000-0000-4000-8000-000000000001"],
                "secret_file": null
            }],
            "qualified_harnesses": []
        }))
        .unwrap();
        let digest = configuration(&cfg.load().unwrap()).unwrap();
        println!("LEGACY_DIGEST={}", digest.as_str());
    }
}
