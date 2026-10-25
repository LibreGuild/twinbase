//! The Firestore mock over real gRPC with the generated client (the same protocol SDKs use).

use twinbase_firestore_proto::json::{fields_from_plain, fields_to_plain};
use twinbase_firestore_proto::v1::firestore_client::FirestoreClient;
use twinbase_firestore_proto::v1::structured_query::{self as sq, CollectionSelector, FieldReference};
use twinbase_firestore_proto::v1::*;
use twinbase_mock::{AuthMode, MockConfig, MockKind, start};

const DB: &str = "projects/demo-app/databases/(default)";

async fn mock() -> twinbase_mock::MockHandle {
    start(MockConfig {
        kind: MockKind::Firestore,
        host: "127.0.0.1".into(),
        port: 0,
        data_dir: None,
        auth: AuthMode::Any,
        faults: Default::default(),
        native: None,
    })
    .await
    .unwrap()
}

fn set(path: &str, fields: serde_json::Value) -> Write {
    Write {
        operation: Some(write::Operation::Update(Document {
            name: format!("{DB}/documents/{path}"),
            fields: fields_from_plain(&fields, DB).unwrap(),
            ..Default::default()
        })),
        ..Default::default()
    }
}

#[tokio::test]
async fn commit_query_aggregate_and_listen() {
    let m = mock().await;
    let mut c = FirestoreClient::connect(format!("http://{}", m.addr)).await.unwrap();
    c.commit(CommitRequest {
        database: DB.into(),
        writes: vec![
            set("cities/LIS", serde_json::json!({ "name": "Lisbon", "pop": 545000 })),
            set("cities/OPO", serde_json::json!({ "name": "Porto", "pop": 232000 })),
        ],
        ..Default::default()
    })
    .await
    .unwrap();

    // Listen to a query; then add a matching city and expect it to arrive.
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let q = StructuredQuery {
        from: vec![CollectionSelector { collection_id: "cities".into(), all_descendants: false }],
        order_by: vec![sq::Order { field: Some(FieldReference { field_path: "pop".into() }), direction: sq::Direction::Descending as i32 }],
        ..Default::default()
    };
    tx.send(ListenRequest {
        database: DB.into(),
        target_change: Some(listen_request::TargetChange::AddTarget(Target {
            target_id: 7,
            target_type: Some(target::TargetType::Query(target::QueryTarget {
                parent: format!("{DB}/documents"),
                query_type: Some(target::query_target::QueryType::StructuredQuery(q.clone())),
            })),
            ..Default::default()
        })),
        ..Default::default()
    })
    .await
    .unwrap();
    let mut listen = c.listen(tokio_stream::wrappers::ReceiverStream::new(rx)).await.unwrap().into_inner();
    let mut initial = Vec::new();
    loop {
        let r = listen.message().await.unwrap().unwrap();
        match r.response_type.unwrap() {
            listen_response::ResponseType::DocumentChange(d) => initial.push(d.document.unwrap().name),
            listen_response::ResponseType::TargetChange(t)
                if t.target_change_type == target_change::TargetChangeType::NoChange as i32 && t.target_ids.is_empty() =>
            {
                break;
            }
            _ => {}
        }
    }
    assert_eq!(initial.len(), 2);
    c.commit(CommitRequest {
        database: DB.into(),
        writes: vec![set("cities/FAO", serde_json::json!({ "name": "Faro", "pop": 64000 }))],
        ..Default::default()
    })
    .await
    .unwrap();
    let mut got = None;
    while let Some(r) = listen.message().await.unwrap() {
        match r.response_type.unwrap() {
            listen_response::ResponseType::DocumentChange(d) => got = Some(d.document.unwrap().name),
            listen_response::ResponseType::TargetChange(_) if got.is_some() => break,
            _ => {}
        }
    }
    assert!(got.unwrap().ends_with("cities/FAO"));
    drop(tx);

    // Query with a filter and an aggregation.
    let mut big = q.clone();
    big.r#where = Some(sq::Filter {
        filter_type: Some(sq::filter::FilterType::FieldFilter(sq::FieldFilter {
            field: Some(FieldReference { field_path: "pop".into() }),
            op: sq::field_filter::Operator::GreaterThan as i32,
            value: Some(twinbase_firestore_proto::json::from_plain(&serde_json::json!(100000), DB).unwrap()),
        })),
    });
    let mut stream = c
        .run_query(RunQueryRequest {
            parent: format!("{DB}/documents"),
            query_type: Some(run_query_request::QueryType::StructuredQuery(big.clone())),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    let mut names = Vec::new();
    while let Some(r) = stream.message().await.unwrap() {
        if let Some(d) = r.document {
            names.push(fields_to_plain(&d.fields)["name"].as_str().unwrap().to_owned());
        }
    }
    assert_eq!(names, vec!["Lisbon", "Porto"]);
    let agg = StructuredAggregationQuery {
        aggregations: vec![
            structured_aggregation_query::Aggregation {
                alias: "n".into(),
                operator: Some(structured_aggregation_query::aggregation::Operator::Count(Default::default())),
            },
            structured_aggregation_query::Aggregation {
                alias: "total".into(),
                operator: Some(structured_aggregation_query::aggregation::Operator::Sum(structured_aggregation_query::aggregation::Sum {
                    field: Some(FieldReference { field_path: "pop".into() }),
                })),
            },
        ],
        query_type: Some(structured_aggregation_query::QueryType::StructuredQuery(q)),
    };
    let mut s = c
        .run_aggregation_query(RunAggregationQueryRequest {
            parent: format!("{DB}/documents"),
            query_type: Some(run_aggregation_query_request::QueryType::StructuredAggregationQuery(agg)),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    let res = s.message().await.unwrap().unwrap().result.unwrap();
    assert_eq!(fields_to_plain(&res.aggregate_fields), serde_json::json!({ "n": 3, "total": 841000 }));
    let missing =
        c.get_document(GetDocumentRequest { name: format!("{DB}/documents/cities/XXX"), ..Default::default() }).await.unwrap_err();
    assert_eq!(missing.code(), tonic::Code::NotFound);
    m.stop().await;
}

/// Keeps a Firestore mock running on port 18080 for manual SDK checks:
/// `cargo test -p twinbase-mock --test firestore_wire -- --ignored hold`.
#[tokio::test]
#[ignore]
async fn hold() {
    let m = start(MockConfig {
        kind: MockKind::Firestore,
        host: "127.0.0.1".into(),
        port: 18080,
        data_dir: None,
        auth: AuthMode::Any,
        faults: Default::default(),
        native: None,
    })
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(600)).await;
    m.stop().await;
}
