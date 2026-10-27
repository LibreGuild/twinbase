import type { Chapter } from "../types";

export const internals: Chapter = {
  id: "internals",
  title: "Storage Internals",
  subtitle: "Pages, the write-ahead log, B-trees versus LSM trees, and the journey of a query.",
  color: "#6f9cf0",
  badge: "internals",
  lessons: [
    {
      id: "pages-wal",
      title: "Pages and the WAL",
      summary: "How a database writes safely and fast.",
      minutes: 6,
      steps: [
        {
          kind: "read",
          title: "Everything is a page",
          body: `Databases read and write disk in fixed-size **pages** (8 KB in PostgreSQL, 16 KB in InnoDB). A table is a heap of pages; an index is a tree of pages. Hot pages are cached in RAM in the **buffer pool**.

Writing a changed page to its place on disk at every commit would be slow and unsafe (a crash mid-write corrupts the page). So databases use a **write-ahead log**.`,
        },
        {
          kind: "read",
          title: "Write-ahead logging",
          body: `\`\`\`mermaid
flowchart LR
  T[Transaction] -->|1. append change| W[(WAL - sequential)]
  W -->|2. fsync, COMMIT returns| OK[Client]
  T -->|3. change page in memory| B[Buffer pool]
  B -->|4. later: checkpoint| D[(Data files)]
\`\`\`

The log is appended **sequentially** (fast) and flushed before COMMIT returns. Data pages are written lazily at **checkpoints**. After a crash, the database **replays the WAL** from the last checkpoint. The same log feeds **replicas** and point-in-time recovery.`,
        },
        {
          kind: "quiz",
          q: {
            type: "order",
            prompt: "Order what happens when you COMMIT.",
            items: ["Change is appended to the WAL", "WAL is flushed to disk", "COMMIT returns to the client", "Dirty pages are written at a checkpoint"],
            explain: "Log first, data pages later — that's write-ahead logging.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "choice",
            prompt: "Why is appending to the WAL faster than updating data pages in place?",
            options: ["The WAL is compressed", "Sequential writes are much faster than random writes", "The WAL lives in RAM only", "It skips fsync"],
            answer: 1,
            explain: "Appending is sequential I/O; updating scattered pages is random I/O.",
          },
        },
      ],
    },
    {
      id: "btree-vs-lsm",
      title: "B-trees vs LSM trees",
      summary: "Two ways to organise data on disk, and what each is good at.",
      minutes: 7,
      steps: [
        {
          kind: "read",
          title: "Update in place, or append and merge",
          body: `**B-trees** (PostgreSQL, MySQL) update pages in place: great reads, decent writes.

**LSM trees** (Cassandra, RocksDB, ScyllaDB) buffer writes in memory (*memtable*), flush them as immutable sorted files (*SSTables*), and **compact** files in the background. Writes are very fast; reads may check several files (Bloom filters help).

Write some keys below and watch where they land.`,
          widget: "lsm-vs-btree",
        },
        {
          kind: "quiz",
          q: {
            type: "match",
            prompt: "Match the structure to its trait.",
            pairs: [
              ["B-tree", "Updates pages in place"],
              ["LSM tree", "Appends, then compacts in the background"],
              ["Memtable", "In-memory buffer of recent writes"],
              ["Bloom filter", "Quickly says \"not in this file\""],
            ],
            explain: "LSM trades read work and compaction for very fast writes.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "choice",
            prompt: "A workload ingests millions of sensor readings per second and rarely reads them. Which structure fits best?",
            options: ["B-tree", "LSM tree", "Hash index in RAM only", "No storage at all"],
            answer: 1,
            explain: "LSM trees shine on write-heavy workloads.",
          },
        },
      ],
    },
    {
      id: "query-journey",
      title: "The journey of a query",
      summary: "Parser, planner, executor — and why the planner is the clever bit.",
      minutes: 5,
      steps: [
        {
          kind: "read",
          title: "From text to rows",
          body: `\`\`\`mermaid
flowchart LR
  S[SQL text] --> P[Parser: syntax tree]
  P --> R[Rewriter: views, rules]
  R --> PL[Planner: cheapest plan]
  PL --> E[Executor: pulls rows]
  E --> C[Client]
\`\`\`

The **planner** considers many plans (join orders, index or scan, hash or nested-loop join) and estimates their cost from **table statistics**. That is why the same query can switch plans when data grows — and why stale statistics cause slow queries.

Prepared statements let the database **parse and plan once**, then execute many times with different parameters.`,
        },
        {
          kind: "quiz",
          q: {
            type: "order",
            prompt: "Order the stages of running a query.",
            items: ["Parse", "Rewrite", "Plan", "Execute"],
            explain: "Parser → rewriter → planner → executor.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "blank",
            prompt: "The planner estimates costs using table ___.",
            answers: ["statistics", "stats"],
            explain: "Statistics: row counts, value distributions, distinct counts.",
          },
        },
      ],
    },
  ],
};
