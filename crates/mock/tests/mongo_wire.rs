//! The MongoDB mock driven by the official Rust driver: CRUD, cursors, aggregation, indexes,
//! SCRAM authentication, fault injection and persistence.

use futures::TryStreamExt;
use mongodb::bson::{Bson, Document, doc};
use mongodb::options::{ClientOptions, Credential, FindOptions, IndexOptions, ReturnDocument};
use mongodb::{Client, IndexModel};
use twinbase_mock::{AuthMode, FaultAction, FaultConfig, FaultRule, MockConfig, MockKind, start};

async fn mock(dir: Option<std::path::PathBuf>, auth: AuthMode) -> twinbase_mock::MockHandle {
    start(MockConfig {
        kind: MockKind::Mongodb,
        host: "127.0.0.1".into(),
        port: 0,
        data_dir: dir,
        auth,
        faults: Default::default(),
        native: None,
    })
    .await
    .unwrap()
}

async fn client(port: u16, credential: Option<Credential>) -> Client {
    let mut o =
        ClientOptions::parse(format!("mongodb://127.0.0.1:{port}/?directConnection=true&serverSelectionTimeoutMS=3000")).await.unwrap();
    o.credential = credential;
    Client::with_options(o).unwrap()
}

#[tokio::test]
async fn crud_cursors_aggregation_and_indexes() {
    let m = mock(None, AuthMode::Any).await;
    let c = client(m.addr.port(), None).await;
    let db = c.database("shop");
    db.run_command(doc! { "ping": 1 }).await.unwrap();
    let users = db.collection::<Document>("users");
    let docs: Vec<Document> = (0..300).map(|i| doc! { "n": i, "group": i % 3, "name": format!("user{i}") }).collect();
    let r = users.insert_many(docs).await.unwrap();
    assert_eq!(r.inserted_ids.len(), 300);

    // Cursor batching through getMore.
    let all: Vec<Document> = users.find(doc! {}).batch_size(50).await.unwrap().try_collect().await.unwrap();
    assert_eq!(all.len(), 300);
    let page: Vec<Document> = users
        .find(doc! { "group": 1, "n": { "$gte": 100 } })
        .with_options(FindOptions::builder().sort(doc! { "n": -1 }).skip(2).limit(3).projection(doc! { "_id": 0, "n": 1 }).build())
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert_eq!(page, vec![doc! { "n": 292 }, doc! { "n": 289 }, doc! { "n": 286 }]);
    assert_eq!(users.count_documents(doc! { "group": 2 }).await.unwrap(), 100);
    assert_eq!(users.estimated_document_count().await.unwrap(), 300);
    let groups = users.distinct("group", doc! {}).await.unwrap();
    assert_eq!(groups, vec![Bson::Int32(0), Bson::Int32(1), Bson::Int32(2)]);

    // Updates, upserts, replacement, findOneAndUpdate, deletes.
    let u = users.update_many(doc! { "group": 0 }, doc! { "$inc": { "score": 5 }, "$set": { "tier": "gold" } }).await.unwrap();
    assert_eq!((u.matched_count, u.modified_count), (100, 100));
    let up = users.update_one(doc! { "name": "new" }, doc! { "$setOnInsert": { "n": -1 } }).upsert(true).await.unwrap();
    assert!(up.upserted_id.is_some());
    let rep = users.replace_one(doc! { "name": "new" }, doc! { "name": "new", "replaced": true }).await.unwrap();
    assert_eq!(rep.modified_count, 1);
    let after = users
        .find_one_and_update(doc! { "name": "user1" }, doc! { "$push": { "tags": "vip" } })
        .return_document(ReturnDocument::After)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.get_array("tags").unwrap(), &vec![Bson::String("vip".into())]);
    let del = users.delete_many(doc! { "n": { "$lt": 10 } }).await.unwrap();
    assert_eq!(del.deleted_count, 10);

    // Aggregation.
    let agg: Vec<Document> = users
        .aggregate(vec![
            doc! { "$match": { "n": { "$gte": 0 } } },
            doc! { "$group": { "_id": "$group", "count": { "$sum": 1 }, "top": { "$max": "$n" } } },
            doc! { "$sort": { "_id": 1 } },
        ])
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert_eq!(agg.len(), 3);
    assert_eq!(agg[0], doc! { "_id": 0, "count": 96, "top": 297 });

    // Unique indexes and duplicate key errors.
    users
        .create_index(IndexModel::builder().keys(doc! { "name": 1 }).options(IndexOptions::builder().unique(true).build()).build())
        .await
        .unwrap();
    let names: Vec<String> = users.list_index_names().await.unwrap();
    assert_eq!(names, vec!["_id_", "name_1"]);
    let err = users.insert_one(doc! { "name": "user20" }).await.unwrap_err();
    assert!(
        matches!(*err.kind, mongodb::error::ErrorKind::Write(mongodb::error::WriteFailure::WriteError(ref e)) if e.code == 11000),
        "{err}"
    );
    users.drop_index("name_1").await.unwrap();

    let colls = db.list_collection_names().await.unwrap();
    assert_eq!(colls, vec!["users"]);
    assert!(c.list_database_names().await.unwrap().contains(&"shop".to_string()));
    users.drop().await.unwrap();
    assert!(db.list_collection_names().await.unwrap().is_empty());
    m.stop().await;
}

#[tokio::test]
async fn scram_authentication() {
    let m = mock(None, AuthMode::Password { user: "app".into(), password: "s3cret".into() }).await;
    let port = m.addr.port();
    for mechanism in [None, Some(mongodb::options::AuthMechanism::ScramSha1), Some(mongodb::options::AuthMechanism::ScramSha256)] {
        let cred = Credential::builder().username("app".to_string()).password("s3cret".to_string()).mechanism(mechanism).build();
        let c = client(port, Some(cred)).await;
        c.database("app").collection::<Document>("t").insert_one(doc! { "x": 1 }).await.unwrap();
    }
    let wrong = client(port, Some(Credential::builder().username("app".to_string()).password("nope".to_string()).build())).await;
    let e = wrong.database("app").run_command(doc! { "ping": 1 }).await.unwrap_err();
    assert!(e.to_string().contains("Authentication failed"), "{e}");
    let anonymous = client(port, None).await;
    let e = anonymous.database("app").collection::<Document>("t").find_one(doc! {}).await.unwrap_err();
    assert!(e.to_string().contains("requires authentication"), "{e}");
    m.stop().await;

    // A mock that accepts anyone explains why credentials fail.
    let m = mock(None, AuthMode::Any).await;
    let c = client(m.addr.port(), Some(Credential::builder().username("u".to_string()).password("p".to_string()).build())).await;
    let e = c.database("x").run_command(doc! { "ping": 1 }).await.unwrap_err();
    assert!(e.to_string().contains("without credentials"), "{e}");
    m.stop().await;
}

#[tokio::test]
async fn faults_and_persistence() {
    let dir = tempfile::tempdir().unwrap();
    let m = mock(Some(dir.path().to_path_buf()), AuthMode::Any).await;
    let c = client(m.addr.port(), None).await;
    let orders = c.database("app").collection::<Document>("orders");
    orders.insert_many(vec![doc! { "_id": 1, "total": 10 }, doc! { "_id": 2, "total": 20 }]).await.unwrap();
    m.faults
        .set(FaultConfig {
            enabled: true,
            refuse_connections: false,
            rules: vec![FaultRule {
                enabled: true,
                name: "find fails".into(),
                matches: "db.orders.find".into(),
                probability: 1.0,
                action: FaultAction::Error { code: Some("50".into()), message: Some("operation exceeded time limit".into()) },
            }],
        })
        .unwrap();
    let e = orders.find_one(doc! {}).await.unwrap_err();
    assert!(e.to_string().contains("exceeded time limit"), "{e}");
    let events = m.traffic.since(0, 100);
    assert!(events.iter().any(
        |ev| matches!(&ev.kind, twinbase_mock::TrafficKind::Query { text, error: Some(_), .. } if text.starts_with("db.orders.find"))
    ));
    m.faults.set(FaultConfig::default()).unwrap();
    assert_eq!(orders.count_documents(doc! {}).await.unwrap(), 2);
    drop(c);
    m.stop().await;

    let m = mock(Some(dir.path().to_path_buf()), AuthMode::Any).await;
    let c = client(m.addr.port(), None).await;
    let orders = c.database("app").collection::<Document>("orders");
    let total: Vec<Document> =
        orders.aggregate(vec![doc! { "$group": { "_id": null, "sum": { "$sum": "$total" } } }]).await.unwrap().try_collect().await.unwrap();
    assert_eq!(total[0].get_i32("sum").unwrap(), 30);
    m.stop().await;
}
