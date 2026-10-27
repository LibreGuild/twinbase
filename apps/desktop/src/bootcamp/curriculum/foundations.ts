import type { Chapter } from "../types";

export const foundations: Chapter = {
  id: "foundations",
  title: "Database Foundations",
  subtitle: "What a database is, how data is shaped, and the families of databases you will meet.",
  color: "#55a9a0",
  badge: "foundations",
  lessons: [
    {
      id: "what-is-a-database",
      title: "What is a database?",
      summary: "Why apps don't just write files, and what a DBMS does for you.",
      minutes: 4,
      steps: [
        {
          kind: "read",
          title: "More than a file",
          body: `A **database** is an organised collection of data. The program that stores it, answers questions about it and keeps it safe is the **database management system** (DBMS): PostgreSQL, MySQL, Redis and MongoDB are all DBMSs.

You *could* keep your app's data in a JSON file. It works — until:

- two users save at the same moment and one change disappears,
- the file grows to 20 GB and every lookup reads all of it,
- the power fails halfway through a write.

A DBMS solves exactly these problems: **concurrency**, **fast lookups** and **durability**.`,
        },
        {
          kind: "read",
          title: "Tables, rows and columns",
          body: `Relational databases store data in **tables**. Each **row** is one thing (one user, one order) and each **column** is one fact about it, with a **type** such as \`integer\`, \`text\` or \`timestamp\`.

Hover over the table below: one row is a record, one column is an attribute, and the **primary key** identifies each row uniquely.`,
          widget: "table-anatomy",
        },
        {
          kind: "quiz",
          q: {
            type: "choice",
            prompt: "Two users edit the same record at once and one change is silently lost. Which DBMS job prevents this?",
            options: ["Durability", "Concurrency control", "Indexing", "Compression"],
            answer: 1,
            explain: "Concurrency control coordinates simultaneous readers and writers so changes don't overwrite each other.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "match",
            prompt: "Match each term to its meaning.",
            pairs: [
              ["Row", "One record, e.g. one customer"],
              ["Column", "One attribute with a type"],
              ["Table", "A set of records of the same kind"],
              ["Primary key", "Identifies each row uniquely"],
            ],
            explain: "Tables hold rows; every row has the same typed columns; the primary key tells rows apart.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "blank",
            prompt: "The program that stores data and answers queries is a database management ___.",
            answers: ["system"],
            explain: "DBMS = database management system.",
          },
        },
      ],
    },
    {
      id: "keys-and-relationships",
      title: "Keys and relationships",
      summary: "Primary keys, foreign keys and the three kinds of relationship.",
      minutes: 5,
      steps: [
        {
          kind: "read",
          title: "Primary keys",
          body: `Every table needs a way to point at exactly one row: the **primary key** (PK). It must be **unique** and **never null**.

- A *natural* key is a real-world value (an ISBN, a country code).
- A *surrogate* key is invented by the database (\`id bigserial\`, a UUID).

Surrogate keys are the common default: real-world values change (people change email addresses), invented ids don't.`,
        },
        {
          kind: "read",
          title: "Foreign keys connect tables",
          body: `A **foreign key** (FK) is a column that holds the primary key of a row in another table. The database checks it: you can't create an order for a customer who doesn't exist.

\`\`\`mermaid
erDiagram
  CUSTOMER ||--o{ ORDER : places
  ORDER ||--|{ ORDER_ITEM : contains
  PRODUCT ||--o{ ORDER_ITEM : "appears in"
\`\`\`

- **One-to-many**: one customer, many orders (FK on the "many" side).
- **Many-to-many**: orders and products, through a **join table** (\`order_item\`).
- **One-to-one**: a user and their profile (FK that is also unique).`,
        },
        {
          kind: "quiz",
          q: {
            type: "choice",
            prompt: "Students take many courses and courses have many students. How do you model it?",
            options: ["A course_id column in students", "A student_id column in courses", "A join table enrollments(student_id, course_id)", "One table with both"],
            answer: 2,
            explain: "Many-to-many relationships need a join table holding one row per (student, course) pair.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "truefalse",
            prompt: "A primary key column may contain NULL as long as it is unique.",
            answer: false,
            explain: "Primary keys are UNIQUE and NOT NULL: every row must be identifiable.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "multi",
            prompt: "Which are good reasons to prefer a surrogate key (like a generated id)? Pick all that apply.",
            options: ["Real-world values can change", "It is always shorter to type", "It never needs to carry meaning", "It lets two rows share a key"],
            answers: [0, 2],
            explain: "Surrogate keys stay stable because they carry no business meaning. Keys must still be unique.",
          },
        },
      ],
    },
    {
      id: "database-families",
      title: "SQL, NoSQL and friends",
      summary: "Relational, document, key-value, wide-column, search, graph and vector databases.",
      minutes: 5,
      steps: [
        {
          kind: "read",
          title: "Many shapes of data",
          body: `"NoSQL" just means *not only relational*. Each family is shaped around an access pattern:

| Family | Thinks in | Examples |
| --- | --- | --- |
| Relational | tables + SQL + joins | PostgreSQL, MySQL, SQL Server |
| Key-value | key → value, super fast | Redis, DynamoDB |
| Document | JSON documents | MongoDB, Firestore |
| Wide-column | rows with many sparse columns | Cassandra, ScyllaDB |
| Search | inverted text indexes | Elasticsearch, OpenSearch |
| Vector | embeddings + similarity | pgvector, dedicated vector DBs |
| Graph | nodes + edges | Neo4j |

TwinBase can talk to most of these — and mock several of them locally.`,
        },
        {
          kind: "read",
          title: "How to choose",
          body: `Start from **how the data is read**, not how it looks:

- Need joins, constraints and ad-hoc questions? **Relational** is the safe default.
- Read whole objects by id, schema varies per record? **Document**.
- Tiny values, very hot, often temporary (sessions, caches, counters)? **Key-value**.
- Full-text search with relevance ranking? **Search engine**.
- "Find things *similar* to this"? **Vector** search.

Many systems use several: PostgreSQL as the source of truth, Redis in front of it, a search index next to it.`,
        },
        {
          kind: "quiz",
          q: {
            type: "match",
            prompt: "Match each need to the best-fitting database family.",
            pairs: [
              ["Session store with expiry", "Key-value"],
              ["Invoices with lines and strict totals", "Relational"],
              ["Product catalogue with varied attributes", "Document"],
              ["\"Show me similar images\"", "Vector"],
            ],
            explain: "Pick the family whose model matches how you read the data.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "truefalse",
            prompt: "Using NoSQL means you can't also use a relational database in the same system.",
            answer: false,
            explain: "Polyglot persistence is common: each store does what it is best at.",
          },
        },
      ],
    },
  ],
};
