//! Query plans: run `EXPLAIN` in the best format each server offers and normalise the result into a
//! tree for the plan visualizer, with simple tuning hints.

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Default, Serialize)]
pub struct PlanNode {
    pub op: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<String>,
    /// Extra lines such as filters, join conditions and sort keys.
    pub details: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub est_rows: Option<f64>,
    /// Total estimated cost (planner units).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub est_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_rows: Option<f64>,
    /// Total time of this node including children, per loop, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loops: Option<f64>,
    pub children: Vec<PlanNode>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    /// `postgres-json`, `postgres-text`, `mysql-tree`, `mysql-table`, `sqlite`, `mssql` or `clickhouse`.
    pub format: String,
    pub analyzed: bool,
    pub roots: Vec<PlanNode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub planning_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_ms: Option<f64>,
    pub hints: Vec<String>,
    /// The server's output, for reference.
    pub raw: String,
}

impl Plan {
    fn new(format: &str, roots: Vec<PlanNode>, raw: String) -> Plan {
        let analyzed = roots.iter().any(has_actuals);
        let mut p = Plan { format: format.into(), analyzed, roots, planning_ms: None, execution_ms: None, hints: vec![], raw };
        p.hints = hints(&p);
        p
    }
}

fn has_actuals(n: &PlanNode) -> bool {
    n.actual_rows.is_some() || n.children.iter().any(has_actuals)
}

// ------------------------------------------------------------------ PostgreSQL JSON

/// `EXPLAIN (FORMAT JSON)` output: `[{"Plan": {...}, "Planning Time": .., "Execution Time": ..}]`.
pub fn parse_pg_json(text: &str) -> Option<Plan> {
    let v: Value = serde_json::from_str(text.trim()).ok()?;
    let top = v.as_array().and_then(|a| a.first()).unwrap_or(&v);
    let root = pg_json_node(top.get("Plan")?);
    let mut plan = Plan::new("postgres-json", vec![root], serde_json::to_string_pretty(&v).unwrap_or_default());
    plan.planning_ms = top.get("Planning Time").and_then(Value::as_f64);
    plan.execution_ms = top.get("Execution Time").and_then(Value::as_f64);
    Some(plan)
}

fn pg_json_node(v: &Value) -> PlanNode {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    let f = |k: &str| v.get(k).and_then(Value::as_f64);
    let mut op = s("Node Type").unwrap_or_else(|| "?".into());
    if let Some(j) = s("Join Type").filter(|_| op.contains("Join") || op == "Nested Loop") {
        op = format!("{op} ({j})");
    }
    if let Some(st) = s("Strategy").filter(|st| st != "Plain") {
        op = format!("{op} ({st})");
    }
    let mut details = Vec::new();
    for key in ["Filter", "Index Cond", "Recheck Cond", "Hash Cond", "Merge Cond", "Join Filter", "One-Time Filter", "Cache Key"] {
        if let Some(x) = s(key) {
            details.push(format!("{key}: {x}"));
        }
    }
    for key in ["Sort Key", "Group Key", "Presorted Key"] {
        if let Some(arr) = v.get(key).and_then(Value::as_array) {
            details.push(format!("{key}: {}", arr.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")));
        }
    }
    for key in ["Rows Removed by Filter", "Rows Removed by Index Recheck", "Rows Removed by Join Filter", "Heap Fetches"] {
        if let Some(x) = f(key).filter(|x| *x > 0.0) {
            details.push(format!("{key}: {x}"));
        }
    }
    if let (Some(m), Some(t)) = (s("Sort Method"), s("Sort Space Type")) {
        details.push(format!("Sort Method: {m} ({t}, {} kB)", f("Sort Space Used").unwrap_or(0.0)));
    }
    if let Some(hit) = f("Shared Hit Blocks") {
        let read = f("Shared Read Blocks").unwrap_or(0.0);
        if hit + read > 0.0 {
            details.push(format!("Buffers: shared hit={hit} read={read}"));
        }
    }
    let relation = s("Relation Name").map(|r| match s("Alias") {
        Some(a) if a != r => format!("{r} {a}"),
        _ => r,
    });
    PlanNode {
        op,
        relation: relation.or_else(|| s("CTE Name").map(|c| format!("CTE {c}"))),
        index: s("Index Name"),
        details,
        est_rows: f("Plan Rows"),
        est_cost: f("Total Cost"),
        actual_rows: f("Actual Rows"),
        actual_ms: f("Actual Total Time"),
        loops: f("Actual Loops"),
        children: v.get("Plans").and_then(Value::as_array).map(|a| a.iter().map(pg_json_node).collect()).unwrap_or_default(),
    }
}

// ------------------------------------------------------------------ indented text (PostgreSQL text, MySQL TREE)

/// Numbers after `key=` in a line, e.g. `rows=3` or `time=0.02..0.04` (returns the last value).
fn num_after(line: &str, key: &str) -> Option<f64> {
    let i = line.find(key)? + key.len();
    let rest = &line[i..];
    let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == 'e' || c == '-' || c == '+')).unwrap_or(rest.len());
    let token = &rest[..end];
    token.rsplit("..").next()?.parse().ok()
}

fn split_metrics(line: &str) -> (String, Option<&str>, Option<&str>) {
    let est = line.find("(cost=");
    let act = line.find("(actual ");
    let cut = [est, act, line.find("(never executed)")].into_iter().flatten().min().unwrap_or(line.len());
    (line[..cut].trim().to_owned(), est.map(|i| &line[i..]), act.map(|i| &line[i..]))
}

fn node_from_line(text: &str) -> PlanNode {
    let (name, est, act) = split_metrics(text);
    let mut n = PlanNode::default();
    // Relation and index names, in PostgreSQL ("Seq Scan on t", "Index Scan using i on t"),
    // MySQL ("Table scan on t", "Index lookup on t using i") and SQLite ("SCAN t", "SEARCH t USING INDEX i").
    let lower = name.to_ascii_lowercase();
    let word_after = |needle: &str| -> Option<String> {
        let i = lower.find(needle)? + needle.len();
        name[i..].split_whitespace().next().map(|w| w.trim_matches(|c: char| c == '(' || c == ')' || c == ',').to_owned())
    };
    n.relation = word_after(" on ").or_else(|| {
        let first = name.split_whitespace().next()?.to_ascii_uppercase();
        matches!(first.as_str(), "SCAN" | "SEARCH").then(|| name.split_whitespace().nth(1).map(str::to_owned)).flatten()
    });
    n.index = word_after(" using index ").or_else(|| word_after(" using covering index ")).or_else(|| word_after(" using "));
    if n.index
        .as_deref()
        .is_some_and(|i| i.eq_ignore_ascii_case("integer") || i.eq_ignore_ascii_case("index") || i.eq_ignore_ascii_case("temp"))
    {
        n.index = None;
    }
    n.op = name;
    if let Some(e) = est {
        n.est_cost = num_after(e, "cost=");
        n.est_rows = num_after(e, "rows=");
    }
    if let Some(a) = act {
        n.actual_ms = num_after(a, "time=");
        n.actual_rows = num_after(a, "rows=");
        n.loops = num_after(a, "loops=");
    }
    n
}

/// Parse plans printed as indented lines with `->` before child nodes.
fn parse_indented(text: &str, format: &str, every_line_is_node: bool) -> Plan {
    let mut roots: Vec<PlanNode> = Vec::new();
    // Path of (indent, index) pairs to the most recent node.
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut planning = None;
    let mut execution = None;

    fn node_at<'a>(roots: &'a mut [PlanNode], path: &[(usize, usize)]) -> &'a mut PlanNode {
        let mut n = &mut roots[path[0].1];
        for (_, i) in &path[1..] {
            n = &mut n.children[*i];
        }
        n
    }

    for raw_line in text.lines() {
        if raw_line.trim().is_empty() {
            continue;
        }
        let trimmed = raw_line.trim_start();
        let lead = raw_line.len() - trimmed.len();
        if let Some(rest) = trimmed.strip_prefix("Planning Time:").or_else(|| trimmed.strip_prefix("Planning time:")) {
            planning = rest.trim().trim_end_matches("ms").trim().parse().ok();
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("Execution Time:").or_else(|| trimmed.strip_prefix("Execution time:")) {
            execution = rest.trim().trim_end_matches("ms").trim().parse().ok();
            continue;
        }
        let is_child = trimmed.starts_with("->");
        let is_node = is_child || every_line_is_node || stack.is_empty() || lead == 0;
        if !is_node {
            if !stack.is_empty() {
                node_at(&mut roots, &stack).details.push(trimmed.to_owned());
            }
            continue;
        }
        let body = trimmed.trim_start_matches("->").trim();
        let node = node_from_line(body);
        let indent = lead;
        while stack.last().is_some_and(|(i, _)| *i >= indent) {
            stack.pop();
        }
        if stack.is_empty() {
            roots.push(node);
            stack.push((indent, roots.len() - 1));
        } else {
            let parent = node_at(&mut roots, &stack);
            parent.children.push(node);
            let idx = parent.children.len() - 1;
            stack.push((indent, idx));
        }
    }
    let mut plan = Plan::new(format, roots, text.to_owned());
    plan.planning_ms = planning;
    plan.execution_ms = execution;
    plan
}

/// `EXPLAIN [ANALYZE]` in PostgreSQL's text format (also what the TwinLite mock prints).
pub fn parse_pg_text(text: &str) -> Plan {
    parse_indented(text, "postgres-text", false)
}

/// MySQL 8 `EXPLAIN FORMAT=TREE` / `EXPLAIN ANALYZE`.
pub fn parse_mysql_tree(text: &str) -> Plan {
    parse_indented(text, "mysql-tree", true)
}

/// Classic tabular MySQL `EXPLAIN` (one row per table access).
pub fn parse_mysql_table(columns: &[String], rows: &[Vec<Value>]) -> Plan {
    let col = |name: &str| columns.iter().position(|c| c.eq_ignore_ascii_case(name));
    let text = |row: &Vec<Value>, name: &str| -> Option<String> {
        let v = row.get(col(name)?)?;
        match v {
            Value::Null => None,
            Value::String(s) if s.is_empty() => None,
            Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        }
    };
    let mut raw = String::new();
    let nodes: Vec<PlanNode> = rows
        .iter()
        .map(|r| {
            let access = text(r, "type").unwrap_or_default();
            let table = text(r, "table");
            let extra = text(r, "Extra");
            raw.push_str(&format!(
                "{} {} {} {}\n",
                table.clone().unwrap_or_default(),
                access,
                text(r, "key").unwrap_or_default(),
                extra.clone().unwrap_or_default()
            ));
            let op = match access.as_str() {
                "ALL" => "Full table scan".to_owned(),
                "index" => "Full index scan".to_owned(),
                "range" => "Index range scan".to_owned(),
                "ref" | "eq_ref" | "const" | "system" => format!("Index lookup ({access})"),
                "" => extra.clone().unwrap_or_else(|| "Step".into()),
                other => other.to_owned(),
            };
            let mut details = Vec::new();
            if let Some(k) = text(r, "possible_keys") {
                details.push(format!("Possible keys: {k}"));
            }
            if let Some(x) = extra.filter(|_| !access.is_empty()) {
                details.push(x);
            }
            PlanNode {
                op,
                relation: table,
                index: text(r, "key"),
                details,
                est_rows: text(r, "rows").and_then(|x| x.parse().ok()),
                ..Default::default()
            }
        })
        .collect();
    Plan::new("mysql-table", nodes, raw)
}

/// Oracle PLAN_TABLE rows: id, parent_id, operation, options, object_name, cardinality, cost,
/// access predicates, filter predicates.
pub fn parse_oracle(rows: &[Vec<Value>]) -> Plan {
    let num = |v: Option<&Value>| v.and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())));
    let text = |v: Option<&Value>| v.and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(str::to_owned);
    let nodes: Vec<(i64, Option<i64>, PlanNode)> = rows
        .iter()
        .map(|r| {
            let op = format!("{}{}", text(r.get(2)).unwrap_or_default(), text(r.get(3)).map(|o| format!(" {o}")).unwrap_or_default());
            let mut details = Vec::new();
            if let Some(a) = text(r.get(7)) {
                details.push(format!("Access: {a}"));
            }
            if let Some(f) = text(r.get(8)) {
                details.push(format!("Filter: {f}"));
            }
            let full = op == "TABLE ACCESS FULL";
            let node = PlanNode {
                op: if full { "Table scan (TABLE ACCESS FULL)".into() } else { op },
                relation: if full || text(r.get(2)).is_some_and(|o| o.starts_with("TABLE")) { text(r.get(4)) } else { None },
                index: text(r.get(2)).filter(|o| o.starts_with("INDEX")).and(text(r.get(4))),
                details,
                est_rows: num(r.get(5)),
                est_cost: num(r.get(6)),
                ..Default::default()
            };
            (num(r.first()).unwrap_or(0.0) as i64, num(r.get(1)).map(|p| p as i64), node)
        })
        .collect();
    fn build(parent: Option<i64>, nodes: &[(i64, Option<i64>, PlanNode)], depth: usize) -> Vec<PlanNode> {
        if depth > 200 {
            return vec![];
        }
        nodes.iter().filter(|n| n.1 == parent).map(|n| PlanNode { children: build(Some(n.0), nodes, depth + 1), ..n.2.clone() }).collect()
    }
    Plan::new(
        "oracle",
        build(None, &nodes, 0),
        rows.iter().map(|r| r.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(" ")).collect::<Vec<_>>().join("\n"),
    )
}

/// Spanner `queryPlan.planNodes` (PLAN or PROFILE mode): relational operators as the tree.
pub fn parse_spanner(stats: &Value) -> Plan {
    let nodes = stats["queryPlan"]["planNodes"].as_array().cloned().unwrap_or_default();
    let by_index: std::collections::HashMap<u64, &Value> = nodes.iter().filter_map(|n| n["index"].as_u64().map(|i| (i, n))).collect();
    let desc = |i: u64| by_index.get(&i).and_then(|n| n["shortRepresentation"]["description"].as_str()).unwrap_or("").to_owned();
    fn num(v: &Value) -> Option<f64> {
        v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    }
    fn build(i: u64, by_index: &std::collections::HashMap<u64, &Value>, desc: &dyn Fn(u64) -> String, depth: usize) -> Option<PlanNode> {
        let n = by_index.get(&i)?;
        if depth > 200 {
            return None;
        }
        let meta = &n["metadata"];
        let mut node = PlanNode { op: n["displayName"].as_str().unwrap_or("Step").to_owned(), ..Default::default() };
        if let Some(t) = meta["scan_target"].as_str() {
            node.relation = Some(t.to_owned());
        }
        if meta["scan_type"].as_str() == Some("IndexScan") {
            node.index = meta["scan_target"].as_str().map(str::to_owned);
        }
        if meta["Full scan"].as_str() == Some("true") || meta["full_scan"].as_str() == Some("true") {
            node.op = format!("Table scan ({})", node.op);
        }
        for (k, v) in meta.as_object().into_iter().flatten() {
            if let Some(s) = v.as_str().filter(|_| !matches!(k.as_str(), "scan_target" | "scan_type" | "Full scan" | "full_scan")) {
                node.details.push(format!("{k}: {s}"));
            }
        }
        node.actual_rows = num(&n["executionStats"]["rows"]["total"]);
        node.actual_ms = num(&n["executionStats"]["latency"]["total"])
            .map(|t| if n["executionStats"]["latency"]["unit"].as_str() == Some("secs") { t * 1000.0 } else { t });
        for link in n["childLinks"].as_array().into_iter().flatten() {
            let Some(c) = link["childIndex"].as_u64() else { continue };
            let child = by_index.get(&c);
            if child.is_some_and(|c| c["kind"].as_str() == Some("RELATIONAL")) {
                if let Some(cn) = build(c, by_index, desc, depth + 1) {
                    node.children.push(cn);
                }
            } else if let Some(kind) =
                link["type"].as_str().filter(|t| matches!(*t, "Condition" | "Split Range" | "Seek Condition" | "Residual Condition"))
            {
                let d = desc(c);
                if !d.is_empty() {
                    node.details
                        .push(format!("{}: {d}", if kind == "Condition" || kind == "Residual Condition" { "Filter" } else { kind }));
                }
            }
        }
        Some(node)
    }
    let roots: Vec<PlanNode> = build(0, &by_index, &desc, 0).into_iter().filter(|n| n.op != "No query plan").collect();
    let mut plan = Plan::new("spanner", roots, stats["queryPlan"].to_string());
    if plan.roots.is_empty() {
        plan.hints.push("The server returned no query plan (the Spanner emulator does not produce plans).".into());
    }
    plan
}

/// BigQuery dry run: the bytes a query would scan, and so its on-demand cost.
pub fn parse_bigquery_dry_run(stats: &Value) -> Plan {
    let bytes = stats["totalBytesProcessed"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .or_else(|| stats["totalBytesProcessed"].as_f64())
        .unwrap_or(0.0);
    let gib = bytes / 1_073_741_824.0;
    let tables: Vec<String> = stats["query"]["referencedTables"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| format!("{}.{}", t["datasetId"].as_str().unwrap_or(""), t["tableId"].as_str().unwrap_or("")))
        .collect();
    let mut node = PlanNode { op: "Query (dry run)".into(), relation: tables.first().cloned(), ..Default::default() };
    node.details.push(format!(
        "Would scan {} ({bytes:.0} bytes)",
        if gib >= 1.0 { format!("{gib:.2} GiB") } else { format!("{:.1} MiB", bytes / 1_048_576.0) }
    ));
    // On-demand pricing: $6.25 per TiB scanned, the first 10 MiB of each query minimum.
    node.details.push(format!("Estimated on-demand cost: ${:.4}", (bytes.max(10.0 * 1_048_576.0) / 1_099_511_627_776.0) * 6.25));
    if tables.len() > 1 {
        node.details.push(format!("Tables: {}", tables.join(", ")));
    }
    let mut plan = Plan::new("bigquery", vec![node], stats.to_string());
    if gib >= 1.0 {
        plan.hints.push(format!(
            "This query scans {gib:.1} GiB. Select only the columns you need and filter on partitioning or clustering columns to scan less."
        ));
    }
    plan
}

/// ClickHouse `EXPLAIN json = 1, indexes = 1`: nodes with index usage per MergeTree read.
pub fn parse_clickhouse(text: &str) -> Plan {
    fn node(v: &Value, filtered: bool, hints: &mut Vec<String>) -> PlanNode {
        let op = v["Node Type"].as_str().unwrap_or("Step").to_owned();
        let description = v["Description"].as_str().unwrap_or("").to_owned();
        let filtered = filtered || description.contains("WHERE") || op == "Filter";
        let mut n = PlanNode { op: op.clone(), ..Default::default() };
        if op.starts_with("ReadFrom") && !description.is_empty() {
            n.relation = Some(description.rsplit('.').next().unwrap_or(&description).to_owned());
        } else if !description.is_empty() {
            n.details.push(description.trim_matches(['(', ')']).to_owned());
        }
        let mut whole = false;
        for ix in v["Indexes"].as_array().into_iter().flatten() {
            let kind = ix["Type"].as_str().unwrap_or("Index");
            let (init, sel) = (ix["Initial Granules"].as_u64(), ix["Selected Granules"].as_u64());
            let mut line = format!("{kind}: {}", ix["Condition"].as_str().or(ix["Name"].as_str()).unwrap_or(""));
            if let (Some(i), Some(s)) = (init, sel) {
                line.push_str(&format!(" · granules {s} of {i}"));
                if kind == "PrimaryKey" && s == i && i > 1 {
                    whole = true;
                }
            }
            if kind == "PrimaryKey" {
                n.index = Some("primary key".into());
            }
            n.details.push(line);
        }
        if whole {
            n.op = format!("Full scan ({op})");
            if filtered && let Some(t) = &n.relation {
                hints.push(format!(
                    "{t} is read completely: the WHERE condition does not use its sorting key (ORDER BY) or a data-skipping index."
                ));
            }
        }
        n.children = v["Plans"].as_array().into_iter().flatten().map(|c| node(c, filtered, hints)).collect();
        n
    }
    let v: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let mut extra = Vec::new();
    let roots: Vec<PlanNode> = v.as_array().into_iter().flatten().map(|p| node(&p["Plan"], false, &mut extra)).collect();
    let mut plan = Plan::new("clickhouse", roots, text.to_owned());
    plan.hints.extend(extra);
    plan
}

/// `[db].[dbo].[orders].[total]` → `total`; `OBJECT:([db].[dbo].[orders].[ix])` parts.
fn mssql_unqualify(s: &str) -> String {
    let re = regex::Regex::new(r"(?:\[[^\]]*\]\.)+\[([^\]]*)\]").expect("valid regex");
    let s = re.replace_all(s, "$1");
    s.replace(['[', ']'], "")
}

/// SQL Server `SHOWPLAN_ALL` / `STATISTICS PROFILE` rows (NodeId/Parent tree).
pub fn parse_mssql(columns: &[String], rows: &[Vec<Value>]) -> Plan {
    let col = |name: &str| columns.iter().position(|c| c.eq_ignore_ascii_case(name));
    let text = |row: &Vec<Value>, name: &str| -> Option<String> {
        match row.get(col(name)?)? {
            Value::Null => None,
            Value::String(s) if s.trim().is_empty() => None,
            Value::String(s) => Some(s.trim().to_owned()),
            other => Some(other.to_string()),
        }
    };
    let num = |row: &Vec<Value>, name: &str| text(row, name).and_then(|s| s.parse::<f64>().ok());
    let object = regex::Regex::new(r"OBJECT:\(((?:\[[^\]]*\]\.?)+)(?:\s+AS\s+\[[^\]]*\])?\)").expect("valid regex");
    let mut raw = String::new();
    let mut nodes: Vec<(i64, i64, PlanNode)> = Vec::new();
    for r in rows {
        if let Some(s) = text(r, "StmtText") {
            raw.push_str(&s);
            raw.push('\n');
        }
        // The first row of each statement describes the statement itself, not an operator.
        if text(r, "Type").is_some_and(|t| t != "PLAN_ROW") || text(r, "PhysicalOp").is_none() {
            continue;
        }
        let id = num(r, "NodeId").unwrap_or(0.0) as i64;
        let parent = num(r, "Parent").unwrap_or(0.0) as i64;
        let physical = text(r, "PhysicalOp").unwrap_or_default();
        let logical = text(r, "LogicalOp").unwrap_or_default();
        let argument = text(r, "Argument").unwrap_or_default();
        let (mut relation, mut index) = (None, None);
        if let Some(m) = object.captures(&argument) {
            let parts: Vec<String> =
                m[1].split("].").map(|p| p.trim_matches(['[', ']', '.']).to_owned()).filter(|p| !p.is_empty()).collect();
            // [db].[schema].[table].[index] or [db].[schema].[table]
            match parts.len() {
                4 => {
                    relation = Some(parts[2].clone());
                    index = Some(parts[3].clone());
                }
                n if n >= 1 => relation = Some(parts[n.min(3) - 1].clone()),
                _ => {}
            }
        }
        let mut details = Vec::new();
        if logical != physical && !logical.is_empty() {
            details.push(format!("Logical: {logical}"));
        }
        if let Some(p) = argument.find("WHERE:(") {
            let w = &argument[p + 7..];
            let mut depth = 1;
            let end = w.char_indices().find(|&(_, c)| {
                match c {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                depth == 0
            });
            let cond = end.map(|(i, _)| &w[..i]).unwrap_or(w);
            details.push(format!("Filter: {}", mssql_unqualify(cond)));
        } else if !argument.is_empty() && relation.is_none() {
            details.push(mssql_unqualify(&argument));
        }
        // Clustered index scans read the whole table.
        let op = if physical == "Clustered Index Scan" { "Table Scan (clustered index)".to_owned() } else { physical };
        let executes = num(r, "Executes");
        let node = PlanNode {
            op,
            relation,
            index,
            details,
            est_rows: num(r, "EstimateRows"),
            est_cost: num(r, "TotalSubtreeCost"),
            actual_rows: num(r, "Rows").map(|rows| rows / executes.unwrap_or(1.0).max(1.0)),
            loops: executes,
            ..Default::default()
        };
        nodes.push((id, parent, node));
    }
    fn build(parent: i64, nodes: &[(i64, i64, PlanNode)], depth: usize) -> Vec<PlanNode> {
        if depth > 200 {
            return vec![];
        }
        nodes.iter().filter(|n| n.1 == parent).map(|n| PlanNode { children: build(n.0, nodes, depth + 1), ..n.2.clone() }).collect()
    }
    let ids: std::collections::HashSet<i64> = nodes.iter().map(|n| n.0).collect();
    // Roots: operators whose parent is not an operator (one per statement).
    let mut roots = Vec::new();
    for n in nodes.iter().filter(|n| !ids.contains(&n.1)) {
        roots.push(PlanNode { children: build(n.0, &nodes, 0), ..n.2.clone() });
    }
    Plan::new("mssql", roots, raw)
}

/// SQLite `EXPLAIN QUERY PLAN` rows: (id, parent, detail).
pub fn parse_sqlite(entries: &[(i64, i64, String)]) -> Plan {
    fn build(parent: i64, entries: &[(i64, i64, String)]) -> Vec<PlanNode> {
        entries.iter().filter(|e| e.1 == parent).map(|e| PlanNode { children: build(e.0, entries), ..node_from_line(&e.2) }).collect()
    }
    let raw = entries.iter().map(|e| e.2.clone()).collect::<Vec<_>>().join("\n");
    Plan::new("sqlite", build(0, entries), raw)
}

// ------------------------------------------------------------------ running EXPLAIN

fn cell_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

async fn run_explain(
    client: &dyn twinbase_client::SqlClient,
    sql: &str,
    rolled_back: bool,
) -> twinbase_client::Result<Vec<twinbase_client::QueryResult>> {
    if rolled_back { client.query_rolled_back(sql, 10_000).await } else { client.query(sql, 10_000).await }
}

/// The last result set that has columns (skips BEGIN/ROLLBACK results).
fn plan_result(results: Vec<twinbase_client::QueryResult>) -> Option<twinbase_client::QueryResult> {
    results.into_iter().rev().find(|r| !r.columns.is_empty())
}

/// Explain one statement. With `analyze`, the statement really runs — inside a transaction that is
/// rolled back, so writes leave no trace (sequences may still advance).
pub async fn run(client: &dyn twinbase_client::SqlClient, statement: &str, analyze: bool) -> crate::error::Result<Plan> {
    use twinbase_client::SqlDialect;
    let stmt = statement.trim().trim_end_matches(';').trim();
    match client.dialect() {
        SqlDialect::Postgres => {
            let json =
                if analyze { format!("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {stmt}") } else { format!("EXPLAIN (FORMAT JSON) {stmt}") };
            if let Ok(results) = run_explain(client, &json, analyze).await
                && let Some(r) = plan_result(results)
            {
                let text: String = r.rows.iter().map(|row| row.first().map(cell_text).unwrap_or_default()).collect::<Vec<_>>().join("\n");
                if let Some(plan) = parse_pg_json(&text) {
                    return Ok(plan);
                }
                return Ok(parse_pg_text(&text));
            }
            // Servers (or the TwinLite mock) without JSON plans: the text format.
            let plain = if analyze { format!("EXPLAIN ANALYZE {stmt}") } else { format!("EXPLAIN {stmt}") };
            let r = plan_result(run_explain(client, &plain, analyze).await?).unwrap_or_default();
            Ok(parse_pg_text(&r.rows.iter().map(|row| row.first().map(cell_text).unwrap_or_default()).collect::<Vec<_>>().join("\n")))
        }
        SqlDialect::Mysql => {
            let tree = if analyze { format!("EXPLAIN ANALYZE {stmt}") } else { format!("EXPLAIN FORMAT=TREE {stmt}") };
            if let Ok(results) = run_explain(client, &tree, analyze).await
                && let Some(r) = plan_result(results)
                && r.columns.len() == 1
            {
                let text = r.rows.iter().map(|row| row.first().map(cell_text).unwrap_or_default()).collect::<Vec<_>>().join("\n");
                if text.trim_start().starts_with("->") {
                    return Ok(parse_mysql_tree(&text));
                }
            }
            let r = plan_result(client.query(&format!("EXPLAIN {stmt}"), 1000).await?).unwrap_or_default();
            let cols: Vec<String> = r.columns.iter().map(|c| c.name.clone()).collect();
            let mut plan = parse_mysql_table(&cols, &r.rows);
            if analyze {
                plan.hints.insert(0, "This server does not support EXPLAIN ANALYZE; showing estimates only.".into());
            }
            Ok(plan)
        }
        SqlDialect::Oracle => {
            // EXPLAIN PLAN into the session's PLAN_TABLE, read it back, then remove the rows.
            let id = format!("TB{}", uuid::Uuid::new_v4().simple().to_string()[..12].to_ascii_uppercase());
            let r = client
                .query_wrapped(
                    &format!("EXPLAIN PLAN SET STATEMENT_ID = '{id}' FOR {stmt}"),
                    &format!(
                        "SELECT id, parent_id, operation, options, object_name, cardinality, cost, access_predicates, filter_predicates FROM plan_table WHERE statement_id = '{id}' ORDER BY id"
                    ),
                    &format!("DELETE FROM plan_table WHERE statement_id = '{id}'"),
                    1000,
                    false,
                )
                .await?;
            let rows = r.into_iter().rev().find(|x| !x.columns.is_empty()).map(|x| x.rows).unwrap_or_default();
            let mut plan = parse_oracle(&rows);
            if analyze {
                plan.hints.insert(0, "Showing the optimizer's estimates; actual row counts need the gather_plan_statistics hint and DBMS_XPLAN.DISPLAY_CURSOR.".into());
            }
            Ok(plan)
        }
        SqlDialect::Spanner => {
            let stats = client.native_plan(stmt, analyze).await?.unwrap_or_default();
            Ok(parse_spanner(&stats))
        }
        SqlDialect::Bigquery => {
            let v = client.native_plan(stmt, analyze).await?.unwrap_or_default();
            let mut plan = parse_bigquery_dry_run(&v["dry_run"]);
            if analyze {
                plan.hints.insert(
                    0,
                    "BigQuery has no EXPLAIN ANALYZE; after a run, the job's execution details in the console show each stage.".into(),
                );
            }
            Ok(plan)
        }
        SqlDialect::Cassandra => {
            // CQL has no planner: the shape of the WHERE clause decides the cost.
            let upper = stmt.to_ascii_uppercase();
            let mut node = PlanNode { op: "CQL request".into(), details: vec![stmt.to_owned()], ..Default::default() };
            let mut hints = Vec::new();
            if upper.contains("ALLOW FILTERING") {
                node.op = "Full scan (ALLOW FILTERING)".into();
                hints.push("ALLOW FILTERING reads every partition on every node. Query by the partition key, or add a table or materialized view keyed by the filtered column.".to_owned());
            } else if upper.trim_start().starts_with("SELECT") && !upper.contains(" WHERE ") {
                node.op = "Full scan".into();
                hints.push(
                    "A SELECT without a partition key reads the whole table across the cluster; use LIMIT or query one partition."
                        .to_owned(),
                );
            }
            if analyze {
                hints.insert(0, "Cassandra has no EXPLAIN; run the statement with TRACING ON in cqlsh to see where time goes.".into());
            }
            let mut plan = Plan::new("cassandra", vec![node], stmt.to_owned());
            plan.hints.extend(hints);
            Ok(plan)
        }
        SqlDialect::Clickhouse => {
            let r = plan_result(client.query(&format!("EXPLAIN json = 1, indexes = 1 {stmt}"), 10).await?).unwrap_or_default();
            let text = r.rows.first().and_then(|row| row.first()).map(cell_text).unwrap_or_default();
            let mut plan = parse_clickhouse(&text);
            if analyze {
                plan.hints.insert(0, "ClickHouse has no EXPLAIN ANALYZE; showing the plan with index usage. system.query_log has the actual rows and time of past runs.".into());
            }
            Ok(plan)
        }
        SqlDialect::Mssql => {
            // SHOWPLAN_ALL: estimated plan, the statement does not run. STATISTICS PROFILE: the
            // statement runs (inside a rolled-back transaction) and a profile with actual rows follows.
            let (on, off) = if analyze {
                ("SET STATISTICS PROFILE ON", "SET STATISTICS PROFILE OFF")
            } else {
                ("SET SHOWPLAN_ALL ON", "SET SHOWPLAN_ALL OFF")
            };
            let results = client.query_wrapped(on, stmt, off, 10_000, analyze).await?;
            let profile = results.into_iter().rev().find(|r| r.columns.iter().any(|c| c.name == "PhysicalOp")).unwrap_or_default();
            let cols: Vec<String> = profile.columns.iter().map(|c| c.name.clone()).collect();
            Ok(parse_mssql(&cols, &profile.rows))
        }
        SqlDialect::Sqlite => {
            let r = plan_result(client.query(&format!("EXPLAIN QUERY PLAN {stmt}"), 10_000).await?).unwrap_or_default();
            let entries: Vec<(i64, i64, String)> = r
                .rows
                .iter()
                .map(|row| {
                    let n = |i: usize| row.get(i).and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).unwrap_or(0);
                    (n(0), n(1), row.get(3).map(cell_text).unwrap_or_default())
                })
                .collect();
            let mut plan = parse_sqlite(&entries);
            if analyze {
                plan.hints.insert(0, "SQLite has no EXPLAIN ANALYZE; showing the query plan only.".into());
            }
            Ok(plan)
        }
    }
}

// ------------------------------------------------------------------ hints

fn walk<'a>(n: &'a PlanNode, out: &mut Vec<&'a PlanNode>) {
    out.push(n);
    for c in &n.children {
        walk(c, out);
    }
}

/// Columns compared in a filter such as `(total > 100)` or `(c.email = 'x')`.
fn filter_columns(filter: &str) -> Vec<String> {
    let re =
        regex::Regex::new(r"\(?\s*(?:[A-Za-z_][\w]*\.)?([A-Za-z_][\w]*)\s*(?:=|<>|!=|<=|>=|<|>|~~|LIKE|IN\b|IS\b)").expect("valid regex");
    let mut out: Vec<String> = re.captures_iter(filter).map(|c| c[1].to_owned()).collect();
    out.retain(|c| !matches!(c.to_ascii_uppercase().as_str(), "AND" | "OR" | "NOT" | "NULL" | "TRUE" | "FALSE"));
    out.dedup();
    out
}

fn hints(plan: &Plan) -> Vec<String> {
    let mut all = Vec::new();
    for r in &plan.roots {
        walk(r, &mut all);
    }
    let mut out = Vec::new();
    for n in all {
        let op = n.op.to_ascii_lowercase();
        let rows = n.actual_rows.or(n.est_rows).unwrap_or(0.0);
        let full_scan = op.starts_with("seq scan") || op.starts_with("table scan") || op == "full table scan" || op.starts_with("scan ");
        if full_scan && let Some(rel) = &n.relation {
            let filter = n.details.iter().find_map(|d| d.strip_prefix("Filter: ").or_else(|| d.strip_prefix("Filter:")));
            match filter {
                Some(f) if rows >= 1000.0 || n.details.iter().any(|d| d.starts_with("Rows Removed by Filter")) => {
                    let cols = filter_columns(f);
                    if !cols.is_empty() {
                        out.push(format!(
                            "Full scan of {rel} with a filter on {}: an index on ({}) could avoid reading every row.",
                            cols.join(", "),
                            cols.join(", ")
                        ));
                    }
                }
                None if rows >= 10_000.0 => {
                    out.push(format!("Full scan of {rel} (~{rows:.0} rows). Add a WHERE clause or an index if this runs often."))
                }
                _ => {}
            }
        }
        if let (Some(est), Some(act)) = (n.est_rows, n.actual_rows)
            && (est.max(1.0) / act.max(1.0) > 10.0 || act.max(1.0) / est.max(1.0) > 10.0)
            && (est - act).abs() > 100.0
        {
            out.push(format!(
                "{}: the planner expected ~{est:.0} rows but got {act:.0}. Outdated statistics? Try ANALYZE{}.",
                n.op,
                n.relation.as_deref().map(|r| format!(" {}", r.split_whitespace().next().unwrap_or(r))).unwrap_or_default()
            ));
        }
        if n.details.iter().any(|d| d.contains("external merge") || d.contains("Disk")) {
            out.push(format!("{} spills to disk; more work_mem or an index matching the sort order would help.", n.op));
        }
        if op.contains("using temporary") || n.details.iter().any(|d| d.contains("Using temporary") || d.contains("Using filesort")) {
            out.push(format!(
                "{} uses a temporary table or filesort; an index matching GROUP BY / ORDER BY can avoid it.",
                n.relation.as_deref().unwrap_or("A step")
            ));
        }
        if op.starts_with("use temp b-tree") {
            out.push(format!(
                "SQLite builds a temporary B-tree ({}); an index on the ORDER BY / GROUP BY columns avoids it.",
                n.op.to_lowercase()
            ));
        }
    }
    out.dedup();
    out
}

#[cfg(test)]
mod tests {

    #[test]
    fn oracle_plan_table_rows() {
        use serde_json::json;
        let rows = vec![
            vec![
                json!(0),
                json!(null),
                json!("SELECT STATEMENT"),
                json!(null),
                json!(null),
                json!(5000),
                json!(12),
                json!(null),
                json!(null),
            ],
            vec![
                json!(1),
                json!(0),
                json!("TABLE ACCESS"),
                json!("FULL"),
                json!("ORDERS"),
                json!(5000),
                json!(12),
                json!(null),
                json!("\"TOTAL\">100"),
            ],
        ];
        let p = parse_oracle(&rows);
        let scan = &p.roots[0].children[0];
        assert_eq!(scan.relation.as_deref(), Some("ORDERS"));
        assert_eq!(scan.details, vec!["Filter: \"TOTAL\">100"]);
        assert_eq!(p.roots[0].est_cost, Some(12.0));
    }

    #[test]
    fn clickhouse_json_plans() {
        let text = r#"[{"Plan": {"Node Type": "Expression", "Description": "(Project names + Projection)", "Plans": [{"Node Type": "Expression", "Description": "(WHERE + Change column names to column identifiers)", "Plans": [{"Node Type": "ReadFromMergeTree", "Description": "default.tbx", "Indexes": [{"Type": "PrimaryKey", "Condition": "true", "Initial Parts": 1, "Selected Parts": 1, "Initial Granules": 12, "Selected Granules": 12}]}]}]}}]"#;
        let p = parse_clickhouse(text);
        let read = &p.roots[0].children[0].children[0];
        assert_eq!(read.op, "Full scan (ReadFromMergeTree)");
        assert_eq!(read.relation.as_deref(), Some("tbx"));
        assert_eq!(read.details, vec!["PrimaryKey: true · granules 12 of 12"]);
        assert!(p.hints.iter().any(|h| h.contains("tbx is read completely")), "{:?}", p.hints);
    }

    #[test]
    fn sql_server_showplan_rows() {
        use serde_json::json;
        let cols: Vec<String> =
            ["StmtText", "NodeId", "Parent", "PhysicalOp", "LogicalOp", "Argument", "EstimateRows", "TotalSubtreeCost", "Type"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        let rows = vec![
            vec![
                json!("SELECT * FROM orders WHERE total > 100"),
                json!(1),
                json!(0),
                json!(null),
                json!(null),
                json!(null),
                json!("5000"),
                json!("0.4"),
                json!("SELECT"),
            ],
            vec![
                json!("  |--Clustered Index Scan(...)"),
                json!(2),
                json!(1),
                json!("Clustered Index Scan"),
                json!("Clustered Index Scan"),
                json!("OBJECT:([shop].[dbo].[orders].[PK__orders__3213E83F]), WHERE:([shop].[dbo].[orders].[total]>(100.00))"),
                json!("5000"),
                json!("0.4"),
                json!("PLAN_ROW"),
            ],
        ];
        let p = parse_mssql(&cols, &rows);
        assert_eq!(p.roots.len(), 1);
        let n = &p.roots[0];
        assert_eq!(n.relation.as_deref(), Some("orders"));
        assert_eq!(n.index.as_deref(), Some("PK__orders__3213E83F"));
        assert_eq!(n.details, vec!["Filter: total>(100.00)"]);
        assert!(p.hints.iter().any(|h| h.contains("index on (total)")), "{:?}", p.hints);
    }
    use super::*;

    #[test]
    fn postgres_json_plans_with_actuals() {
        let json = r#"[{"Plan": {"Node Type": "Hash Join", "Join Type": "Inner", "Startup Cost": 1.07, "Total Cost": 25.3, "Plan Rows": 3, "Actual Total Time": 0.05, "Actual Rows": 3, "Actual Loops": 1,
            "Hash Cond": "(o.customer_id = c.id)", "Plans": [
              {"Node Type": "Seq Scan", "Relation Name": "orders", "Alias": "o", "Total Cost": 20.5, "Plan Rows": 2, "Actual Rows": 5000, "Actual Total Time": 4.1, "Actual Loops": 1, "Filter": "(total > '100'::numeric)", "Rows Removed by Filter": 45000},
              {"Node Type": "Hash", "Total Cost": 1.03, "Plan Rows": 3, "Plans": [{"Node Type": "Index Scan", "Relation Name": "customers", "Alias": "c", "Index Name": "customers_pkey", "Total Cost": 1.0, "Plan Rows": 3}]}
            ]}, "Planning Time": 0.12, "Execution Time": 4.3}]"#;
        let p = parse_pg_json(json).unwrap();
        assert!(p.analyzed);
        assert_eq!(p.execution_ms, Some(4.3));
        let root = &p.roots[0];
        assert_eq!(root.op, "Hash Join (Inner)");
        assert_eq!(root.children[0].relation.as_deref(), Some("orders o"));
        assert_eq!(root.children[1].children[0].index.as_deref(), Some("customers_pkey"));
        assert!(p.hints.iter().any(|h| h.contains("index on (total)")), "{:?}", p.hints);
        assert!(p.hints.iter().any(|h| h.contains("expected ~2 rows but got 5000")), "{:?}", p.hints);
    }

    #[test]
    fn postgres_text_and_twinlite_plans() {
        let text = "Hash Join  (cost=1.07..2.30 rows=1 width=8) (actual time=0.031..0.040 rows=3 loops=1)\n  Hash Cond: (o.customer_id = c.id)\n  ->  Seq Scan on orders o  (cost=0.00..1.03 rows=3 width=8) (actual time=0.010..0.012 rows=3 loops=1)\n        Filter: (total > '100'::numeric)\n  ->  Hash  (cost=1.03..1.03 rows=3 width=4)\n        ->  Seq Scan on customers c  (cost=0.00..1.03 rows=3 width=4)\nPlanning Time: 0.08 ms\nExecution Time: 0.06 ms";
        let p = parse_pg_text(text);
        assert_eq!(p.roots.len(), 1);
        let root = &p.roots[0];
        assert_eq!(root.op, "Hash Join");
        assert_eq!(root.details, vec!["Hash Cond: (o.customer_id = c.id)"]);
        assert_eq!(root.est_cost, Some(2.30));
        assert_eq!(root.actual_rows, Some(3.0));
        assert_eq!(root.children.len(), 2);
        assert_eq!(root.children[0].relation.as_deref(), Some("orders"));
        assert_eq!(root.children[0].details, vec!["Filter: (total > '100'::numeric)"]);
        assert_eq!(root.children[1].children[0].relation.as_deref(), Some("customers"));
        assert_eq!(p.execution_ms, Some(0.06));
        // TwinLite prints SQLite steps in the same indented shape.
        let lite = parse_pg_text("SCAN o\nSEARCH c USING INTEGER PRIMARY KEY (rowid=?)\nUSE TEMP B-TREE FOR ORDER BY");
        assert_eq!(lite.roots.len(), 3);
        assert_eq!(lite.roots[0].relation.as_deref(), Some("o"));
        assert!(lite.hints.iter().any(|h| h.contains("temporary B-tree")));
    }

    #[test]
    fn mysql_tree_and_table_plans() {
        let tree = "-> Nested loop inner join  (cost=1.10 rows=3) (actual time=0.05..0.08 rows=3 loops=1)\n    -> Filter: (o.total > 100)  (cost=0.55 rows=1)\n        -> Table scan on o  (cost=0.55 rows=3)\n    -> Single-row index lookup on c using PRIMARY (id=o.customer_id)  (cost=0.12 rows=1)";
        let p = parse_mysql_tree(tree);
        let root = &p.roots[0];
        assert_eq!(root.op, "Nested loop inner join");
        assert_eq!(root.actual_rows, Some(3.0));
        assert_eq!(root.children[0].children[0].relation.as_deref(), Some("o"));
        assert_eq!(root.children[1].index.as_deref(), Some("PRIMARY"));
        let cols: Vec<String> =
            ["id", "select_type", "table", "type", "possible_keys", "key", "rows", "Extra"].iter().map(|s| s.to_string()).collect();
        let rows = vec![vec![
            Value::from("1"),
            Value::from("SIMPLE"),
            Value::from("orders"),
            Value::from("ALL"),
            Value::Null,
            Value::Null,
            Value::from("50000"),
            Value::from("Using where; Using filesort"),
        ]];
        let t = parse_mysql_table(&cols, &rows);
        assert_eq!(t.roots[0].op, "Full table scan");
        assert!(t.hints.iter().any(|h| h.contains("filesort")));
    }

    #[test]
    fn sqlite_query_plans_build_a_tree() {
        let p = parse_sqlite(&[
            (2, 0, "SCAN users".into()),
            (5, 0, "SEARCH orders USING INDEX orders_user_idx (user_id=?)".into()),
            (9, 5, "LIST SUBQUERY 1".into()),
        ]);
        assert_eq!(p.roots.len(), 2);
        assert_eq!(p.roots[1].index.as_deref(), Some("orders_user_idx"));
        assert_eq!(p.roots[1].children[0].op, "LIST SUBQUERY 1");
    }
}
