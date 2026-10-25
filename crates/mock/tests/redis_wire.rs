//! The Redis mock driven by a real client (redis-rs): data types, expiry, transactions,
//! pub/sub, RESP3 and persistence.

use redis::AsyncCommands;
use twinbase_mock::{AuthMode, MockConfig, MockKind, start};

async fn mock(dir: Option<std::path::PathBuf>, auth: AuthMode) -> twinbase_mock::MockHandle {
    start(MockConfig {
        kind: MockKind::Redis,
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

#[tokio::test]
async fn data_types_and_transactions() {
    let m = mock(None, AuthMode::Any).await;
    let client = redis::Client::open(format!("redis://127.0.0.1:{}/", m.addr.port())).unwrap();
    let mut c = client.get_multiplexed_async_connection().await.unwrap();
    let _: () = c.set("greeting", "hello").await.unwrap();
    let v: String = c.get("greeting").await.unwrap();
    assert_eq!(v, "hello");
    let n: i64 = c.incr("counter", 5).await.unwrap();
    assert_eq!(n, 5);
    let _: () = c.hset_multiple("user:1", &[("name", "Ann"), ("role", "admin")]).await.unwrap();
    let h: std::collections::HashMap<String, String> = c.hgetall("user:1").await.unwrap();
    assert_eq!(h["role"], "admin");
    let _: () = c.rpush("queue", &["a", "b", "c"]).await.unwrap();
    let l: Vec<String> = c.lrange("queue", 0, -1).await.unwrap();
    assert_eq!(l, vec!["a", "b", "c"]);
    let _: () = c.zadd_multiple("board", &[(10, "ann"), (30, "bob"), (20, "cid")]).await.unwrap();
    let top: Vec<(String, f64)> = c.zrevrange_withscores("board", 0, 1).await.unwrap();
    assert_eq!(top, vec![("bob".to_string(), 30.0), ("cid".to_string(), 20.0)]);
    let _: () = c.set_ex("temp", "x", 100).await.unwrap();
    let ttl: i64 = c.ttl("temp").await.unwrap();
    assert!(ttl > 90);
    let mut keys: Vec<String> = c.keys("*").await.unwrap();
    keys.sort();
    assert_eq!(keys, vec!["board", "counter", "greeting", "queue", "temp", "user:1"]);
    let err: redis::RedisResult<String> = c.get("queue").await;
    assert!(err.unwrap_err().to_string().contains("WRONGTYPE"));
    let (a, b): (i64, i64) = redis::pipe().atomic().incr("counter", 1).incr("counter", 1).query_async(&mut c).await.unwrap();
    assert_eq!((a, b), (6, 7));
    m.stop().await;
}

#[tokio::test]
async fn pubsub_and_auth() {
    let m = mock(None, AuthMode::Password { user: "default".into(), password: "pw".into() }).await;
    let port = m.addr.port();
    let bad = redis::Client::open(format!("redis://127.0.0.1:{port}/")).unwrap();
    let mut bad_conn = bad.get_multiplexed_async_connection().await.unwrap();
    let denied: redis::RedisResult<String> = bad_conn.get("x").await;
    assert!(denied.is_err());
    let client = redis::Client::open(format!("redis://:pw@127.0.0.1:{port}/")).unwrap();
    let mut pubsub = client.get_async_pubsub().await.unwrap();
    pubsub.subscribe("news").await.unwrap();
    let mut c = client.get_multiplexed_async_connection().await.unwrap();
    let receivers: i64 = c.publish("news", "hello").await.unwrap();
    assert_eq!(receivers, 1);
    use futures::StreamExt;
    let msg = tokio::time::timeout(std::time::Duration::from_secs(2), pubsub.on_message().next()).await.unwrap().unwrap();
    assert_eq!(msg.get_channel_name(), "news");
    assert_eq!(msg.get_payload::<String>().unwrap(), "hello");
    m.stop().await;
}

#[tokio::test]
async fn resp3_and_persistence() {
    let dir = std::env::temp_dir().join(format!("tb-redis-{}", uuid::Uuid::new_v4()));
    let m = mock(Some(dir.clone()), AuthMode::Any).await;
    let port = m.addr.port();
    let client = redis::Client::open(format!("redis://127.0.0.1:{port}/?protocol=resp3")).unwrap();
    let mut c = client.get_multiplexed_async_connection().await.unwrap();
    let _: () = c.hset("cfg", "mode", "dark").await.unwrap();
    let h: std::collections::HashMap<String, String> = c.hgetall("cfg").await.unwrap();
    assert_eq!(h["mode"], "dark");
    let _: () = c.sadd("tags", &["x", "y"]).await.unwrap();
    m.stop().await;
    let m = mock(Some(dir.clone()), AuthMode::Any).await;
    let client = redis::Client::open(format!("redis://127.0.0.1:{}/", m.addr.port())).unwrap();
    let mut c = client.get_multiplexed_async_connection().await.unwrap();
    let mode: String = c.hget("cfg", "mode").await.unwrap();
    assert_eq!(mode, "dark");
    let n: i64 = c.scard("tags").await.unwrap();
    assert_eq!(n, 2);
    m.stop().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn fault_injection_replies_with_redis_errors() {
    use twinbase_mock::{FaultAction, FaultConfig, FaultRule};
    let m = mock(None, AuthMode::Any).await;
    let client = redis::Client::open(format!("redis://127.0.0.1:{}/", m.addr.port())).unwrap();
    let mut c = client.get_multiplexed_async_connection().await.unwrap();
    m.faults
        .set(FaultConfig {
            enabled: true,
            refuse_connections: false,
            rules: vec![FaultRule {
                enabled: true,
                name: String::new(),
                matches: "/^set /".into(),
                probability: 1.0,
                action: FaultAction::Error {
                    code: Some("READONLY".into()),
                    message: Some("You can't write against a read only replica.".into()),
                },
            }],
        })
        .unwrap();
    let err = c.set::<_, _, ()>("k", "v").await.unwrap_err();
    assert_eq!(err.code(), Some("READONLY"));
    let v: Option<String> = c.get("k").await.unwrap();
    assert_eq!(v, None);
}
