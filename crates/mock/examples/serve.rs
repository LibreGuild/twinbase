//! Start one mock server for manual testing: `cargo run -p twinbase-mock --example serve -- postgres 15432 [dir]`.

use twinbase_mock::{AuthMode, MockConfig, MockKind, start};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let kind = match args.get(1).map(String::as_str) {
        Some("mysql") => MockKind::Mysql,
        Some("mssql") => MockKind::Mssql,
        Some("redis") => MockKind::Redis,
        _ => MockKind::Postgres,
    };
    let port = args.get(2).and_then(|p| p.parse().ok()).unwrap_or(0);
    let data_dir = args.get(3).map(Into::into);
    let m =
        start(MockConfig { kind, host: "0.0.0.0".into(), port, data_dir, auth: AuthMode::Any, faults: Default::default(), native: None })
            .await
            .expect("start");
    println!("{kind:?} mock listening on {}", m.addr);
    tokio::signal::ctrl_c().await.ok();
    m.stop().await;
}
