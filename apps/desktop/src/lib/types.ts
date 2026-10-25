// Shapes returned by the TwinBase core API (crates/core/src/api.rs).

export type DbKind = "postgres" | "mysql" | "sqlite" | "mssql" | "clickhouse" | "cassandra" | "spanner" | "bigquery" | "oracle" | "elasticsearch" | "redis" | "mongodb" | "dynamodb" | "firestore";
export type MockKind = "postgres" | "mysql" | "mssql" | "redis" | "mongodb" | "dynamodb" | "firestore";
export type SqlDialect = "postgres" | "mysql" | "sqlite" | "mssql" | "clickhouse" | "cassandra" | "spanner" | "bigquery" | "oracle";
export type TlsMode = "disable" | "require" | "verify-full";
export type AiAccess = "none" | "read-only" | "read-write";
export type Cell = null | boolean | number | string;

export interface ConnectionDef {
  id: string;
  name: string;
  kind: DbKind;
  host: string;
  port: number;
  user: string;
  database?: string;
  path?: string;
  tls: TlsMode;
  color?: string;
  read_only: boolean;
  ai_access: AiAccess;
  group?: string;
  has_password?: boolean;
  ssh?: SshTunnel;
  has_ssh_secret?: boolean;
}

export interface SshTunnel {
  host: string;
  port: number;
  user: string;
  auth: "password" | "key" | "agent";
  key_path?: string;
}

export type AuthMode = { mode: "any" } | { mode: "password"; user: string; password: string };

export type FaultAction =
  | { type: "delay"; ms: number; jitter_ms?: number }
  | { type: "error"; code?: string; message?: string }
  | { type: "drop" };

export interface FaultRule {
  enabled: boolean;
  name?: string;
  matches: string;
  probability: number;
  action: FaultAction;
}

export interface FaultConfig {
  enabled: boolean;
  refuse_connections: boolean;
  rules: FaultRule[];
}

export interface MockDef {
  id: string;
  name: string;
  kind: MockKind;
  host: string;
  port: number;
  auth: AuthMode;
  database: string;
  persist: boolean;
  auto_start: boolean;
  description?: string;
  faults?: FaultConfig;
  /** PostgreSQL mocks: TwinLite (built in) or a real PostgreSQL server. */
  engine?: "twinlite" | "native";
}

export interface MockStatus extends MockDef {
  running: boolean;
  connection_id: string;
  error?: string;
}

export type Expectation =
  | { kind: "rows"; count: number }
  | { kind: "empty" }
  | { kind: "succeeds" }
  | { kind: "fails"; code?: string };

export interface SavedQuery {
  id: string;
  name: string;
  collection: string;
  connection?: string;
  tags: string[];
  docs: string;
  sql: string;
  expect?: Expectation;
}

export interface Collection {
  id: string;
  name: string;
  docs: string;
}

export interface WorkspaceInfo {
  name: string;
  path: string;
  connections: ConnectionDef[];
  mocks: MockStatus[];
  collections: Collection[];
  queries: SavedQuery[];
  connected: string[];
}

export interface Settings {
  theme: "system" | "light" | "dark";
  font_size: number;
  font_family: string;
  mono_font: string;
  row_limit: number;
  page_size: number;
  recent_workspaces: string[];
  last_workspace?: string;
  mcp: { enabled: boolean; port: number; token: string };
  confirm_destructive: boolean;
  /** Bin folder of a PostgreSQL installation for native-engine mocks (empty: detect). */
  postgres_bin: string;
  /** Oracle Instant Client folder (empty: detect). */
  oracle_client: string;
}

export interface ResultColumn {
  name: string;
  type_name: string;
}

export interface QueryResult {
  statement: string;
  columns: ResultColumn[];
  rows: Cell[][];
  affected?: number;
  truncated: boolean;
  elapsed_ms: number;
  message?: string;
}

export interface TableInfo {
  schema: string;
  name: string;
  kind: string;
}

export interface ColumnInfo {
  name: string;
  data_type: string;
  nullable: boolean;
  default?: string;
  primary_key: boolean;
  unique: boolean;
  auto: boolean;
}

export interface ForeignKeyInfo {
  name: string;
  columns: string[];
  ref_schema: string;
  ref_table: string;
  ref_columns: string[];
  on_delete?: string;
}

export interface IndexInfo {
  name: string;
  columns: string[];
  unique: boolean;
  primary: boolean;
}

export interface TableSchema {
  schema: string;
  name: string;
  kind: string;
  columns: ColumnInfo[];
  primary_key: string[];
  foreign_keys: ForeignKeyInfo[];
  indexes: IndexInfo[];
}

export interface RowsPage {
  result: QueryResult;
  total: number | null;
  /** Counting stopped early: at least this many rows match. */
  total_at_least?: number;
  key_columns: string[];
  row_id_column: string | null;
}

export type FilterOp =
  | "eq" | "ne" | "lt" | "le" | "gt" | "ge"
  | "contains" | "starts_with" | "ends_with" | "is_null" | "is_not_null";

export interface Filter {
  column: string;
  op: FilterOp;
  value?: Cell;
}

export interface SortKey {
  column: string;
  descending?: boolean;
}

export interface RowQuery {
  offset: number;
  limit: number;
  sort: SortKey[];
  filters: Filter[];
  where_sql?: string;
}

export type Row = Record<string, Cell>;

export interface ChangeSet {
  updates: { key: Row; values: Row }[];
  inserts: Row[];
  deletes: Row[];
}

export interface ChangedRow {
  key: Cell[];
  before: Cell[];
  after: Cell[];
  changed: number[];
}

export interface TableDiff {
  table: string;
  columns: string[];
  key_columns: string[];
  added: Cell[][];
  removed: Cell[][];
  changed: ChangedRow[];
  sampled: boolean;
}

export interface PreviewResult {
  statements: { sql: string; affected?: number; error?: string }[];
  tables: TableDiff[];
  notes: string[];
  expected_affected?: number;
  ok: boolean;
}

export interface RedisKey {
  key: string;
  type: string;
  ttl_ms: number;
}

export type RedisValue =
  | { type: "string"; value: string }
  | { type: "list"; items: string[] }
  | { type: "hash"; fields: [string, string][] }
  | { type: "set"; members: string[] }
  | { type: "zset"; members: [string, number][] }
  | { type: "none" };

export interface Snippet {
  id: string;
  label: string;
  language: string;
  code: string;
}

export type Actor = { kind: "user" } | { kind: "agent"; name: string } | { kind: "cli" };

export interface Activity {
  id: number;
  at: number;
  actor: Actor;
  action: string;
  summary: string;
  connection?: string;
  sql?: string;
  ok: boolean;
  error?: string;
  duration_ms: number;
}

export type TrafficEvent = { id: number; at: number; connection: number } & (
  | { kind: "connect"; peer: string; user?: string; database?: string }
  | { kind: "disconnect" }
  | { kind: "query"; text: string; duration_us: number; rows: number; error?: string }
);

export interface ScenarioResult {
  id: string;
  name: string;
  passed: boolean;
  expectation?: Expectation;
  actual: string;
  error?: string;
  elapsed_ms: number;
}

export interface HistoryEntry {
  at: number;
  sql: string;
  ok: boolean;
  elapsed_ms: number;
  error?: string;
}

export interface AgentSetup {
  id: string;
  name: string;
  kind: "command" | "json" | "toml" | "text";
  steps: string;
  config: string;
}

export interface McpConfig {
  url: string;
  token: string;
  cli: string;
  enabled: boolean;
  agents: AgentSetup[];
  note: string;
}

export type CoreEvent =
  | ({ type: "activity" } & Activity)
  | { type: "mock_status"; id: string; running: boolean; port: number; error?: string }
  | { type: "mock_traffic"; id: string; event: TrafficEvent }
  | { type: "connection_status"; id: string; connected: boolean }
  | { type: "schema_changed"; connection: string }
  | { type: "data_changed"; connection: string; table?: string }
  | { type: "workspace_changed" }
  | { type: "mcp_client"; name: string; connected: boolean };

export interface SnapshotInfo {
  id: string;
  label: string;
  taken_at: string;
  connection: string;
  source: string;
  dialect: SqlDialect;
  schema: string;
  tables: number;
}

export type SchemaSource = { snapshot: string } | { connection: string; schema?: string };

export type SchemaChange =
  | { kind: "table_added"; table: string }
  | { kind: "table_removed"; table: string }
  | { kind: "column_added"; table: string; column: string; data_type: string }
  | { kind: "column_removed"; table: string; column: string }
  | { kind: "column_changed"; table: string; column: string; details: string[] }
  | { kind: "primary_key_changed"; table: string; from: string[]; to: string[] }
  | { kind: "foreign_key_added"; table: string; columns: string[]; ref_table: string }
  | { kind: "foreign_key_removed"; table: string; columns: string[]; ref_table: string }
  | { kind: "index_added"; table: string; name: string; columns: string[]; unique: boolean }
  | { kind: "index_removed"; table: string; name: string; columns: string[]; unique: boolean };

export interface SchemaDiff {
  dialect: SqlDialect;
  changes: SchemaChange[];
  warnings: string[];
  identical: boolean;
  up_sql: string;
  down_sql: string;
  from_label: string;
  to_label: string;
}

export interface PlanNode {
  op: string;
  relation?: string;
  index?: string;
  details: string[];
  est_rows?: number;
  est_cost?: number;
  actual_rows?: number;
  actual_ms?: number;
  loops?: number;
  children: PlanNode[];
}

export interface QueryPlan {
  format: string;
  analyzed: boolean;
  roots: PlanNode[];
  planning_ms?: number;
  execution_ms?: number;
  hints: string[];
  raw: string;
  statement: string;
}

export interface ApiError {
  kind: string;
  message: string;
  code?: string;
  detail?: string;
  statement_index?: number;
}

// ------------------------------------------------------------------ MongoDB

/** A document as relaxed Extended JSON ({"$oid": …}, {"$date": …}, …). */
export type MongoDoc = Record<string, unknown>;

export interface MongoCollection {
  name: string;
  type: string;
  count: number | null;
}

export interface MongoDatabase {
  name: string;
  size_bytes: number;
  empty: boolean;
}

export interface MongoPage {
  documents: MongoDoc[];
  total: number | null;
  duration_ms: number;
}

export interface MongoOutput {
  statement: string;
  database: string;
  documents?: MongoDoc[];
  value?: unknown;
  message?: string;
  truncated: boolean;
  duration_ms: number;
  error?: { message: string; code?: string };
}

export interface MongoField {
  path: string;
  types: [string, number][];
  present: number;
}
