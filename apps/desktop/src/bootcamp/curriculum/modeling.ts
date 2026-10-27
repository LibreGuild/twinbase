import type { Chapter } from "../types";

export const modeling: Chapter = {
  id: "modeling",
  title: "Data Modeling",
  subtitle: "Design tables that stay correct as the app grows: normal forms, constraints and trade-offs.",
  color: "#8a6bd1",
  badge: "architect",
  lessons: [
    {
      id: "normalization",
      title: "Normalization",
      summary: "Store each fact once: 1NF, 2NF and 3NF in plain words.",
      minutes: 7,
      steps: [
        {
          kind: "read",
          title: "Say it once",
          body: `When the same fact is stored in many places, the copies drift apart: change a customer's city in one order and the other orders still show the old one. **Normalization** removes that duplication by splitting tables so each fact lives in **one** place.

Press the button below to normalise a messy spreadsheet step by step.`,
          widget: "normalize",
        },
        {
          kind: "read",
          title: "The normal forms",
          body: `- **1NF** — every cell holds one value (no "red, blue" lists in a column) and rows are identifiable.
- **2NF** — every non-key column depends on the **whole** key (in \`order_items(order_id, product_id, product_name)\`, the name depends only on product_id → move it out).
- **3NF** — non-key columns depend **only on the key**, not on other non-key columns (\`zip → city\` belongs in its own table).

> "The key, the whole key, and nothing but the key."`,
        },
        {
          kind: "quiz",
          q: {
            type: "choice",
            prompt: "A `tags` column holds \"sql, redis, cache\". Which normal form does this break?",
            options: ["1NF", "2NF", "3NF", "None"],
            answer: 0,
            explain: "1NF requires atomic values; use a separate tags table (or an array type deliberately).",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "blank",
            prompt: "In 3NF every non-key column depends on the key, the whole key, and nothing but the ___.",
            answers: ["key"],
            explain: "Classic summary by Bill Kent.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "choice",
            prompt: "`employees(id, name, dept_id, dept_name)` — what is the 3NF fix?",
            options: ["Drop dept_id", "Move dept_name to departments(id, name)", "Add an index on dept_name", "Make name the primary key"],
            answer: 1,
            explain: "dept_name depends on dept_id, not on the employee: give departments their own table.",
          },
        },
      ],
    },
    {
      id: "constraints",
      title: "Constraints",
      summary: "Let the database refuse bad data: NOT NULL, UNIQUE, CHECK, FOREIGN KEY.",
      minutes: 6,
      steps: [
        {
          kind: "read",
          title: "Rules that can't be skipped",
          body: `Application code has bugs; **constraints** are enforced on every write, by every client:

\`\`\`sql
CREATE TABLE accounts (
  id       bigint PRIMARY KEY,
  email    text NOT NULL UNIQUE,
  balance  numeric(12,2) NOT NULL CHECK (balance >= 0),
  owner_id bigint REFERENCES users(id) ON DELETE CASCADE
);
\`\`\`

\`ON DELETE\` decides what happens to children when the parent goes: \`CASCADE\` deletes them, \`SET NULL\` detaches them, the default (\`NO ACTION\`/\`RESTRICT\`) refuses the delete.`,
        },
        {
          kind: "quiz",
          q: {
            type: "match",
            prompt: "Match the constraint to the rule it enforces.",
            pairs: [
              ["NOT NULL", "A value must be present"],
              ["UNIQUE", "No two rows share the value"],
              ["CHECK", "A condition must hold"],
              ["FOREIGN KEY", "The referenced row must exist"],
            ],
            explain: "Constraints turn data rules into guarantees.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "choice",
            prompt: "Deleting a user should also delete their sessions. Which FK action?",
            options: ["ON DELETE RESTRICT", "ON DELETE SET NULL", "ON DELETE CASCADE", "ON UPDATE CASCADE"],
            answer: 2,
            explain: "CASCADE removes dependent rows together with the parent.",
          },
        },
        {
          kind: "lab",
          lab: {
            engine: "postgres",
            title: "A table that defends itself",
            task: "Create a table `wallets` with columns `id integer PRIMARY KEY`, `owner text NOT NULL UNIQUE` and `balance numeric(10,2) NOT NULL` with a **CHECK** that the balance is never negative. Then insert one wallet: `(1, 'ada', 50)`.",
            setup: "DROP TABLE IF EXISTS wallets;",
            starter: "CREATE TABLE wallets (\n  id integer PRIMARY KEY,\n  owner text NOT NULL UNIQUE,\n  balance numeric(10,2) NOT NULL\n);\n",
            hint: "Add `CHECK (balance >= 0)` after the balance type, then `INSERT INTO wallets VALUES (1, 'ada', 50);`",
            solution:
              "CREATE TABLE wallets (id integer PRIMARY KEY, owner text NOT NULL UNIQUE, balance numeric(10,2) NOT NULL CHECK (balance >= 0));\nINSERT INTO wallets VALUES (1, 'ada', 50);",
            check: {
              kind: "state",
              query:
                "SELECT (SELECT COUNT(*) FROM wallets) AS rows, (SELECT COUNT(*) FROM information_schema.table_constraints WHERE table_name = 'wallets' AND constraint_type = 'CHECK') > 0 AS has_check",
              expect: "1|t",
            },
          },
        },
      ],
    },
    {
      id: "denormalization",
      title: "When to break the rules",
      summary: "Denormalization, JSON columns and read models.",
      minutes: 5,
      steps: [
        {
          kind: "read",
          title: "Trading writes for reads",
          body: `Normalised schemas are great for correctness, but some screens need data from ten tables. **Denormalization** stores a deliberate copy to make reads cheap:

- a cached \`order_count\` on \`customers\`,
- a **materialized view** refreshed every few minutes,
- a \`jsonb\` column for attributes that differ per product.

The price: every write must keep the copy in sync (triggers, application code, or accepting it may be a bit stale).`,
        },
        {
          kind: "read",
          title: "A rule of thumb",
          body: `\`\`\`mermaid
flowchart TD
  A[Start normalised] --> B{Slow read path measured?}
  B -- no --> A
  B -- yes --> C{Index or better query fixes it?}
  C -- yes --> D[Fix query]
  C -- no --> E[Denormalise that path and document how it stays in sync]
\`\`\``,
        },
        {
          kind: "quiz",
          q: {
            type: "multi",
            prompt: "What do you take on when you denormalise? Pick all that apply.",
            options: ["Keeping copies in sync on writes", "Possible stale data", "Faster reads on that path", "Automatic consistency for free"],
            answers: [0, 1, 2],
            explain: "Denormalization speeds reads at the cost of write complexity and possible staleness.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "truefalse",
            prompt: "You should denormalise early, before measuring, because joins are always slow.",
            answer: false,
            explain: "Joins on indexed keys are fast; measure first, then denormalise the specific slow path.",
          },
        },
      ],
    },
  ],
};
