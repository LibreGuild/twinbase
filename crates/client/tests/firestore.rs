//! The Firestore client and its console statements against the TwinBase Firestore mock.

use serde_json::json;
use twinbase_client::firestore_client::FsQuery;
use twinbase_client::{Client, ConnectionConfig, DbKind, TlsMode};
use twinbase_mock::{AuthMode, MockConfig, MockKind, start};

fn config(port: u16, token: Option<&str>) -> ConnectionConfig {
    ConnectionConfig {
        kind: DbKind::Firestore,
        host: "127.0.0.1".into(),
        port,
        user: String::new(),
        password: token.map(Into::into),
        database: Some("demo-app".into()),
        path: None,
        tls: TlsMode::Disable,
        timeout_secs: 5,
        ssh: None,
        via: None,
    }
}

#[tokio::test]
async fn firestore_client_and_console() {
    let mock = start(MockConfig {
        kind: MockKind::Firestore,
        host: "127.0.0.1".into(),
        port: 0,
        data_dir: None,
        auth: AuthMode::Password { user: String::new(), password: "dev-token".into() },
        faults: Default::default(),
        native: None,
    })
    .await
    .unwrap();
    let port = mock.addr.port();
    assert!(Client::connect(&config(port, Some("wrong"))).await.is_err());
    let client = Client::connect(&config(port, Some("dev-token"))).await.unwrap();
    let f = client.firestore().unwrap();
    let out = f
        .run(
            "set users/ann {\"name\": \"Ann\", \"age\": 31, \"city\": \"Lisbon\", \"joined\": {\"__timestamp\": \"2024-05-01T10:00:00Z\"}}\n\
             set users/bob {\"name\": \"Bob\", \"age\": 25, \"city\": \"Porto\"}\n\
             set users/cid {\"name\": \"Cid\", \"age\": 40, \"city\": \"Lisbon\", \"tags\": [\"vip\"]}\n\
             add users/ann/orders {\"total\": 10.5}\n\
             query users where city == \"Lisbon\" and age > 30 order by age desc\n\
             count users where age >= 25\n\
             update users/bob {\"age\": 26, \"address.street\": \"Rua A\"}\n\
             get users/bob\n\
             collections users/ann\n\
             query group orders",
            100,
        )
        .await
        .unwrap();
    assert!(out.iter().all(|o| o.error.is_none()), "{out:?}");
    let names =
        |i: usize| out[i].documents.as_ref().unwrap().iter().map(|d| d.data["name"].as_str().unwrap_or("").to_owned()).collect::<Vec<_>>();
    assert_eq!(names(4), vec!["Cid", "Ann"]);
    assert_eq!(out[5].value, Some(json!(3)));
    assert_eq!(
        out[7].documents.as_ref().unwrap()[0].data,
        json!({ "name": "Bob", "age": 26, "city": "Porto", "address": { "street": "Rua A" } })
    );
    assert_eq!(out[8].value, Some(json!(["orders"])));
    assert_eq!(out[9].documents.as_ref().unwrap()[0].data, json!({ "total": 10.5 }));
    assert_eq!(out[4].documents.as_ref().unwrap()[1].data["joined"], json!({ "__timestamp": "2024-05-01T10:00:00Z" }));

    // Paging by name and updates that must find the document.
    let p1 = f.query(&FsQuery { collection: "users".into(), limit: Some(2), ..Default::default() }).await.unwrap();
    let p2 = f
        .query(&FsQuery { collection: "users".into(), limit: Some(2), start_after: Some(p1[1].path.clone()), ..Default::default() })
        .await
        .unwrap();
    assert_eq!(p1.iter().chain(&p2).map(|d| d.id.as_str()).collect::<Vec<_>>(), vec!["ann", "bob", "cid"]);
    let missing = f.set("users/zed", &json!({ "a": 1 }), true, Some(true)).await.unwrap_err();
    assert_eq!(missing.code.as_deref(), Some("NOT_FOUND"));
    let failed = f.run("delete users/ann\nget users/nobody\nquery users where age > \"x\" and\ncount users", 10).await;
    assert!(failed.is_err(), "parse errors are reported before anything runs");
    assert_eq!(f.delete(&["users/ann".into(), "users/bob".into()]).await.unwrap(), 2);
    assert_eq!(f.count(&FsQuery { collection: "users".into(), ..Default::default() }).await.unwrap(), 1);
    mock.stop().await;
}
