import type { Chapter } from "../types";
import { SHOP_SQL } from "./datasets";

export const indexes: Chapter = {
  id: "indexes",
  title: "Indexes & Performance",
  subtitle: "Why some queries take 1 ms and others 10 s — and how indexes and plans explain it.",
  color: "#e0954a",
  badge: "speed",
  lessons: [
    {
      id: "why-indexes",
      title: "Why indexes exist",
      summary: "Full scans versus index lookups, at scale.",
      minutes: 5,
      steps: [
        {
          kind: "read",
          title: "The phone-book trick",
          body: `Without an index, finding \`WHERE email = 'ken@x.io'\` means reading **every row** — a *sequential scan*. With an index, the database jumps almost straight to it, like using the alphabetical order of a phone book.

Drag the slider: a scan grows **linearly** with the table, a B-tree lookup grows with the **logarithm** — a billion rows is only about 30 comparisons.`,
          widget: "index-race",
        },
        {
          kind: "read",
          title: "Nothing is free",
          body: `Every index:

- speeds up reads that can use it,
- **slows down writes** (each INSERT/UPDATE/DELETE must update the index too),
- takes disk space and memory.

So index the columns you **filter, join and sort by** — not every column.`,
        },
        {
          kind: "quiz",
          q: {
            type: "choice",
            prompt: "Roughly how many comparisons does a balanced tree need to find one key among 1,000,000?",
            options: ["About 20", "About 1,000", "About 500,000", "1,000,000"],
            answer: 0,
            explain: "log₂(1,000,000) ≈ 20.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "truefalse",
            prompt: "Adding an index makes INSERTs into that table faster.",
            answer: false,
            explain: "Writes must maintain every index, so they get slower, not faster.",
          },
        },
      ],
    },
    {
      id: "btrees",
      title: "Inside a B-tree",
      summary: "How lookups walk the tree, and why column order matters in composite indexes.",
      minutes: 7,
      steps: [
        {
          kind: "read",
          title: "A wide, shallow tree",
          body: `The default index in PostgreSQL, MySQL and SQL Server is a **B-tree** (strictly a B+tree). Each node is a disk page holding *hundreds* of sorted keys, so the tree stays only 3–4 levels deep even for huge tables. Leaves are linked, which makes **range scans** (\`BETWEEN\`, \`ORDER BY\`) cheap.

Search for a key below and watch the path.`,
          widget: "btree",
        },
        {
          kind: "read",
          title: "Composite indexes: leftmost first",
          body: `An index on \`(last_name, first_name)\` is sorted by last name, then first name — like a phone book.

- ✅ \`WHERE last_name = 'Lee'\`
- ✅ \`WHERE last_name = 'Lee' AND first_name = 'Ann'\`
- ❌ \`WHERE first_name = 'Ann'\` — the first column isn't constrained, so the order doesn't help.

An index that contains **every column a query needs** is *covering*: the database can answer from the index alone (an *index-only scan*).`,
        },
        {
          kind: "quiz",
          q: {
            type: "multi",
            prompt: "An index exists on (country, city). Which filters can use it efficiently?",
            options: ["country = 'NL'", "country = 'NL' AND city = 'Utrecht'", "city = 'Utrecht'", "country = 'NL' ORDER BY city"],
            answers: [0, 1, 3],
            explain: "The leftmost column must be constrained; ordering by the next column then comes for free.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "blank",
            prompt: "An index holding every column a query needs is called a ___ index.",
            answers: ["covering"],
            explain: "Covering indexes allow index-only scans.",
          },
        },
      ],
    },
    {
      id: "query-plans",
      title: "Reading query plans",
      summary: "EXPLAIN, scans, joins and the questions to ask.",
      minutes: 7,
      steps: [
        {
          kind: "read",
          title: "Ask the planner",
          body: `\`EXPLAIN\` shows the plan the database picked; \`EXPLAIN ANALYZE\` also runs it and shows real timings.

\`\`\`
Nested Loop  (cost=0.29..16.33 rows=1)
  -> Index Scan using customers_pkey on customers
  -> Seq Scan on orders  (rows=50000)   ← suspicious
\`\`\`

Things to look for:

- **Seq Scan** on a big table behind a selective filter → missing index?
- **estimated rows** far from **actual rows** → stale statistics (\`ANALYZE\`).
- **Sort** spilling to disk, or a **Nested Loop** over many rows.

TwinBase draws plans as a tree with the most expensive steps highlighted — open any SQL tab and press **Explain**.`,
        },
        {
          kind: "quiz",
          q: {
            type: "choice",
            prompt: "The plan estimates 10 rows but EXPLAIN ANALYZE shows 2,000,000. What is the first thing to try?",
            options: ["Drop all indexes", "Refresh table statistics (ANALYZE)", "Add LIMIT 10", "Restart the server"],
            answer: 1,
            explain: "Bad estimates usually mean stale statistics; the planner chose a plan for the wrong row count.",
          },
        },
        {
          kind: "lab",
          lab: {
            engine: "postgres",
            title: "Index the hot filter",
            task: "The app constantly runs `SELECT * FROM orders WHERE customer_id = $1`. Create an index named **`orders_customer_idx`** on `orders(customer_id)`.",
            setup: SHOP_SQL,
            starter: "CREATE INDEX ",
            hint: "`CREATE INDEX orders_customer_idx ON orders (customer_id);`",
            solution: "CREATE INDEX orders_customer_idx ON orders (customer_id);",
            check: { kind: "state", query: "SELECT indexname FROM pg_indexes WHERE tablename = 'orders' AND indexname = 'orders_customer_idx'", expect: "orders_customer_idx" },
          },
        },
      ],
    },
  ],
};
