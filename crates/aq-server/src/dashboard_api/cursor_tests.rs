use super::*;
use serde_json::json;
fn binding() -> Binding {
    Binding {
        principal: Uuid::new_v4(),
        authorization_partition: "1".into(),
        scope: json!({"kind":"all_authorized"}),
        filters: json!({"q":"normalized"}),
        sort: "created_at,id".into(),
        page_size: 25,
        resource: "sessions".into(),
    }
}
#[test]
fn shared_cursor_binds_every_dimension_and_expires_with_restart() {
    let codec = CursorCodec::default();
    let b = binding();
    let token = codec
        .encode(&b, "snapshot-1", "offset-2", 100, 200)
        .unwrap();
    let decoded = codec.decode(&token, &b, 150).unwrap();
    assert_eq!(decoded.snapshot, "snapshot-1");
    assert_eq!(decoded.position, "offset-2");
    for changed in [
        Binding {
            principal: Uuid::new_v4(),
            ..b.clone()
        },
        Binding {
            authorization_partition: "2".into(),
            ..b.clone()
        },
        Binding {
            scope: json!({"kind":"project","project_id":Uuid::new_v4()}),
            ..b.clone()
        },
        Binding {
            filters: json!({"q":"changed"}),
            ..b.clone()
        },
        Binding {
            sort: "id".into(),
            ..b.clone()
        },
        Binding {
            page_size: 20,
            ..b.clone()
        },
        Binding {
            resource: "projects".into(),
            ..b.clone()
        },
    ] {
        assert!(codec.decode(&token, &changed, 150).is_err());
    }
    assert_eq!(
        codec.decode(&token, &b, 200).unwrap_err(),
        CursorError::RestartRequired
    );
    assert!(codec.decode(&token, &b, 99).is_err());
    assert!(CursorCodec::default().decode(&token, &b, 150).is_err());
    assert!(codec.decode("bad", &b, 150).is_err());
    assert!(codec.decode(&"a".repeat(9000), &b, 150).is_err());
    let mut bytes = token.into_bytes();
    bytes[20] = if bytes[20] == b'A' { b'B' } else { b'A' };
    assert!(
        codec
            .decode(std::str::from_utf8(&bytes).unwrap(), &b, 150)
            .is_err()
    );
}
