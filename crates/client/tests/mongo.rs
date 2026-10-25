//! The MongoDB client against the TwinBase mock, and against a real MongoDB when
//! `TWINBASE_REAL_MONGO_PORT` (and `TWINBASE_REAL_MONGO_PASSWORD` for user `root`) are set.

use serde_json::json;
use twinbase_client::{Client, ConnectionConfig, DbKind, FindParams, TlsMode};
use twinbase_mock::{AuthMode, MockConfig, MockKind, start};

fn config(port: u16, user: &str, password: Option<&str>) -> ConnectionConfig {
    ConnectionConfig {
        kind: DbKind::Mongodb,
        host: "127.0.0.1".into(),
        port,
        user: user.into(),
        password: password.map(Into::into),
        database: Some("shop".into()),
        path: None,
        tls: TlsMode::Disable,
        timeout_secs: 5,
        ssh: None,
        via: None,
    }
}

async fn exercise(client: &Client) {
    let m = client.mongo().unwrap();
    let _ = m.drop_collection(None, "orders").await;
    let ids = m.insert(None, "orders", "[{_id: 1, customer: 'ann', total: 10, at: ISODate('2024-05-01T10:00:00Z')}, {_id: 2, customer: 'bob', total: 25.5, items: [{sku: 'a', qty: 2}]}, {_id: 3, customer: 'ann', total: 7}]").await.unwrap();
    assert_eq!(ids, vec![json!(1), json!(2), json!(3)]);
    let page = m
        .find(
            None,
            "orders",
            &FindParams {
                filter: "{customer: 'ann'}".into(),
                sort: "{total: -1}".into(),
                projection: "{_id: 1, total: 1}".into(),
                skip: 0,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.documents, vec![json!({ "_id": 1, "total": 10 }), json!({ "_id": 3, "total": 7 })]);
    assert_eq!(page.total, Some(2));
    m.replace(None, "orders", json!(3), "{_id: 3, customer: 'ann', total: 8, note: 'edited'}").await.unwrap();
    assert!(m.replace(None, "orders", json!(99), "{x: 1}").await.unwrap_err().message.contains("no longer exists"));
    assert!(m.replace(None, "orders", json!(3), "{_id: 4}").await.is_err());
    assert_eq!(m.update(None, "orders", "{customer: 'ann'}", "{$inc: {total: 1}}", true).await.unwrap(), (2, 2));
    let (docs, truncated) =
        m.aggregate(None, "orders", "[{$group: {_id: '$customer', sum: {$sum: '$total'}}}, {$sort: {_id: 1}}]", 100).await.unwrap();
    assert!(!truncated);
    assert_eq!(docs, vec![json!({ "_id": "ann", "sum": 20 }), json!({ "_id": "bob", "sum": 25.5 })]);
    let name = m.create_index(None, "orders", "{customer: 1, total: -1}", false, None).await.unwrap();
    assert_eq!(name, "customer_1_total_-1");
    assert_eq!(m.indexes(None, "orders").await.unwrap().len(), 2);
    m.drop_index(None, "orders", &name).await.unwrap();
    let schema = m.schema(None, "orders", 100).await.unwrap();
    assert!(schema.fields.iter().any(|f| f.path == "items[].qty"));
    let colls = m.collections(None).await.unwrap();
    assert!(colls.iter().any(|c| c.name == "orders" && c.count == Some(3)));

    // Console scripts.
    let out = m.run(None, "db.orders.find({total: {$gt: 9}}).sort({total: 1}).limit(5)\ndb.orders.countDocuments({customer: 'ann'})\nuse other\ndb.things.insertOne({a: 1})\nshow collections", 100).await.unwrap();
    assert_eq!(out.len(), 5);
    assert_eq!(out[0].documents.as_ref().unwrap().len(), 2);
    assert_eq!(out[1].value, Some(json!(2)));
    assert_eq!(out[3].database, "other");
    assert_eq!(out[4].documents.as_ref().unwrap()[0]["name"], "things");
    let out = m.run(None, "db.orders.find().limit(2)", 100).await.unwrap();
    assert_eq!(out[0].documents.as_ref().unwrap().len(), 2);
    assert!(!out[0].truncated);
    let out = m.run(None, "db.orders.find()", 2).await.unwrap();
    assert!(out[0].truncated);
    let out = m
        .run(
            None,
            "db.orders.updateOne({_id: 2}, {$set: {'items.$[i].qty': 5}}, {arrayFilters: [{'i.sku': 'a'}]})\ndb.orders.findOne({_id: 2})",
            10,
        )
        .await
        .unwrap();
    assert_eq!(out[1].value.as_ref().unwrap()["items"][0]["qty"], 5);
    let out = m.run(None, "db.orders.insertOne({_id: 1})\ndb.orders.countDocuments()", 10).await.unwrap();
    assert_eq!(out.len(), 1, "the script stops at the first error");
    assert_eq!(out[0].error.as_ref().unwrap().code.as_deref(), Some("11000"));
    let out = m.run(None, "db.runCommand({dbStats: 1})", 10).await.unwrap();
    assert_eq!(out[0].value.as_ref().unwrap()["db"], "shop");
    m.run(None, "db.getSiblingDB('other').dropDatabase()", 10).await.unwrap();
    assert_eq!(m.delete_ids(None, "orders", vec![json!(1), json!(2)]).await.unwrap(), 2);
    assert_eq!(m.delete(None, "orders", "{}", true).await.unwrap(), 1);
}

#[tokio::test]
async fn mongo_mock() {
    let mock = start(MockConfig {
        kind: MockKind::Mongodb,
        host: "127.0.0.1".into(),
        port: 0,
        data_dir: None,
        auth: AuthMode::Password { user: "dev".into(), password: "pw".into() },
        faults: Default::default(),
        native: None,
    })
    .await
    .unwrap();
    let client = Client::connect(&config(mock.addr.port(), "dev", Some("pw"))).await.unwrap();
    exercise(&client).await;
    let bad = Client::connect(&config(mock.addr.port(), "dev", Some("nope"))).await;
    assert!(bad.err().unwrap().message.contains("authentication failed"));
    mock.stop().await;
}

#[tokio::test]
async fn mongo_real() {
    let Some(port) = std::env::var("TWINBASE_REAL_MONGO_PORT").ok().and_then(|p| p.parse().ok()) else { return };
    let password = std::env::var("TWINBASE_REAL_MONGO_PASSWORD").unwrap_or_default();
    let client = Client::connect(&config(port, "root", Some(&password))).await.unwrap();
    assert!(client.mongo().unwrap().server_version().await.unwrap().starts_with('7'));
    exercise(&client).await;
}
