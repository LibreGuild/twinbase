import { describe, expect, it } from "vitest";
import { alterTableSql, createTableSql, idColumn, newColumn, validateDesign, type DesignTable } from "./ddlgen";

const users: DesignTable = {
  name: "users",
  columns: [idColumn("postgres"), newColumn("postgres", { name: "email", type: "text", nullable: false, unique: true }), newColumn("postgres", { name: "plan", default: "free" })],
};

describe("createTableSql", () => {
  it("renders PostgreSQL with serial ids, constraints and defaults", () => {
    expect(createTableSql("postgres", users)).toBe(
      "CREATE TABLE users (\n  id serial PRIMARY KEY,\n  email text NOT NULL UNIQUE,\n  plan text DEFAULT 'free'\n);",
    );
  });
  it("renders MySQL auto increment and references", () => {
    const orders: DesignTable = {
      name: "orders",
      columns: [idColumn("mysql"), newColumn("mysql", { name: "user_id", type: "int", nullable: false, references: "users.id", onDelete: "CASCADE" })],
    };
    expect(createTableSql("mysql", orders)).toBe(
      "CREATE TABLE `orders` (\n  `id` int PRIMARY KEY AUTO_INCREMENT,\n  `user_id` int NOT NULL REFERENCES `users`(`id`) ON DELETE CASCADE\n);",
    );
  });
  it("uses a table-level key for composite primary keys", () => {
    const t: DesignTable = { name: "tags", columns: [newColumn("postgres", { name: "a", type: "int", primaryKey: true, nullable: false }), newColumn("postgres", { name: "b", type: "int", primaryKey: true, nullable: false })] };
    expect(createTableSql("postgres", t)).toContain("PRIMARY KEY (a, b)");
  });
});

describe("alterTableSql", () => {
  const before: DesignTable = { original: "users", name: "users", columns: users.columns.map((c) => ({ ...c, original: c.name })) };
  it("adds, renames, drops and changes columns", () => {
    const after: DesignTable = {
      ...before,
      name: "accounts",
      columns: [
        before.columns[0],
        { ...before.columns[1], name: "email_address", nullable: true },
        newColumn("postgres", { name: "created_at", type: "timestamptz", default: "now()" }),
      ],
    };
    const plan = alterTableSql("postgres", before, after);
    expect(plan.statements).toEqual([
      "ALTER TABLE users DROP COLUMN plan;",
      "ALTER TABLE users RENAME COLUMN email TO email_address;",
      "ALTER TABLE users ALTER COLUMN email_address DROP NOT NULL;",
      "ALTER TABLE users ADD COLUMN created_at timestamptz DEFAULT now();",
      "ALTER TABLE users RENAME TO accounts;",
    ]);
  });
  it("uses MODIFY on MySQL", () => {
    const b: DesignTable = { original: "t", name: "t", columns: [{ ...newColumn("mysql", { name: "n", type: "int" }), original: "n" }] };
    const a: DesignTable = { ...b, columns: [{ ...b.columns[0], type: "bigint", nullable: false }] };
    expect(alterTableSql("mysql", b, a).statements).toEqual(["ALTER TABLE `t` MODIFY COLUMN `n` bigint NOT NULL;"]);
  });
  it("explains what SQLite cannot alter", () => {
    const b: DesignTable = { original: "t", name: "t", columns: [{ ...newColumn("sqlite", { name: "n", type: "integer" }), original: "n" }] };
    const a: DesignTable = { ...b, columns: [{ ...b.columns[0], type: "text" }] };
    const plan = alterTableSql("sqlite", b, a);
    expect(plan.statements).toEqual([]);
    expect(plan.notes[0]).toMatch(/SQLite/);
  });
});

describe("validateDesign", () => {
  it("rejects duplicates and empty names", () => {
    expect(validateDesign({ name: "", columns: [] })).toBeTruthy();
    expect(validateDesign({ name: "t", columns: [newColumn("postgres", { name: "a" }), newColumn("postgres", { name: "A" })] })).toMatch(/twice/);
    expect(validateDesign(users)).toBeUndefined();
  });
});

describe("SQL Server", () => {
  const t: DesignTable = {
    name: "orders",
    columns: [idColumn("mssql"), newColumn("mssql", { name: "note", nullable: false, default: "none" }), newColumn("mssql", { name: "paid", type: "bit", default: "false" })],
  };
  it("renders identity columns, brackets and bit defaults", () => {
    expect(createTableSql("mssql", t)).toBe(
      "CREATE TABLE [orders] (\n  [id] int IDENTITY(1,1) PRIMARY KEY,\n  [note] nvarchar(255) NOT NULL DEFAULT 'none',\n  [paid] bit DEFAULT 0\n);",
    );
  });
  it("alters with ADD, ALTER COLUMN, sp_rename and named-default handling", () => {
    const before = { ...t, original: "orders", columns: t.columns.map((c) => ({ ...c, original: c.name })) };
    const after: DesignTable = {
      ...before,
      name: "purchases",
      columns: [
        before.columns[0],
        { ...before.columns[1], name: "memo", type: "nvarchar(max)", nullable: true, default: "" },
        newColumn("mssql", { name: "total", type: "decimal(10,2)" }),
      ],
    };
    const { statements } = alterTableSql("mssql", before, after);
    expect(statements[0]).toMatch(/^EXEC\(N'DECLARE @n sysname = .*paid.*'\);$/);
    expect(statements).toContain("ALTER TABLE [orders] DROP COLUMN [paid];");
    expect(statements).toContain("ALTER TABLE [orders] ADD [total] decimal(10,2);");
    expect(statements).toContain("EXEC sp_rename N'[orders].[note]', N'memo', N'COLUMN';");
    expect(statements).toContain("ALTER TABLE [orders] ALTER COLUMN [memo] nvarchar(max) NULL;");
    expect(statements.at(-1)).toBe("EXEC sp_rename N'[orders]', N'purchases';");
  });
});

describe("ClickHouse", () => {
  const t: DesignTable = {
    name: "hits",
    columns: [idColumn("clickhouse"), newColumn("clickhouse", { name: "url", nullable: false }), newColumn("clickhouse", { name: "ref", default: "none" }), newColumn("clickhouse", { name: "at", type: "DateTime", nullable: false, default: "now()" })],
  };
  it("renders MergeTree tables sorted by the key, with Nullable types", () => {
    expect(createTableSql("clickhouse", t)).toBe(
      "CREATE TABLE hits (\n  id UInt64,\n  url String,\n  ref Nullable(String) DEFAULT 'none',\n  at DateTime DEFAULT now()\n) ENGINE = MergeTree ORDER BY (id);",
    );
  });
  it("alters with MODIFY COLUMN and RENAME", () => {
    const before = { ...t, original: "hits", columns: t.columns.map((c) => ({ ...c, original: c.name })) };
    const after: DesignTable = { ...before, name: "visits", columns: [before.columns[0], { ...before.columns[1], name: "path", type: "LowCardinality(String)" }, { ...before.columns[2], default: "" }, before.columns[3]] };
    const { statements } = alterTableSql("clickhouse", before, after);
    expect(statements).toEqual([
      "ALTER TABLE hits RENAME COLUMN url TO path;",
      "ALTER TABLE hits MODIFY COLUMN path LowCardinality(String);",
      "ALTER TABLE hits MODIFY COLUMN ref REMOVE DEFAULT;",
      "RENAME TABLE hits TO visits;",
    ]);
  });
});

describe("Cassandra", () => {
  it("renders a primary key and plain columns", () => {
    const t: DesignTable = { name: "events", columns: [idColumn("cassandra"), newColumn("cassandra", { name: "kind", nullable: false, default: "x", unique: true })] };
    expect(createTableSql("cassandra", t)).toBe('CREATE TABLE events (\n  id uuid,\n  kind text,\n  PRIMARY KEY (id)\n);');
  });
});

describe("GoogleSQL", () => {
  const t: DesignTable = { name: "items", columns: [idColumn("spanner"), newColumn("spanner", { name: "name", nullable: false }), newColumn("spanner", { name: "price", type: "NUMERIC", default: "0" })] };
  it("puts Spanner's primary key after the columns", () => {
    expect(createTableSql("spanner", t)).toBe("CREATE TABLE items (\n  id INT64 NOT NULL,\n  name STRING(MAX) NOT NULL,\n  price NUMERIC DEFAULT (0)\n) PRIMARY KEY (id);");
  });
  it("declares BigQuery keys NOT ENFORCED", () => {
    const b: DesignTable = { name: "items", columns: [idColumn("bigquery"), newColumn("bigquery", { name: "name" })] };
    expect(createTableSql("bigquery", b)).toBe("CREATE TABLE items (\n  id INT64 NOT NULL,\n  name STRING,\n  PRIMARY KEY (id) NOT ENFORCED\n);");
  });
});

describe("Oracle", () => {
  it("writes identity and DEFAULT before NOT NULL", () => {
    const t: DesignTable = { name: "accounts", columns: [idColumn("oracle"), newColumn("oracle", { name: "balance", type: "NUMBER(12,2)", nullable: false, default: "0" })] };
    expect(createTableSql("oracle", t)).toBe('CREATE TABLE accounts (\n  id NUMBER(10) GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,\n  balance NUMBER(12,2) DEFAULT 0 NOT NULL\n);');
  });
});
