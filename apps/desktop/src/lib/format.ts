import type { Cell, DbKind } from "./types";

export function cellText(v: Cell): string {
  if (v === null) return "NULL";
  if (typeof v === "boolean") return v ? "true" : "false";
  return String(v);
}

/** Single-line preview for grid cells (newlines shown as ↵, long values cut). */
export function cellPreview(v: Cell): string {
  const s = cellText(v);
  const flat = s.length > 400 ? `${s.slice(0, 400)}…` : s;
  return flat.replace(/\r?\n/g, " ↵ ");
}

export type CellClass = "num" | "null" | "bool" | "json" | "text";

export function cellClass(v: Cell, typeName?: string): CellClass {
  if (v === null) return "null";
  if (typeof v === "number") return "num";
  if (typeof v === "boolean") return "bool";
  const t = (typeName ?? "").toLowerCase();
  if (/^(int|bigint|smallint|numeric|decimal|float|double|real|serial|bigserial|tinyint|mediumint)/.test(t) || /int[248]$|float[48]$/.test(t)) return "num";
  if (t.includes("json")) return "json";
  return "text";
}

/** Pretty-print JSON text when it parses; otherwise return as-is. */
export function prettyValue(v: Cell): string {
  if (typeof v === "string") {
    const t = v.trim();
    if ((t.startsWith("{") && t.endsWith("}")) || (t.startsWith("[") && t.endsWith("]"))) {
      try {
        return JSON.stringify(JSON.parse(t), null, 2);
      } catch {
        /* not JSON */
      }
    }
  }
  return cellText(v);
}

export function timeOfDay(ms: number): string {
  return new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false });
}

export function relativeTime(ms: number): string {
  const d = (Date.now() - ms) / 1000;
  if (d < 5) return "just now";
  if (d < 60) return `${Math.floor(d)}s ago`;
  if (d < 3600) return `${Math.floor(d / 60)}m ago`;
  if (d < 86400) return `${Math.floor(d / 3600)}h ago`;
  return new Date(ms).toLocaleDateString();
}

export function duration(ms: number): string {
  if (ms < 1) return `${(ms * 1000).toFixed(0)} µs`;
  if (ms < 1000) return `${ms.toFixed(ms < 10 ? 1 : 0)} ms`;
  return `${(ms / 1000).toFixed(2)} s`;
}

export function plural(n: number, word: string, many = `${word}s`): string {
  return `${n.toLocaleString()} ${n === 1 ? word : many}`;
}

export function kindLabel(kind: DbKind): string {
  return { postgres: "PostgreSQL", mysql: "MySQL", sqlite: "SQLite", mssql: "SQL Server", clickhouse: "ClickHouse", cassandra: "Cassandra", spanner: "Spanner", bigquery: "BigQuery", oracle: "Oracle", elasticsearch: "Elasticsearch", redis: "Redis", mongodb: "MongoDB", dynamodb: "DynamoDB", firestore: "Firestore" }[kind];
}

export function kindClass(kind: DbKind): string {
  return { postgres: "kind-pg", mysql: "kind-mysql", sqlite: "kind-sqlite", mssql: "kind-mssql", clickhouse: "kind-clickhouse", cassandra: "kind-cassandra", spanner: "kind-spanner", bigquery: "kind-bigquery", oracle: "kind-oracle", elasticsearch: "kind-es", redis: "kind-redis", mongodb: "kind-mongo", dynamodb: "kind-dynamo", firestore: "kind-firestore" }[kind];
}

export function quoteIdent(kind: DbKind, name: string): string {
  if (kind === "mysql") return "`" + name.replace(/`/g, "``") + "`";
  if (kind === "spanner" || kind === "bigquery") return /^[A-Za-z_][A-Za-z0-9_]*$/.test(name) ? name : "`" + name.replace(/\\/g, "\\\\").replace(/`/g, "\\`") + "`";
  if (kind === "clickhouse") return /^[A-Za-z_][A-Za-z0-9_]*$/.test(name) ? name : "`" + name.replace(/\\/g, "\\\\").replace(/`/g, "\\`") + "`";
  if (kind === "mssql") return `[${name.replace(/]/g, "]]")}]`;
  return /^[a-z_][a-z0-9_]*$/.test(name) ? name : `"${name.replace(/"/g, '""')}"`;
}

export function qualifiedName(kind: DbKind, schema: string, table: string): string {
  const defaultSchema = kind === "postgres" ? "public" : kind === "sqlite" ? "main" : kind === "mssql" ? "dbo" : "";
  return !schema || schema === defaultSchema ? quoteIdent(kind, table) : `${quoteIdent(kind, schema)}.${quoteIdent(kind, table)}`;
}

export function sqlLiteral(v: Cell): string {
  if (v === null) return "NULL";
  if (typeof v === "number") return String(v);
  if (typeof v === "boolean") return v ? "TRUE" : "FALSE";
  return `'${v.replace(/'/g, "''")}'`;
}

/** Statements that rewrite or drop data without a filter: worth a confirmation or a preview. */
export function riskyStatement(sql: string): string | undefined {
  const stripped = sql.replace(/--[^\n]*|\/\*[\s\S]*?\*\//g, " ").replace(/'(?:[^']|'')*'/g, "''");
  for (const raw of stripped.split(";")) {
    const s = raw.trim().replace(/\s+/g, " ").toUpperCase();
    if (!s) continue;
    if (/^(DROP|TRUNCATE)\b/.test(s)) return `${s.split(" ").slice(0, 3).join(" ")} removes data permanently.`;
    if (/^(DELETE FROM|UPDATE)\b/.test(s) && !/\bWHERE\b/.test(s)) return `${s.startsWith("DELETE") ? "DELETE" : "UPDATE"} without WHERE changes every row.`;
  }
  return undefined;
}

/** True when the SQL only reads data (for enabling "Preview"). */
export function isReadOnlySql(sql: string): boolean {
  const s = sql.replace(/--[^\n]*|\/\*[\s\S]*?\*\//g, " ").trim().toUpperCase();
  // Statements end at ";" or at a SQL Server "GO" line. SELECT … INTO creates a table.
  return s.split(/;|^[ \t]*GO[ \t]*\d*[ \t]*$/m).every((p) => {
    const t = p.trim();
    return !t || (/^(SELECT|WITH|SHOW|EXPLAIN|DESCRIBE|DESC|VALUES|TABLE)\b/.test(t) && !/^WITH\b[\s\S]*\b(INSERT|UPDATE|DELETE|MERGE)\b/.test(t) && !/^(SELECT|WITH)\b[\s\S]*\bINTO\b/.test(t));
  });
}

export async function copyText(text: string): Promise<void> {
  await navigator.clipboard.writeText(text);
}

/** Kinds whose query tabs are consoles in their own language rather than SQL. */
export const CONSOLE_KINDS: readonly DbKind[] = ["mongodb", "dynamodb", "firestore", "elasticsearch"];

/** What a new query tab on a connection starts with. */
export function consoleStart(kind: DbKind): string {
  return { mongodb: "show collections", dynamodb: "ListTables {}", firestore: "collections", elasticsearch: "GET /_cat/indices" }[kind as string] ?? "";
}
