import type { Chapter } from "../types";

export const nosql: Chapter = {
  id: "nosql",
  title: "Documents & Key-Value",
  subtitle: "MongoDB-style documents, DynamoDB partition keys and designing for access patterns.",
  color: "#4faa55",
  badge: "nosql",
  lessons: [
    {
      id: "documents",
      title: "Document databases",
      summary: "JSON documents, embedding versus referencing.",
      minutes: 7,
      steps: [
        {
          kind: "read",
          title: "Store the object you read",
          body: `A document database stores **JSON-like documents** in **collections**. A document can nest arrays and objects, so data read together lives together:

\`\`\`json
{ "_id": 1, "name": "Ada", "address": { "city": "London" },
  "orders": [ { "id": 101, "total": 165.0 } ] }
\`\`\`

**Embed** when the child belongs to one parent and is read with it (addresses, order lines). **Reference** (store an id) when the child is shared, large, or grows without bound (comments on a viral post).`,
        },
        {
          kind: "quiz",
          q: {
            type: "choice",
            prompt: "A blog post can collect millions of comments. Embed them in the post document?",
            options: ["Yes, always embed", "No — store comments separately with a post_id", "Store them in the _id", "Use a single giant array per day"],
            answer: 1,
            explain: "Unbounded arrays make documents huge (MongoDB caps documents at 16 MB). Reference instead.",
          },
        },
        {
          kind: "lab",
          lab: {
            engine: "mongodb",
            title: "Insert and query documents",
            task: "Insert three books into `books`: **SQL Basics** (2019, tags `[\"sql\"]`), **Redis in Action** (2013, tags `[\"redis\", \"cache\"]`) and **Designing Data-Intensive Applications** (2017, tags `[\"systems\"]`). Use fields `title`, `year`, `tags`.",
            setup: "db.books.drop()",
            starter: 'db.books.insertMany([\n  { title: "SQL Basics", year: 2019, tags: ["sql"] },\n])',
            hint: "Put all three objects in the array passed to insertMany.",
            solution:
              'db.books.insertMany([{ title: "SQL Basics", year: 2019, tags: ["sql"] }, { title: "Redis in Action", year: 2013, tags: ["redis", "cache"] }, { title: "Designing Data-Intensive Applications", year: 2017, tags: ["systems"] }])',
            check: { kind: "state", query: 'db.books.countDocuments({})\ndb.books.countDocuments({ tags: "cache", year: { $lt: 2015 } })', expect: "3|1" },
          },
        },
      ],
    },
    {
      id: "partition-keys",
      title: "Partition keys and access patterns",
      summary: "How DynamoDB spreads data, and why hot partitions hurt.",
      minutes: 7,
      steps: [
        {
          kind: "read",
          title: "Design for the queries",
          body: `DynamoDB and Cassandra split a table into **partitions** by hashing the **partition key**. A query is fast when it names one partition key (and optionally a range of the **sort key**). There are no joins, so you design the table **from the list of queries**:

| Query | Partition key | Sort key |
| --- | --- | --- |
| Orders of a customer, newest first | \`CUSTOMER#42\` | \`ORDER#2025-02-14\` |
| Messages in a chat room | \`ROOM#7\` | timestamp |

This is the idea behind **single-table design**: several entity types share one table, keyed so each query is a single partition read.`,
        },
        {
          kind: "read",
          title: "Hot partitions",
          body: `Every partition has a throughput limit. A key like \`status = "active"\` or today's date sends almost all traffic to **one partition** — a *hot partition*. Choose high-cardinality keys (user id, device id) or add a random/calculated **suffix** (\`DATE#2025-02-14#7\`) to spread writes.`,
        },
        {
          kind: "quiz",
          q: {
            type: "choice",
            prompt: "Which partition key spreads a high-volume event table best?",
            options: ["event_type (5 values)", "country (40 values)", "device_id (millions of values)", "\"events\" for every row"],
            answer: 2,
            explain: "High cardinality and even access spread load across partitions.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "truefalse",
            prompt: "In DynamoDB you usually design tables from the entity relationships first and figure out queries later.",
            answer: false,
            explain: "Key-value stores are designed access-pattern first.",
          },
        },
      ],
    },
    {
      id: "choosing",
      title: "Choosing the right store",
      summary: "A decision guide you can reuse in design interviews.",
      minutes: 5,
      steps: [
        {
          kind: "read",
          title: "A practical flow",
          body: `\`\`\`mermaid
flowchart TD
  A{Need joins, constraints,<br/>ad-hoc questions?} -- yes --> PG[Relational: PostgreSQL / MySQL]
  A -- no --> B{Simple key lookups,<br/>huge scale, known queries?}
  B -- yes --> KV[Key-value / wide-column: DynamoDB, Cassandra]
  B -- no --> C{Flexible nested objects<br/>read whole?}
  C -- yes --> DOC[Document: MongoDB, Firestore]
  C -- no --> D{Text relevance or<br/>similarity search?}
  D -- text --> ES[Search: Elasticsearch]
  D -- similarity --> VEC[Vector: pgvector]
\`\`\`

And whatever you choose, put **Redis** in front when the same reads repeat.`,
        },
        {
          kind: "quiz",
          q: {
            type: "match",
            prompt: "Match the product requirement to a store.",
            pairs: [
              ["Bank ledger with strict invariants", "PostgreSQL"],
              ["Mobile app syncing user documents offline", "Firestore"],
              ["Search box with typo tolerance", "Elasticsearch"],
              ["IoT readings, 1M writes/s, query by device", "Cassandra"],
            ],
            explain: "Match the store to the dominant access pattern.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "choice",
            prompt: "You can't decide yet and need joins, transactions and flexible JSON fields. A sensible default?",
            options: ["PostgreSQL with jsonb columns", "A plain CSV file", "Redis as the only store", "A search engine as the source of truth"],
            answer: 0,
            explain: "PostgreSQL covers relational needs and semi-structured data with jsonb; specialise later when a workload demands it.",
          },
        },
      ],
    },
  ],
};
