// SQL generation for the table designer and schema templates.

import { quoteIdent } from "./format";
import type { DbKind, TableSchema } from "./types";

export type SqlKind = Exclude<DbKind, "redis" | "mongodb" | "dynamodb" | "firestore" | "elasticsearch">;

export interface DesignColumn {
  /** Name in the database before editing (undefined for new columns). */
  original?: string;
  name: string;
  type: string;
  nullable: boolean;
  primaryKey: boolean;
  unique: boolean;
  auto: boolean;
  default: string;
  /** Referenced `table.column`, or "". */
  references: string;
  onDelete: "" | "CASCADE" | "SET NULL" | "RESTRICT";
}

export interface DesignTable {
  original?: string;
  name: string;
  columns: DesignColumn[];
}

export const TYPE_SUGGESTIONS: Record<SqlKind, string[]> = {
  postgres: ["integer", "bigint", "smallint", "serial", "bigserial", "numeric(10,2)", "real", "double precision", "boolean", "text", "varchar(255)", "char(2)", "uuid", "date", "time", "timestamp", "timestamptz", "interval", "jsonb", "json", "bytea", "text[]", "integer[]", "vector(1536)", "halfvec(1536)", "sparsevec(30000)"],
  mysql: ["int", "bigint", "smallint", "tinyint", "decimal(10,2)", "double", "float", "boolean", "varchar(255)", "text", "longtext", "char(36)", "date", "datetime", "timestamp", "time", "json", "blob", "enum('a','b')"],
  sqlite: ["integer", "real", "text", "blob", "numeric", "boolean", "datetime"],
  oracle: ["NUMBER(10)", "NUMBER(19)", "NUMBER(12,2)", "VARCHAR2(255 CHAR)", "VARCHAR2(4000 CHAR)", "CHAR(1)", "CLOB", "BLOB", "DATE", "TIMESTAMP", "TIMESTAMP WITH TIME ZONE", "BINARY_DOUBLE", "RAW(16)"],
  spanner: ["INT64", "STRING(MAX)", "STRING(255)", "NUMERIC", "FLOAT64", "BOOL", "DATE", "TIMESTAMP", "BYTES(MAX)", "JSON", "ARRAY<STRING(MAX)>"],
  bigquery: ["INT64", "STRING", "NUMERIC", "BIGNUMERIC", "FLOAT64", "BOOL", "DATE", "DATETIME", "TIMESTAMP", "BYTES", "JSON", "GEOGRAPHY", "ARRAY<STRING>"],
  cassandra: ["uuid", "timeuuid", "text", "int", "bigint", "decimal", "double", "boolean", "timestamp", "date", "time", "blob", "inet", "list<text>", "set<text>", "map<text, text>", "counter"],
  clickhouse: ["UInt64", "Int64", "Int32", "UInt8", "Float64", "Decimal(10, 2)", "String", "LowCardinality(String)", "FixedString(2)", "Bool", "Date", "DateTime", "DateTime64(3)", "UUID", "Array(String)", "Map(String, String)", "JSON", "IPv4"],
  mssql: ["int", "bigint", "smallint", "tinyint", "bit", "decimal(10,2)", "money", "float", "real", "nvarchar(255)", "nvarchar(max)", "varchar(255)", "nchar(10)", "uniqueidentifier", "date", "time", "datetime2", "datetimeoffset", "varbinary(max)", "xml"],
};

export function newColumn(kind: SqlKind, partial: Partial<DesignColumn> = {}): DesignColumn {
  return { name: "", type: kind === "mysql" ? "varchar(255)" : kind === "mssql" ? "nvarchar(255)" : kind === "clickhouse" ? "String" : kind === "spanner" ? "STRING(MAX)" : kind === "bigquery" ? "STRING" : kind === "oracle" ? "VARCHAR2(255 CHAR)" : "text", nullable: true, primaryKey: false, unique: false, auto: false, default: "", references: "", onDelete: "", ...partial };
}

export function idColumn(kind: SqlKind): DesignColumn {
  if (kind === "clickhouse") return newColumn(kind, { name: "id", type: "UInt64", nullable: false, primaryKey: true });
  if (kind === "cassandra") return newColumn(kind, { name: "id", type: "uuid", nullable: false, primaryKey: true });
  if (kind === "spanner" || kind === "bigquery") return newColumn(kind, { name: "id", type: "INT64", nullable: false, primaryKey: true });
  return newColumn(kind, { name: "id", type: kind === "postgres" ? "integer" : kind === "mysql" || kind === "mssql" ? "int" : kind === "oracle" ? "NUMBER(10)" : "integer", nullable: false, primaryKey: true, auto: true });
}

function columnType(kind: SqlKind, c: DesignColumn): string {
  if (c.auto && kind === "postgres") {
    if (/^(bigint|int8|bigserial)$/i.test(c.type)) return "bigserial";
    if (/^(smallint|int2)$/i.test(c.type)) return "smallserial";
    if (/^(int|integer|int4|serial)$/i.test(c.type)) return "serial";
  }
  return c.type.trim() || "text";
}

function defaultExpr(v: string, kind?: SqlKind): string {
  const t = v.trim();
  if (!t) return "";
  if (kind === "clickhouse" && /^(now\(\)|current_timestamp)$/i.test(t)) return "now()";
  if ((kind === "spanner" || kind === "bigquery") && /^(now\(\)|current_timestamp)$/i.test(t)) return "CURRENT_TIMESTAMP()";
  if (kind === "mssql" || kind === "oracle") {
    if (/^true$/i.test(t)) return "1";
    if (/^false$/i.test(t)) return "0";
    if (/^now\(\)$/i.test(t)) return "SYSUTCDATETIME()";
  }
  // Bare words that are not functions/keywords/numbers become string literals.
  if (/^-?\d+(\.\d+)?$/.test(t) || /^(true|false|null|current_timestamp|current_date|now\(\)|.*\(.*\)|'.*')$/i.test(t)) return t;
  return `'${t.replace(/'/g, "''")}'`;
}

function referenceClause(kind: SqlKind, c: DesignColumn): string {
  if (!c.references.includes(".")) return "";
  const [t, col] = c.references.split(".");
  return ` REFERENCES ${quoteIdent(kind, t)}(${quoteIdent(kind, col)})${c.onDelete ? ` ON DELETE ${c.onDelete}` : ""}`;
}

/** ClickHouse: nullability is part of the type; no column constraints. */
function clickhouseColumn(c: DesignColumn): string {
  const ty = columnType("clickhouse", c);
  let s = `${quoteIdent("clickhouse", c.name)} ${c.nullable && !c.primaryKey && !/^Nullable\(/.test(ty) ? `Nullable(${ty})` : ty}`;
  const d = defaultExpr(c.default, "clickhouse");
  if (d) s += ` DEFAULT ${d}`;
  return s;
}

export function columnDefinition(kind: SqlKind, c: DesignColumn, inlinePk: boolean): string {
  if (kind === "clickhouse") return clickhouseColumn(c);
  // CQL columns are a name and a type: no constraints or defaults.
  if (kind === "cassandra") return `${quoteIdent(kind, c.name)} ${columnType(kind, c)}`;
  // GoogleSQL: NOT NULL and defaults only; keys are declared after the columns.
  if (kind === "spanner" || kind === "bigquery") {
    const d = defaultExpr(c.default, kind);
    return `${quoteIdent(kind, c.name)} ${columnType(kind, c)}${c.nullable ? "" : " NOT NULL"}${d ? (kind === "spanner" ? ` DEFAULT (${d})` : ` DEFAULT ${d}`) : ""}`;
  }
  let s = `${quoteIdent(kind, c.name)} ${columnType(kind, c)}`;
  if (c.auto && kind === "mssql") s += " IDENTITY(1,1)";
  if (c.auto && kind === "oracle") s += " GENERATED BY DEFAULT AS IDENTITY";
  // Oracle wants DEFAULT before constraints.
  const oracleDefault = kind === "oracle" && !c.auto ? defaultExpr(c.default, kind) : "";
  if (oracleDefault) s += ` DEFAULT ${oracleDefault}`;
  if (inlinePk && c.primaryKey) s += kind === "sqlite" && c.auto ? " PRIMARY KEY AUTOINCREMENT" : " PRIMARY KEY";
  if (c.auto && kind === "mysql") s += " AUTO_INCREMENT";
  if (!c.nullable && !(inlinePk && c.primaryKey)) s += " NOT NULL";
  const d = kind === "oracle" ? "" : defaultExpr(c.default, kind);
  if (d && !c.auto) s += ` DEFAULT ${d}`;
  if (c.unique && !c.primaryKey) s += " UNIQUE";
  s += referenceClause(kind, c);
  return s;
}

export function createTableSql(kind: SqlKind, t: DesignTable): string {
  const cols = t.columns.filter((c) => c.name.trim());
  const pks = cols.filter((c) => c.primaryKey);
  if (kind === "spanner" || kind === "bigquery") {
    const key = pks.length ? pks : kind === "spanner" ? cols.slice(0, 1) : [];
    const keyList = key.map((c) => quoteIdent(kind, c.name)).join(", ");
    const lines = cols.map((c) => `  ${columnDefinition(kind, c, false)}`);
    if (kind === "bigquery" && key.length) lines.push(`  PRIMARY KEY (${keyList}) NOT ENFORCED`);
    return `CREATE TABLE ${quoteIdent(kind, t.name)} (\n${lines.join(",\n")}\n)${kind === "spanner" ? ` PRIMARY KEY (${keyList})` : ""};`;
  }
  if (kind === "cassandra") {
    const key = pks.length ? pks : cols.slice(0, 1);
    const lines = [...cols.map((c) => `  ${columnDefinition(kind, c, false)}`), `  PRIMARY KEY (${key.map((c) => quoteIdent(kind, c.name)).join(", ")})`];
    return `CREATE TABLE ${quoteIdent(kind, t.name)} (\n${lines.join(",\n")}\n);`;
  }
  if (kind === "clickhouse") {
    const order = pks.length ? `(${pks.map((c) => quoteIdent(kind, c.name)).join(", ")})` : "tuple()";
    return `CREATE TABLE ${quoteIdent(kind, t.name)} (\n${cols.map((c) => `  ${clickhouseColumn(c)}`).join(",\n")}\n) ENGINE = MergeTree ORDER BY ${order};`;
  }
  const inline = pks.length === 1;
  const lines = cols.map((c) => `  ${columnDefinition(kind, c, inline)}`);
  if (pks.length > 1) lines.push(`  PRIMARY KEY (${pks.map((c) => quoteIdent(kind, c.name)).join(", ")})`);
  return `CREATE TABLE ${quoteIdent(kind, t.name)} (\n${lines.join(",\n")}\n);`;
}

/** Turn an existing table into designer state. */
export function fromSchema(kind: SqlKind, s: TableSchema): DesignTable {
  return {
    original: s.name,
    name: s.name,
    columns: s.columns.map((c) => {
      const fk = s.foreign_keys.find((f) => f.columns.length === 1 && f.columns[0] === c.name);
      const def = c.default && !c.auto ? c.default.replace(/::[a-z ]+$/i, "") : "";
      return {
        original: c.name,
        name: c.name,
        type: kind === "postgres" && c.auto ? c.data_type.replace(/^(serial|int4)$/, "integer").replace(/^(bigserial|int8)$/, "bigint") : c.data_type,
        nullable: c.nullable,
        primaryKey: c.primary_key,
        unique: c.unique && !c.primary_key,
        auto: c.auto,
        default: def,
        references: fk ? `${fk.ref_table}.${fk.ref_columns[0]}` : "",
        onDelete: ((fk?.on_delete ?? "").toUpperCase() as DesignColumn["onDelete"]) || "",
      };
    }),
  };
}

export interface AlterPlan {
  statements: string[];
  notes: string[];
}

/** SQL Server: drop a column's default constraint, whatever name the server gave it. */
function mssqlDropDefault(table: string, column: string): string {
  const lit = (s: string) => s.replace(/'/g, "''");
  const inner = `DECLARE @n sysname = (SELECT d.name FROM sys.default_constraints d JOIN sys.columns c ON c.default_object_id = d.object_id WHERE d.parent_object_id = OBJECT_ID(N'${lit(table)}') AND c.name = N'${lit(column)}'); IF @n IS NOT NULL EXEC(N'ALTER TABLE ${table} DROP CONSTRAINT [' + @n + N']');`;
  return `EXEC(N'${lit(inner)}');`;
}

/** ALTER statements that turn `before` into `after`. */
export function alterTableSql(kind: SqlKind, before: DesignTable, after: DesignTable): AlterPlan {
  const out: string[] = [];
  const notes: string[] = [];
  const tableName = before.original ?? before.name;
  const t = quoteIdent(kind, tableName);
  const q = (n: string) => quoteIdent(kind, n);
  const afterCols = after.columns.filter((c) => c.name.trim());
  const kept = new Set(afterCols.map((c) => c.original).filter(Boolean));

  for (const c of before.columns) {
    if (kept.has(c.name)) continue;
    if (kind === "cassandra") {
      out.push(`ALTER TABLE ${t} DROP ${q(c.name)};`);
      continue;
    }
    if (kind === "mssql" && c.default.trim()) out.push(mssqlDropDefault(t, c.name));
    out.push(`ALTER TABLE ${t} DROP COLUMN ${q(c.name)};`);
  }

  for (const c of afterCols) {
    if (!c.original) {
      if (c.primaryKey) notes.push(`New column "${c.name}" is marked primary key; primary keys of existing tables are not changed by the designer.`);
      out.push(`ALTER TABLE ${t} ${kind === "mssql" || kind === "cassandra" || kind === "oracle" ? "ADD" : "ADD COLUMN"} ${columnDefinition(kind, { ...c, primaryKey: false }, false)};`);
      continue;
    }
    const old = before.columns.find((x) => x.name === c.original)!;
    let name = old.name;
    if (kind === "cassandra") {
      // CQL renames only primary-key columns and cannot change types in place.
      if (c.name !== old.name) {
        if (old.primaryKey) out.push(`ALTER TABLE ${t} RENAME ${q(old.name)} TO ${q(c.name)};`);
        else notes.push(`Cassandra can only rename primary-key columns ("${old.name}").`);
      }
      if (c.type.trim().toLowerCase() !== old.type.trim().toLowerCase()) notes.push(`Cassandra cannot change the type of "${old.name}" in place.`);
      continue;
    }
    if (c.name !== old.name) {
      out.push(
        kind === "mssql"
          ? `EXEC sp_rename N'${`${t}.${q(old.name)}`.replace(/'/g, "''")}', N'${c.name.replace(/'/g, "''")}', N'COLUMN';`
          : `ALTER TABLE ${t} RENAME COLUMN ${q(old.name)} TO ${q(c.name)};`,
      );
      name = c.name;
    }
    const typeChanged = c.type.trim().toLowerCase() !== old.type.trim().toLowerCase();
    const nullChanged = c.nullable !== old.nullable;
    const defChanged = c.default.trim() !== old.default.trim();
    if (kind === "sqlite") {
      if (typeChanged || nullChanged || defChanged) notes.push(`SQLite cannot change the type, nullability or default of "${name}" in place; recreate the table with SQL instead.`);
    } else if (kind === "oracle") {
      if (typeChanged) out.push(`ALTER TABLE ${t} MODIFY (${q(name)} ${c.type.trim()});`);
      if (defChanged) out.push(`ALTER TABLE ${t} MODIFY (${q(name)} DEFAULT ${c.default.trim() ? defaultExpr(c.default, kind) : "NULL"});`);
      if (nullChanged) out.push(`ALTER TABLE ${t} MODIFY (${q(name)} ${c.nullable ? "NULL" : "NOT NULL"});`);
    } else if (kind === "spanner") {
      if (typeChanged || nullChanged) out.push(`ALTER TABLE ${t} ALTER COLUMN ${q(name)} ${c.type.trim()}${c.nullable ? "" : " NOT NULL"};`);
      if (defChanged) out.push(c.default.trim() ? `ALTER TABLE ${t} ALTER COLUMN ${q(name)} SET DEFAULT (${defaultExpr(c.default, kind)});` : `ALTER TABLE ${t} ALTER COLUMN ${q(name)} DROP DEFAULT;`);
    } else if (kind === "bigquery") {
      if (typeChanged) out.push(`ALTER TABLE ${t} ALTER COLUMN ${q(name)} SET DATA TYPE ${c.type.trim()};`);
      if (nullChanged && c.nullable) out.push(`ALTER TABLE ${t} ALTER COLUMN ${q(name)} DROP NOT NULL;`);
      if (nullChanged && !c.nullable) notes.push(`BigQuery cannot make "${name}" required after the fact.`);
      if (defChanged) out.push(c.default.trim() ? `ALTER TABLE ${t} ALTER COLUMN ${q(name)} SET DEFAULT ${defaultExpr(c.default, kind)};` : `ALTER TABLE ${t} ALTER COLUMN ${q(name)} DROP DEFAULT;`);
    } else if (kind === "clickhouse") {
      if (typeChanged || nullChanged || (defChanged && c.default.trim())) out.push(`ALTER TABLE ${t} MODIFY COLUMN ${clickhouseColumn({ ...c, name })};`);
      if (defChanged && !c.default.trim()) out.push(`ALTER TABLE ${t} MODIFY COLUMN ${q(name)} REMOVE DEFAULT;`);
    } else if (kind === "mssql") {
      if (typeChanged || nullChanged) out.push(`ALTER TABLE ${t} ALTER COLUMN ${q(name)} ${c.type.trim()} ${c.nullable ? "NULL" : "NOT NULL"};`);
      if (defChanged) {
        if (old.default.trim()) out.push(mssqlDropDefault(t, name));
        if (c.default.trim()) out.push(`ALTER TABLE ${t} ADD DEFAULT ${defaultExpr(c.default, kind)} FOR ${q(name)};`);
      }
    } else if (kind === "mysql") {
      if (typeChanged || nullChanged) out.push(`ALTER TABLE ${t} MODIFY COLUMN ${columnDefinition(kind, { ...c, primaryKey: false, unique: false, references: "" }, false)};`);
      else if (defChanged) out.push(c.default.trim() ? `ALTER TABLE ${t} ALTER COLUMN ${q(name)} SET DEFAULT ${defaultExpr(c.default)};` : `ALTER TABLE ${t} ALTER COLUMN ${q(name)} DROP DEFAULT;`);
    } else {
      if (typeChanged) out.push(`ALTER TABLE ${t} ALTER COLUMN ${q(name)} TYPE ${c.type.trim()} USING ${q(name)}::${c.type.trim()};`);
      if (nullChanged) out.push(`ALTER TABLE ${t} ALTER COLUMN ${q(name)} ${c.nullable ? "DROP" : "SET"} NOT NULL;`);
      if (defChanged) out.push(c.default.trim() ? `ALTER TABLE ${t} ALTER COLUMN ${q(name)} SET DEFAULT ${defaultExpr(c.default)};` : `ALTER TABLE ${t} ALTER COLUMN ${q(name)} DROP DEFAULT;`);
    }
    if (kind === "clickhouse") {
      if (c.unique !== old.unique || c.references !== old.references || c.primaryKey !== old.primaryKey)
        notes.push(`ClickHouse has no unique or foreign-key constraints, and its sorting key cannot change in place ("${name}").`);
      continue;
    }
    if (c.unique !== old.unique && !c.primaryKey) {
      if (c.unique) out.push(`ALTER TABLE ${t} ADD CONSTRAINT ${q(`${after.name}_${c.name}_key`)} UNIQUE (${q(name)});`);
      else notes.push(`Removing the unique constraint on "${name}" needs its constraint name; use SQL (see the Structure tab).`);
    }
    if (c.primaryKey !== old.primaryKey) notes.push(`Primary key changes on "${name}" are not applied by the designer.`);
    if (c.references !== old.references) {
      if (old.references) notes.push(`Dropping the existing foreign key on "${name}" needs its constraint name; use SQL (see the Structure tab).`);
      if (c.references) {
        if (kind === "sqlite") notes.push(`SQLite cannot add a foreign key to an existing column.`);
        else {
          const [rt, rc] = c.references.split(".");
          out.push(`ALTER TABLE ${t} ADD CONSTRAINT ${q(`${after.name}_${c.name}_fkey`)} FOREIGN KEY (${q(name)}) REFERENCES ${q(rt)}(${q(rc)})${c.onDelete ? ` ON DELETE ${c.onDelete}` : ""};`);
        }
      }
    }
  }

  if (after.name !== tableName && kind === "cassandra") notes.push("Cassandra cannot rename tables.");
  else if (after.name !== tableName)
    out.push(
      kind === "mysql" || kind === "clickhouse"
        ? `RENAME TABLE ${t} TO ${q(after.name)};`
        : kind === "mssql"
          ? `EXEC sp_rename N'${t.replace(/'/g, "''")}', N'${after.name.replace(/'/g, "''")}';`
          : `ALTER TABLE ${t} RENAME TO ${q(after.name)};`,
    );
  return { statements: out, notes };
}

export function validateDesign(t: DesignTable): string | undefined {
  if (!t.name.trim()) return "Give the table a name.";
  const cols = t.columns.filter((c) => c.name.trim());
  if (!cols.length) return "Add at least one column.";
  const seen = new Set<string>();
  for (const c of cols) {
    const n = c.name.trim().toLowerCase();
    if (seen.has(n)) return `Column "${c.name}" appears twice.`;
    seen.add(n);
    if (!c.type.trim()) return `Column "${c.name}" needs a type.`;
  }
  return undefined;
}
