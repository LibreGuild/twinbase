//! Elasticsearch / OpenSearch client against a real server: set `TWINBASE_REAL_ES_PORT` and
//! `TWINBASE_REAL_ES_PASSWORD` (user `elastic`), e.g. with
//! `docker run -e discovery.type=single-node -e ELASTIC_PASSWORD=… -e xpack.security.http.ssl.enabled=false -p 127.0.0.1:59200:9200 docker.elastic.co/elasticsearch/elasticsearch:8.17.4`.

use serde_json::json;
use twinbase_client::{Client, ConnectionConfig, DbKind, TlsMode};

fn config() -> Option<ConnectionConfig> {
    let port = std::env::var("TWINBASE_REAL_ES_PORT").ok()?.parse().ok()?;
    Some(ConnectionConfig {
        kind: DbKind::Elasticsearch,
        host: "127.0.0.1".into(),
        port,
        user: "elastic".into(),
        password: std::env::var("TWINBASE_REAL_ES_PASSWORD").ok(),
        database: None,
        path: None,
        tls: TlsMode::Disable,
        timeout_secs: 10,
        ssh: None,
        via: None,
    })
}

#[tokio::test]
async fn elasticsearch_end_to_end() {
    let Some(cfg) = config() else { return };
    let c = Client::connect(&cfg).await.unwrap();
    let s = c.search().unwrap();
    assert_eq!(s.info.flavor, "elasticsearch");
    assert!(s.product().starts_with("Elasticsearch 8."));
    let idx = format!("tb-test-{}", std::process::id());
    let out = s
        .run(&format!(
            "DELETE /{idx}?ignore_unavailable=true\nPUT /{idx}\n{{ \"mappings\": {{ \"properties\": {{ \"name\": {{ \"type\": \"text\", \"fields\": {{ \"keyword\": {{ \"type\": \"keyword\" }} }} }}, \"price\": {{ \"type\": \"double\" }}, \"address\": {{ \"properties\": {{ \"city\": {{ \"type\": \"keyword\" }} }} }} }} }} }}\n\nPOST /_bulk?refresh=true\n{{\"index\":{{\"_index\":\"{idx}\",\"_id\":\"1\"}}}}\n{{\"name\":\"Desk lamp\",\"price\":25,\"address\":{{\"city\":\"Lisbon\"}}}}\n{{\"index\":{{\"_index\":\"{idx}\",\"_id\":\"2\"}}}}\n{{\"name\":\"Floor lamp\",\"price\":80}}\n{{\"index\":{{\"_index\":\"{idx}\",\"_id\":\"3\"}}}}\n{{\"name\":\"Desk\",\"price\":200}}\n\nGET /{idx}/_search\n{{ \"query\": {{ \"match\": {{ \"name\": \"lamp\" }} }}, \"sort\": [{{ \"price\": \"asc\" }}] }}"
        ))
        .await
        .unwrap();
    assert_eq!(out.len(), 4, "{out:?}");
    assert_eq!(out[2].response["errors"], false);
    let hits = &out[3].response["hits"];
    assert_eq!(hits["total"]["value"], 2);
    assert_eq!(hits["hits"][0]["_id"], "1");

    // Index list and mapping.
    let indices = s.indices(false).await.unwrap();
    let i = indices.iter().find(|i| i.name == idx).unwrap();
    assert_eq!(i.docs, Some(3));
    let (fields, _) = s.mapping(&idx).await.unwrap();
    let kinds: Vec<(&str, &str)> = fields.iter().map(|f| (f.path.as_str(), f.kind.as_str())).collect();
    assert!(
        kinds.contains(&("name.keyword", "keyword"))
            && kinds.contains(&("address.city", "keyword"))
            && kinds.contains(&("price", "double")),
        "{kinds:?}"
    );

    // Documents: search with sequence numbers, optimistic save, create conflicts, delete.
    let r = s.search(&idx, &json!({ "query": { "term": { "_id": "1" } } })).await.unwrap();
    let hit = &r["hits"]["hits"][0];
    let version = (hit["_seq_no"].as_u64().unwrap(), hit["_primary_term"].as_u64().unwrap());
    s.put(&idx, "1", &json!({ "name": "Desk lamp", "price": 27 }), false, Some(version)).await.unwrap();
    let stale = s.put(&idx, "1", &json!({ "name": "x" }), false, Some(version)).await.unwrap_err();
    assert_eq!(stale.code.as_deref(), Some("version_conflict_engine_exception"), "{stale:?}");
    let dup = s.put(&idx, "2", &json!({ "name": "x" }), true, None).await.unwrap_err();
    assert_eq!(dup.code.as_deref(), Some("version_conflict_engine_exception"));
    let added = s.put(&idx, "", &json!({ "name": "Chair", "price": 40 }), false, None).await.unwrap();
    assert_eq!(added["result"], "created");
    assert_eq!(s.get(&idx, "1").await.unwrap()["_source"]["price"], 27);
    assert_eq!(s.delete(&idx, &["2".into(), "nope".into()]).await.unwrap(), 1);

    // Errors: the console reports the failing request and stops there.
    let out = s.run(&format!("GET /{idx}/_count\nGET /missing-index-xyz/_search\nGET /{idx}/_count")).await.unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(out[1].status, 404);
    assert_eq!(out[1].error.as_ref().unwrap().code.as_deref(), Some("index_not_found_exception"));
    // _cat (JSON, since TwinBase asks for JSON) and SQL.
    let out = s.run(&format!("GET /_cat/indices/{idx}?v\nPOST /_sql?format=json\n{{ \"query\": \"SELECT name FROM \\\"{idx}\\\" ORDER BY price DESC LIMIT 1\" }}")).await.unwrap();
    assert_eq!(out[0].response[0]["index"], idx.as_str(), "{:?}", out[0].response);
    assert_eq!(out[1].response["rows"][0][0], "Desk");

    s.run(&format!("DELETE /{idx}")).await.unwrap();
}

#[tokio::test]
async fn wrong_password_is_a_clear_error() {
    let Some(mut cfg) = config() else { return };
    cfg.password = Some("nope".into());
    let e = Client::connect(&cfg).await.err().unwrap();
    assert_eq!(e.code.as_deref(), Some("security_exception"), "{e:?}");
}
