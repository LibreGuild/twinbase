import type { Chapter } from "../types";

export const scale: Chapter = {
  id: "scale",
  title: "Scaling & System Design",
  subtitle: "Replication, sharding, consistent hashing, CAP — the database half of system design.",
  color: "#3fbfa8",
  badge: "scale",
  lessons: [
    {
      id: "replication",
      title: "Replication",
      summary: "Leaders, followers, lag and read-your-writes.",
      minutes: 7,
      steps: [
        {
          kind: "read",
          title: "Copies for safety and reads",
          body: `**Replication** keeps copies of the data on several servers:

- **Leader–follower**: writes go to the leader, which streams its log (the WAL) to followers. Followers serve reads.
- **Synchronous**: COMMIT waits for a follower → no data loss on failover, slower writes.
- **Asynchronous**: COMMIT returns immediately → fast, but followers **lag** behind.

Write on the leader below and read from a follower before the change arrives.`,
          widget: "replication",
        },
        {
          kind: "read",
          title: "Living with lag",
          body: `With async replicas a user can save a profile and then *not see their own change* (read from a lagging follower). Common fixes:

- **Read-your-writes**: read from the leader for a few seconds after a user writes.
- **Monotonic reads**: pin a user to one replica.
- Route strongly consistent reads (payments) to the leader.`,
        },
        {
          kind: "quiz",
          q: {
            type: "choice",
            prompt: "A user updates their name and the page still shows the old one. Most likely cause?",
            options: ["Index corruption", "Reading from a lagging async replica", "A deadlock", "Too many columns"],
            answer: 1,
            explain: "Replication lag breaks read-your-writes unless you route those reads to the leader.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "match",
            prompt: "Match the replication mode to its trade-off.",
            pairs: [
              ["Synchronous", "No data loss on failover, slower commits"],
              ["Asynchronous", "Fast commits, possible lag and loss"],
              ["Read replicas", "Scale reads, not writes"],
            ],
            explain: "Replication scales reads; writes still go to one leader.",
          },
        },
      ],
    },
    {
      id: "sharding",
      title: "Sharding and consistent hashing",
      summary: "Splitting data across servers without reshuffling everything.",
      minutes: 8,
      steps: [
        {
          kind: "read",
          title: "When one leader isn't enough",
          body: `**Sharding** (partitioning) splits the rows across servers by a **shard key**. Each shard owns a slice; queries that include the key hit one shard, queries that don't must ask all of them (*scatter-gather*).

Naive placement \`shard = hash(key) % N\` moves almost **every** key when N changes. **Consistent hashing** puts servers and keys on a ring: a key belongs to the next server clockwise, so adding a server only moves the keys of its neighbour. Add and remove nodes below and compare.`,
          widget: "hash-ring",
        },
        {
          kind: "quiz",
          q: {
            type: "choice",
            prompt: "With hash(key) % N and 4 servers, you add a 5th. Roughly what share of keys move?",
            options: ["About 20%", "About 50%", "About 80%", "None"],
            answer: 2,
            explain: "A key stays only if hash % 4 == hash % 5 — about 1 in 5 keys. Consistent hashing moves about 1/5 instead.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "multi",
            prompt: "What makes a good shard key? Pick all that apply.",
            options: ["High cardinality", "Present in most queries", "Monotonically increasing timestamps", "Evenly distributed traffic"],
            answers: [0, 1, 3],
            explain: "Increasing keys send all new writes to the last shard.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "blank",
            prompt: "Consistent hashing places servers and keys on a hash ___.",
            answers: ["ring", "circle"],
            explain: "Each key goes to the first server clockwise; virtual nodes even out the load.",
          },
        },
      ],
    },
    {
      id: "cap",
      title: "CAP, PACELC and consistency",
      summary: "What distributed databases can and can't promise.",
      minutes: 6,
      steps: [
        {
          kind: "read",
          title: "During a partition, choose",
          body: `**CAP**: when the network **P**artitions, a distributed store must choose between **C**onsistency (refuse or delay answers) and **A**vailability (answer, maybe with stale data).

**PACELC** adds the everyday case: **E**lse — even without partitions — you trade **L**atency against **C**onsistency.

| System | On partition | Else |
| --- | --- | --- |
| Single-leader SQL (sync) | C | C |
| DynamoDB / Cassandra (default) | A | L |
| Spanner | C | C (pays latency with TrueTime) |`,
        },
        {
          kind: "read",
          title: "Quorums",
          body: `Leaderless stores (Cassandra, Dynamo-style) write to **W** replicas and read from **R** of **N**. If **R + W > N**, every read overlaps the latest write: e.g. N = 3, W = 2, R = 2.`,
        },
        {
          kind: "quiz",
          q: {
            type: "choice",
            prompt: "N = 3 replicas. Which setting guarantees reads see the latest acknowledged write?",
            options: ["W = 1, R = 1", "W = 2, R = 1", "W = 2, R = 2", "W = 1, R = 2"],
            answer: 2,
            explain: "R + W must exceed N: 2 + 2 > 3.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "truefalse",
            prompt: "CAP says you must always give up one of C, A or P, even when the network is healthy.",
            answer: false,
            explain: "The choice is only forced during a partition; PACELC describes the trade-off otherwise.",
          },
        },
      ],
    },
    {
      id: "design-a-system",
      title: "Putting it together",
      summary: "A data architecture for a busy shop, step by step.",
      minutes: 6,
      steps: [
        {
          kind: "read",
          title: "A reference architecture",
          body: `\`\`\`mermaid
flowchart LR
  U[Users] --> API[API servers]
  API -->|reads| C[(Redis cache)]
  API -->|writes| L[(PostgreSQL leader)]
  L -->|WAL stream| R1[(Read replica)]
  API -->|catalogue reads| R1
  L -->|CDC events| Q[[Stream / queue]]
  Q --> S[(Search index)]
  Q --> V[(pgvector embeddings)]
\`\`\`

- **PostgreSQL** is the source of truth (orders, payments).
- **Redis** absorbs repeated reads (product pages, sessions).
- **Replicas** scale catalogue reads.
- **Change data capture** keeps search and recommendation indexes up to date asynchronously.

Interview tip: start from requirements (reads/s, writes/s, data size, consistency needs), then add each piece only when a number demands it.`,
        },
        {
          kind: "quiz",
          q: {
            type: "order",
            prompt: "Order the steps of a good database design discussion.",
            items: ["Clarify requirements and access patterns", "Estimate load and data size", "Pick the primary store and schema", "Add caches, replicas or shards where numbers demand"],
            explain: "Requirements first, scaling last.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "choice",
            prompt: "The search index lags the database by a few seconds. Is that acceptable for a shop's product search?",
            options: ["No, search must be strongly consistent", "Usually yes — eventual consistency is fine for search", "Only if the index is in Redis", "Only for admins"],
            answer: 1,
            explain: "Derived read models are typically eventually consistent; checkout still reads the source of truth.",
          },
        },
      ],
    },
  ],
};
