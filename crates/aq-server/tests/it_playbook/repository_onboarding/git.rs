use super::*;

type Outcome = Result<Value, (&'static str, &'static str)>;
fn reply(r: &Value, outcome: Result<Value, (&str, &str)>) -> Vec<u8> {
    let mut out = json!({"protocol_version":1,"request_id":r["request_id"]});
    match outcome {
        Ok(result) => {
            out["ok"] = true.into();
            out["result"] = result;
        }
        Err((code, message)) => {
            out["ok"] = false.into();
            out["error"] =
                json!({"code":code,"message":message,"details":{},"retry":"refresh_inputs"});
        }
    }
    let mut bytes = serde_json::to_vec(&out).unwrap();
    bytes.push(b'\n');
    bytes
}

pub(super) fn fake_git(path: &std::path::Path, fake: Shared) -> tokio::task::JoinHandle<()> {
    let listener = tokio::net::UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let fake = fake.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stream);
                let mut line = Vec::new();
                reader.read_until(b'\n', &mut line).await.unwrap();
                let r: Value = serde_json::from_slice(&line).unwrap();
                let target = {
                    let f = fake.lock().unwrap();
                    format!("refs/heads/{}", f.branch)
                };
                let outcome = answer(&fake, &r, &target);
                let bytes = reply(&r, outcome);
                reader.into_inner().write_all(&bytes).await.unwrap();
            });
        }
    })
}

fn answer(fake: &Shared, r: &Value, target: &str) -> Outcome {
    match r["operation"].as_str().unwrap() {
        "repo.register" => {
            assert_eq!(r["expected_binding_generation"], 0);
            fake.lock().unwrap().registers.push(r["params"].clone());
            if r["params"].get("credential_profile").is_some() {
                Err((
                    "unsupported_feature",
                    "Named credential profiles are not enabled",
                ))
            } else {
                Ok(
                    json!({"repository_id":r["repository_id"],"binding_generation":1,"mode":"central","object_format":"sha1","allowed_refs":[target],"allowed_targets":[target]}),
                )
            }
        }
        "repo.sync" => {
            fake.lock().unwrap().syncs.push(r["params"].clone());
            Ok(
                json!({"observation_id":"obs-1","binding_generation":1,"covered_refs":[target],"refs":{target:HEAD},"availability":"available"}),
            )
        }
        "tree" => {
            assert_eq!(r["params"]["oid"], json!({"algorithm":"sha1","hex":HEAD}));
            Ok(json!({"entries":fake.lock().unwrap().tree,"truncated":false}))
        }
        "repo.show" => show(fake, r, target),
        "repo.observation" => observation(fake, r, target),
        other => panic!("unexpected aq-git operation {other}"),
    }
}
fn show(fake: &Shared, r: &Value, target: &str) -> Outcome {
    let f = fake.lock().unwrap();
    if r["expected_binding_generation"] != f.generation {
        Err(("binding_conflict", "stale"))
    } else {
        let mut shown = json!({"repository_id":r["repository_id"],"binding_generation":f.generation,"mode":"central","object_format":"sha1","identity":"historical-seed","allowed_refs":[target],"allowed_targets":[target],"remote_url":f.remote});
        shown
            .as_object_mut()
            .unwrap()
            .extend(f.show_overrides.clone());
        Ok(shown)
    }
}
fn observation(fake: &Shared, r: &Value, target: &str) -> Outcome {
    let f = fake.lock().unwrap();
    if r["expected_binding_generation"] != f.generation {
        Err(("binding_conflict", "stale"))
    } else {
        assert_eq!(r["params"]["refs"], json!([target]));
        let head = (!f.absent_ref).then_some(HEAD);
        Ok(
            json!({"observation_id":Uuid::new_v4(),"binding_generation":f.generation,
                                "started_at_ms":1000,"completed_at_ms":2000,"availability":"available",
                                "covered_refs":[target],"refs":{target:head},"last_refresh_error":null}),
        )
    }
}
