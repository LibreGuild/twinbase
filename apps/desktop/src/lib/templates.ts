// Starter schemas for prototyping: each becomes real tables plus generated sample rows.

import { idColumn, newColumn, type DesignTable, type SqlKind } from "./ddlgen";

export interface SchemaTemplate {
  id: string;
  name: string;
  description: string;
  tables: (kind: SqlKind) => { table: DesignTable; rows: number }[];
}

const fk = (kind: SqlKind, name: string, ref: string, onDelete: "CASCADE" | "SET NULL" | "" = "CASCADE", nullable = false) =>
  newColumn(kind, { name, type: kind === "mysql" ? "int" : "integer", nullable, references: ref, onDelete });
const text = (kind: SqlKind, name: string, extra: Partial<ReturnType<typeof newColumn>> = {}) => newColumn(kind, { name, type: kind === "mysql" ? "varchar(255)" : "text", ...extra });
const ts = (kind: SqlKind, name: string) => newColumn(kind, { name, type: kind === "postgres" ? "timestamptz" : kind === "mysql" ? "datetime" : "datetime", nullable: false, default: kind === "sqlite" ? "CURRENT_TIMESTAMP" : "CURRENT_TIMESTAMP" });
const money = (kind: SqlKind, name: string) => newColumn(kind, { name, type: kind === "sqlite" ? "numeric" : "decimal(10,2)", nullable: false });
const int = (kind: SqlKind, name: string, extra: Partial<ReturnType<typeof newColumn>> = {}) => newColumn(kind, { name, type: kind === "mysql" ? "int" : "integer", ...extra });
const bool = (kind: SqlKind, name: string, def: string) => newColumn(kind, { name, type: "boolean", nullable: false, default: def });

export const TEMPLATES: SchemaTemplate[] = [
  {
    id: "blog",
    name: "Blog",
    description: "Authors, posts, comments and tags with a many-to-many join table.",
    tables: (k) => [
      { table: { name: "authors", columns: [idColumn(k), text(k, "name", { nullable: false }), text(k, "email", { nullable: false, unique: true }), text(k, "bio"), ts(k, "created_at")] }, rows: 12 },
      { table: { name: "posts", columns: [idColumn(k), fk(k, "author_id", "authors.id"), text(k, "title", { nullable: false }), newColumn(k, { name: "body", type: "text" }), bool(k, "published", "false"), ts(k, "created_at")] }, rows: 40 },
      { table: { name: "comments", columns: [idColumn(k), fk(k, "post_id", "posts.id"), text(k, "author_name", { nullable: false }), newColumn(k, { name: "body", type: "text", nullable: false }), ts(k, "created_at")] }, rows: 120 },
      { table: { name: "tags", columns: [idColumn(k), text(k, "name", { nullable: false, unique: true })] }, rows: 15 },
      {
        table: {
          name: "post_tags",
          columns: [
            { ...fk(k, "post_id", "posts.id"), primaryKey: true },
            { ...fk(k, "tag_id", "tags.id"), primaryKey: true },
          ],
        },
        rows: 60,
      },
    ],
  },
  {
    id: "shop",
    name: "E-commerce",
    description: "Customers, products, orders and order items — classic relational prototyping.",
    tables: (k) => [
      { table: { name: "customers", columns: [idColumn(k), text(k, "email", { nullable: false, unique: true }), text(k, "full_name", { nullable: false }), text(k, "country"), ts(k, "created_at")] }, rows: 50 },
      { table: { name: "products", columns: [idColumn(k), text(k, "sku", { nullable: false, unique: true }), text(k, "name", { nullable: false }), money(k, "price"), int(k, "stock", { nullable: false, default: "0" }), bool(k, "active", "true")] }, rows: 30 },
      { table: { name: "orders", columns: [idColumn(k), fk(k, "customer_id", "customers.id"), text(k, "status", { nullable: false, default: "pending" }), money(k, "total"), ts(k, "created_at")] }, rows: 120 },
      { table: { name: "order_items", columns: [idColumn(k), fk(k, "order_id", "orders.id"), fk(k, "product_id", "products.id", ""), int(k, "quantity", { nullable: false, default: "1" }), money(k, "unit_price")] }, rows: 300 },
    ],
  },
  {
    id: "saas",
    name: "SaaS (multi-tenant)",
    description: "Organizations, users, memberships with roles, projects and an audit log.",
    tables: (k) => [
      { table: { name: "organizations", columns: [idColumn(k), text(k, "name", { nullable: false }), text(k, "slug", { nullable: false, unique: true }), text(k, "plan", { nullable: false, default: "free" }), ts(k, "created_at")] }, rows: 10 },
      { table: { name: "users", columns: [idColumn(k), text(k, "email", { nullable: false, unique: true }), text(k, "name"), ts(k, "created_at")] }, rows: 40 },
      {
        table: {
          name: "memberships",
          columns: [idColumn(k), fk(k, "organization_id", "organizations.id"), fk(k, "user_id", "users.id"), text(k, "role", { nullable: false, default: "member" }), ts(k, "joined_at")],
        },
        rows: 60,
      },
      { table: { name: "projects", columns: [idColumn(k), fk(k, "organization_id", "organizations.id"), text(k, "name", { nullable: false }), bool(k, "archived", "false"), ts(k, "created_at")] }, rows: 30 },
      { table: { name: "audit_log", columns: [idColumn(k), fk(k, "organization_id", "organizations.id"), fk(k, "user_id", "users.id", "SET NULL", true), text(k, "action", { nullable: false }), newColumn(k, { name: "details", type: k === "postgres" ? "jsonb" : k === "mysql" ? "json" : "text" }), ts(k, "created_at")] }, rows: 150 },
    ],
  },
  {
    id: "tasks",
    name: "Task tracker",
    description: "Boards, columns and cards with assignees — a small kanban backend.",
    tables: (k) => [
      { table: { name: "members", columns: [idColumn(k), text(k, "name", { nullable: false }), text(k, "email", { nullable: false, unique: true })] }, rows: 8 },
      { table: { name: "boards", columns: [idColumn(k), text(k, "name", { nullable: false }), ts(k, "created_at")] }, rows: 4 },
      { table: { name: "lanes", columns: [idColumn(k), fk(k, "board_id", "boards.id"), text(k, "name", { nullable: false }), int(k, "position", { nullable: false, default: "0" })] }, rows: 12 },
      {
        table: {
          name: "cards",
          columns: [idColumn(k), fk(k, "lane_id", "lanes.id"), fk(k, "assignee_id", "members.id", "SET NULL", true), text(k, "title", { nullable: false }), newColumn(k, { name: "description", type: "text" }), newColumn(k, { name: "due_date", type: "date" }), int(k, "position", { nullable: false, default: "0" })],
        },
        rows: 60,
      },
    ],
  },
];
