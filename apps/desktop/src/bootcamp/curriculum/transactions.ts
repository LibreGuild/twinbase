import type { Chapter } from "../types";

export const transactions: Chapter = {
  id: "transactions",
  title: "Transactions & ACID",
  subtitle: "All-or-nothing changes, isolation levels and what happens when two users collide.",
  color: "#c9605c",
  badge: "acid",
  lessons: [
    {
      id: "acid",
      title: "ACID in plain words",
      summary: "Atomicity, Consistency, Isolation, Durability.",
      minutes: 5,
      steps: [
        {
          kind: "read",
          title: "All or nothing",
          body: `A **transaction** groups statements so they succeed or fail together:

\`\`\`sql
BEGIN;
UPDATE accounts SET balance = balance - 100 WHERE id = 1;
UPDATE accounts SET balance = balance + 100 WHERE id = 2;
COMMIT;   -- or ROLLBACK to undo both
\`\`\`

- **A**tomicity — all statements happen, or none do.
- **C**onsistency — constraints hold before and after.
- **I**solation — concurrent transactions don't see each other's half-done work.
- **D**urability — once COMMIT returns, the change survives a crash.`,
        },
        {
          kind: "quiz",
          q: {
            type: "match",
            prompt: "Match each ACID property to its promise.",
            pairs: [
              ["Atomicity", "No half-applied transfer"],
              ["Consistency", "Constraints are never violated"],
              ["Isolation", "Concurrent work doesn't interfere"],
              ["Durability", "Committed data survives power loss"],
            ],
            explain: "That's ACID.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "choice",
            prompt: "The server crashes after COMMIT returned but before the data files were updated. What happens on restart?",
            options: ["The transaction is lost", "It is replayed from the write-ahead log", "Half of it is applied", "The table is dropped"],
            answer: 1,
            explain: "Durability comes from the WAL: committed changes are in the log before COMMIT returns.",
          },
        },
      ],
    },
    {
      id: "isolation",
      title: "Isolation levels",
      summary: "Dirty reads, non-repeatable reads, phantoms and lost updates.",
      minutes: 8,
      steps: [
        {
          kind: "read",
          title: "Two users, one row",
          body: `Full isolation is expensive, so databases offer **levels** that allow some anomalies in exchange for speed. Pick a scenario and a level below, then step through the timeline.`,
          widget: "tx-isolation",
        },
        {
          kind: "read",
          title: "The cheat sheet",
          body: `| Level | Dirty read | Non-repeatable read | Phantom |
| --- | --- | --- | --- |
| Read uncommitted | possible* | possible | possible |
| Read committed | – | possible | possible |
| Repeatable read | – | – | possible* |
| Serializable | – | – | – |

\\* PostgreSQL never shows dirty reads and its Repeatable Read also prevents phantoms. Defaults: PostgreSQL and SQL Server use **Read Committed**, MySQL InnoDB uses **Repeatable Read**.`,
        },
        {
          kind: "quiz",
          q: {
            type: "match",
            prompt: "Match the anomaly to what you observe.",
            pairs: [
              ["Dirty read", "You see data that is later rolled back"],
              ["Non-repeatable read", "The same row changes between two reads"],
              ["Phantom", "A repeated query returns new rows"],
              ["Lost update", "Two writers overwrite each other"],
            ],
            explain: "Each level removes some of these.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "choice",
            prompt: "Which level prevents every anomaly by making transactions behave as if they ran one after another?",
            options: ["Read committed", "Repeatable read", "Serializable", "Snapshot of the day"],
            answer: 2,
            explain: "Serializable may abort a transaction with a serialization error; retry it.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "blank",
            prompt: "To stop a lost update while reading a row you plan to change, lock it with SELECT … FOR ___.",
            answers: ["update"],
            explain: "SELECT … FOR UPDATE locks the rows until the transaction ends.",
          },
        },
      ],
    },
    {
      id: "locks-mvcc",
      title: "Locks, MVCC and deadlocks",
      summary: "How readers and writers avoid blocking each other.",
      minutes: 6,
      steps: [
        {
          kind: "read",
          title: "Multi-version concurrency control",
          body: `Instead of making readers wait for writers, most databases keep **several versions** of a row. A reader sees the version that was committed when its snapshot started; a writer creates a new version.

**Readers don't block writers and writers don't block readers.** Two *writers* on the same row still queue on a row lock.

Old versions are cleaned up later (PostgreSQL's **VACUUM**, InnoDB's purge thread).`,
        },
        {
          kind: "read",
          title: "Deadlocks",
          body: `\`\`\`mermaid
sequenceDiagram
  participant T1
  participant T2
  T1->>DB: lock row A
  T2->>DB: lock row B
  T1->>DB: wants row B (waits)
  T2->>DB: wants row A (waits)
  DB-->>T2: deadlock detected, aborted
\`\`\`

The database breaks the cycle by aborting one transaction. Avoid it by touching rows **in the same order** everywhere, keeping transactions short, and retrying when it happens.`,
        },
        {
          kind: "quiz",
          q: {
            type: "truefalse",
            prompt: "With MVCC, a long-running SELECT blocks other transactions from updating the rows it reads.",
            answer: false,
            explain: "The reader keeps using its snapshot; writers create new row versions.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "multi",
            prompt: "Which habits reduce deadlocks? Pick all that apply.",
            options: ["Lock rows in a consistent order", "Keep transactions short", "Wait for user input inside a transaction", "Retry on deadlock errors"],
            answers: [0, 1, 3],
            explain: "Never hold locks while waiting for a human.",
          },
        },
      ],
    },
  ],
};
