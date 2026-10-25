//! The DynamoDB client against the TwinBase mock (with SigV4 verification) and, when
//! `TWINBASE_DYNAMODB_LOCAL_PORT` is set, against Amazon's DynamoDB Local — the same suite must
//! pass on both.

use serde_json::{Value, json};
use twinbase_client::{Client, ConnectionConfig, DbKind, TlsMode};
use twinbase_mock::{AuthMode, MockConfig, MockKind, start};

fn config(port: u16, user: &str, secret: &str) -> ConnectionConfig {
    ConnectionConfig {
        kind: DbKind::Dynamodb,
        host: "127.0.0.1".into(),
        port,
        user: user.into(),
        password: Some(secret.into()),
        database: Some("eu-west-1".into()),
        path: None,
        tls: TlsMode::Disable,
        timeout_secs: 5,
        ssh: None,
        via: None,
    }
}

async fn exercise(client: &Client, table: &str) {
    let d = client.dynamo().unwrap();
    let call = |op: &'static str, v: Value| async move { d.request(op, &v).await };
    let _ = call("DeleteTable", json!({ "TableName": table })).await;
    call(
        "CreateTable",
        json!({
            "TableName": table, "BillingMode": "PAY_PER_REQUEST",
            "KeySchema": [{ "AttributeName": "pk", "KeyType": "HASH" }, { "AttributeName": "sk", "KeyType": "RANGE" }],
            "AttributeDefinitions": [{ "AttributeName": "pk", "AttributeType": "S" }, { "AttributeName": "sk", "AttributeType": "N" }, { "AttributeName": "state", "AttributeType": "S" }],
            "GlobalSecondaryIndexes": [{ "IndexName": "by-state", "KeySchema": [{ "AttributeName": "state", "KeyType": "HASH" }, { "AttributeName": "sk", "KeyType": "RANGE" }], "Projection": { "ProjectionType": "ALL" } }]
        }),
    )
    .await
    .unwrap();
    assert!(d.list_tables().await.unwrap().contains(&table.to_owned()));
    let desc = d.describe_table(table).await.unwrap();
    assert_eq!(desc["KeySchema"][0]["AttributeName"], "pk");
    assert_eq!(desc["GlobalSecondaryIndexes"][0]["IndexName"], "by-state");

    for i in 1..=12 {
        call(
            "PutItem",
            json!({ "TableName": table, "Item": {
            "pk": { "S": format!("c#{}", i % 2) }, "sk": { "N": i.to_string() }, "state": { "S": if i % 4 == 0 { "open" } else { "paid" } },
            "amount": { "N": format!("{}.50", i * 10) }, "tags": { "SS": ["a", format!("t{i}")] }
        } }),
        )
        .await
        .unwrap();
    }
    let got = call("GetItem", json!({ "TableName": table, "Key": { "pk": { "S": "c#0" }, "sk": { "N": "4" } }, "ProjectionExpression": "amount, #s", "ExpressionAttributeNames": { "#s": "state" } })).await.unwrap();
    assert_eq!(got["Item"], json!({ "amount": { "N": "40.5" }, "state": { "S": "open" } }));

    let upd = call(
        "UpdateItem",
        json!({
            "TableName": table, "Key": { "pk": { "S": "c#0" }, "sk": { "N": "4" } },
            "UpdateExpression": "SET amount = amount + :d, visits = if_not_exists(visits, :z) + :one ADD tags :t REMOVE #s",
            "ConditionExpression": "attribute_exists(pk)",
            "ExpressionAttributeNames": { "#s": "state" },
            "ExpressionAttributeValues": { ":d": { "N": "0.25" }, ":z": { "N": "0" }, ":one": { "N": "1" }, ":t": { "SS": ["z"] } },
            "ReturnValues": "ALL_NEW"
        }),
    )
    .await
    .unwrap();
    assert_eq!(upd["Attributes"]["amount"], json!({ "N": "40.75" }));
    assert_eq!(upd["Attributes"]["visits"], json!({ "N": "1" }));
    assert!(upd["Attributes"].get("state").is_none());
    let mut tags: Vec<String> =
        upd["Attributes"]["tags"]["SS"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_owned()).collect();
    tags.sort();
    assert_eq!(tags, vec!["a", "t4", "z"]);

    let failed = call("PutItem", json!({ "TableName": table, "Item": { "pk": { "S": "c#0" }, "sk": { "N": "4" } }, "ConditionExpression": "attribute_not_exists(pk)" })).await.unwrap_err();
    assert_eq!(failed.code.as_deref(), Some("ConditionalCheckFailedException"));
    let reserved = call(
        "Scan",
        json!({ "TableName": table, "FilterExpression": "state = :s", "ExpressionAttributeValues": { ":s": { "S": "open" } } }),
    )
    .await
    .unwrap_err();
    assert_eq!(reserved.code.as_deref(), Some("ValidationException"));
    assert!(reserved.message.contains("reserved keyword"), "{}", reserved.message);
    let unused = call("Scan", json!({ "TableName": table, "ExpressionAttributeValues": { ":s": { "S": "x" } } })).await.unwrap_err();
    assert_eq!(unused.code.as_deref(), Some("ValidationException"));

    // Query pages backwards.
    let q = |start: Option<Value>| {
        let mut r = json!({ "TableName": table, "KeyConditionExpression": "pk = :p AND sk BETWEEN :a AND :b", "ExpressionAttributeValues": { ":p": { "S": "c#1" }, ":a": { "N": "2" }, ":b": { "N": "11" } }, "ScanIndexForward": false, "Limit": 3 });
        if let Some(s) = start {
            r["ExclusiveStartKey"] = s;
        }
        call("Query", r)
    };
    let p1 = q(None).await.unwrap();
    let sks = |p: &Value| p["Items"].as_array().unwrap().iter().map(|i| i["sk"]["N"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
    assert_eq!(sks(&p1), vec!["11", "9", "7"]);
    let p2 = q(Some(p1["LastEvaluatedKey"].clone())).await.unwrap();
    assert_eq!(sks(&p2), vec!["5", "3"]);

    let gsi = call("Query", json!({ "TableName": table, "IndexName": "by-state", "KeyConditionExpression": "#s = :o", "ExpressionAttributeNames": { "#s": "state" }, "ExpressionAttributeValues": { ":o": { "S": "open" } } })).await.unwrap();
    assert_eq!(sks(&gsi), vec!["8", "12"]);
    let count = call("Scan", json!({ "TableName": table, "FilterExpression": "amount > :m AND contains(tags, :a)", "ExpressionAttributeValues": { ":m": { "N": "60" }, ":a": { "S": "a" } }, "Select": "COUNT" })).await.unwrap();
    assert_eq!((count["Count"].as_u64(), count["ScannedCount"].as_u64()), (Some(7), Some(12)));

    let tx = call("TransactWriteItems", json!({ "TransactItems": [
        { "Put": { "TableName": table, "Item": { "pk": { "S": "tx" }, "sk": { "N": "1" } } } },
        { "ConditionCheck": { "TableName": table, "Key": { "pk": { "S": "c#1" }, "sk": { "N": "1" } }, "ConditionExpression": "amount > :big", "ExpressionAttributeValues": { ":big": { "N": "1000" } } } }
    ] }))
    .await
    .unwrap_err();
    assert_eq!(tx.code.as_deref(), Some("TransactionCanceledException"));
    assert!(
        call("GetItem", json!({ "TableName": table, "Key": { "pk": { "S": "tx" }, "sk": { "N": "1" } } }))
            .await
            .unwrap()
            .get("Item")
            .is_none()
    );

    call(
        "BatchWriteItem",
        json!({ "RequestItems": { table: [
        { "PutRequest": { "Item": { "pk": { "S": "b" }, "sk": { "N": "1" } } } },
        { "DeleteRequest": { "Key": { "pk": { "S": "c#1" }, "sk": { "N": "1" } } } }
    ] } }),
    )
    .await
    .unwrap();
    let got = call("BatchGetItem", json!({ "RequestItems": { table: { "Keys": [{ "pk": { "S": "b" }, "sk": { "N": "1" } }, { "pk": { "S": "c#1" }, "sk": { "N": "1" } }] } } })).await.unwrap();
    assert_eq!(got["Responses"][table].as_array().unwrap().len(), 1);
    let missing = call("DescribeTable", json!({ "TableName": "no-such-table" })).await.unwrap_err();
    assert_eq!(missing.code.as_deref(), Some("ResourceNotFoundException"));
    call("DeleteTable", json!({ "TableName": table })).await.unwrap();
}

#[tokio::test]
async fn dynamodb_mock_with_sigv4() {
    let mock = start(MockConfig {
        kind: MockKind::Dynamodb,
        host: "127.0.0.1".into(),
        port: 0,
        data_dir: None,
        auth: AuthMode::Password { user: "AKIDTEST".into(), password: "test-secret".into() },
        faults: Default::default(),
        native: None,
    })
    .await
    .unwrap();
    let client = Client::connect(&config(mock.addr.port(), "AKIDTEST", "test-secret")).await.unwrap();
    exercise(&client, "orders").await;
    let bad = Client::connect(&config(mock.addr.port(), "AKIDTEST", "wrong")).await.err().unwrap();
    assert_eq!(bad.code.as_deref(), Some("InvalidSignatureException"));
    let traffic = mock.traffic.since(0, 500);
    assert!(traffic.iter().any(|e| matches!(&e.kind, twinbase_mock::TrafficKind::Query { text, .. } if text.starts_with("Query orders"))));
    mock.stop().await;
}

#[tokio::test]
async fn dynamodb_local() {
    let Some(port) = std::env::var("TWINBASE_DYNAMODB_LOCAL_PORT").ok().and_then(|p| p.parse().ok()) else { return };
    let client = Client::connect(&config(port, "local", "local")).await.unwrap();
    exercise(&client, "twinbase-parity").await;
}
