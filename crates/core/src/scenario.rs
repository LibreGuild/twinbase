//! Scenario runner: run every saved query in a collection and check its expectation.

use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use serde_json::json;

use crate::app::App;
use crate::error::{CoreError, Result};
use crate::events::Actor;
use crate::model::Expectation;

#[derive(Debug, Clone, Serialize)]
pub struct ScenarioResult {
    pub id: String,
    pub name: String,
    pub passed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expectation: Option<Expectation>,
    /// What actually happened, e.g. "3 rows" or "error 23505".
    pub actual: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub elapsed_ms: f64,
}

pub async fn run(app: &Arc<App>, collection: &str, connection: Option<&str>, actor: &Actor) -> Result<Vec<ScenarioResult>> {
    let mut queries: Vec<_> = app.data().queries.into_iter().filter(|q| q.collection == collection).collect();
    if queries.is_empty() {
        return Err(CoreError::not_found(format!("collection '{collection}' has no queries")));
    }
    queries.sort_by(|a, b| a.name.cmp(&b.name));
    let mut out = Vec::new();
    for q in queries {
        let conn = connection.map(str::to_owned).or_else(|| q.connection.clone());
        let started = Instant::now();
        let Some(conn) = conn else {
            out.push(ScenarioResult {
                id: q.id,
                name: q.name,
                passed: false,
                expectation: q.expect,
                actual: "no connection".into(),
                error: Some("choose a connection for this query or the run".into()),
                elapsed_ms: 0.0,
            });
            continue;
        };
        // Boxed: the API dispatcher calls back into the scenario runner.
        let r = Box::pin(crate::api::call(app, "db.query", json!({ "connection": conn, "sql": q.sql }), actor)).await;
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        let (rows, err) = match &r {
            Ok(v) => {
                (v.as_array().and_then(|a| a.last()).and_then(|l| l.get("rows")).and_then(|r| r.as_array()).map(|r| r.len() as u64), None)
            }
            Err(e) => (None, Some(e.clone())),
        };
        let actual = match (&err, rows) {
            (Some(e), _) => format!("error {}", e.code.clone().unwrap_or_else(|| e.kind.clone())),
            (None, Some(n)) => format!("{n} row{}", if n == 1 { "" } else { "s" }),
            (None, None) => "ok".into(),
        };
        let passed = match (&q.expect, &err) {
            (None | Some(Expectation::Succeeds), e) => e.is_none(),
            (Some(Expectation::Empty), None) => rows.unwrap_or(0) == 0,
            (Some(Expectation::Rows { count }), None) => rows == Some(*count),
            (Some(Expectation::Fails { code }), Some(e)) => code.as_ref().is_none_or(|c| e.code.as_deref() == Some(c.as_str())),
            (Some(Expectation::Fails { .. }), None) => false,
            (Some(_), Some(_)) => false,
        };
        out.push(ScenarioResult {
            id: q.id,
            name: q.name,
            passed,
            expectation: q.expect,
            actual,
            error: err.map(|e| e.message),
            elapsed_ms,
        });
    }
    Ok(out)
}
