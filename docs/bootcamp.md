# Bootcamp

The Bootcamp teaches how databases work, from a first `SELECT` to storage internals, caching,
distributed systems and vector search. It is built into the app and uses TwinBase's own mock
servers for hands-on labs.

## How it is organised

| Chapter | Lessons |
| --- | --- |
| 1. Database Foundations | What a database is · keys and relationships · SQL, NoSQL and friends |
| 2. SQL Essentials | SELECT/WHERE · sorting and aggregating · joins · changing data safely |
| 3. Data Modeling | normalization · constraints · denormalization |
| 4. Indexes & Performance | why indexes · B-trees and composite indexes · query plans |
| 5. Transactions & ACID | ACID · isolation levels and anomalies · locks, MVCC, deadlocks |
| 6. Storage Internals | pages and the WAL · B-trees vs LSM trees · the journey of a query |
| 7. Caching with Redis | data structures · TTLs and cache-aside · pitfalls · Redis beyond caching |
| 8. Documents & Key-Value | document databases · partition keys · choosing a store |
| 9. Scaling & System Design | replication · sharding and consistent hashing · CAP/PACELC · a reference architecture |
| 10. Vectors & Embeddings | embeddings · similarity metrics · pgvector in practice · ANN indexes and RAG |

Each lesson is a short sequence of cards:

- **Read** cards in plain language, with Mermaid diagrams and interactive illustrations (table
  anatomy, join explorer, index race, B-tree search, normalisation, isolation anomalies, cache
  simulator, consistent hashing ring, replication lag, LSM vs B-tree, embedding space).
- **Quick checks** of six kinds: multiple choice, select-all, true/false, fill in the blank (one
  word), match the pairs, and put in order. Wrong answers explain why and can be retried.
- **Labs** on real servers: write SQL, Redis commands or mongosh statements; **Check** compares your
  result (or the resulting data) with the reference solution. Hints are free; the solution appears
  after two failed checks.

## XP, levels and badges

| Event | XP |
| --- | --- |
| Quiz right first time / after a retry | 10 / 4 |
| Lab solved / with hint or solution | 25 / 15 |
| Lesson completed | 15 (+10 when every answer was right first time) |
| Replaying a finished lesson | a quarter of its XP |

Levels run from *Curious Intern* to *Legend of the WAL*. Badges: one per chapter, plus *First
Steps*, *Lab Rat*, *Perfectionist*, *On Fire* (3-day streak), *Week Warrior* (7-day streak) and
*Bootcamp Graduate*. Lessons unlock in order within a chapter; every chapter is open.

## Isolation from your projects

Opening the Bootcamp switches to its own workspace, `Bootcamp/` in the app directory, with three
lab servers that start automatically: **Lab PostgreSQL** (with pgvector), **Lab Redis** and **Lab
MongoDB** (ports near 25432, 26379 and 27117). Labs reset their data before they start, so keep your
own work elsewhere; **Back to my workspace** (or the workspace switcher) returns. Progress is saved
in `bootcamp-progress.json` next to the settings, so it survives deleting the workspace.

## Adding lessons

Content lives in `apps/desktop/src/bootcamp/curriculum/` as typed data (`types.ts` describes
chapters, lessons, steps, questions and labs). A lab names its engine, a setup script, a starter,
a hint, a solution and how to check it (`result` compares rows with the solution's; `state` runs a
query and compares with an expected `a|b` line, or a `/regex/`).

`npm test` checks every question's structure. With a core running in isolation, it also runs every
lab and asserts that its solution passes and its starter does not:

```sh
TWINBASE_HOME=$(mktemp -d) twinbase serve --dev-api --port 47811 &   # prints the dev API token
TWINBASE_TEST_CORE="http://127.0.0.1:47811|<token>" npx vitest run src/bootcamp
```
