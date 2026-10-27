import type { Chapter } from "../types";
import { DOCS_SQL } from "./datasets";

export const vectors: Chapter = {
  id: "vectors",
  title: "Vectors & Embeddings",
  subtitle: "How AI turns meaning into numbers, and how pgvector finds the nearest ones.",
  color: "#b46be0",
  badge: "vectors",
  lessons: [
    {
      id: "embeddings",
      title: "What is an embedding?",
      summary: "Meaning as coordinates: similar things land close together.",
      minutes: 6,
      steps: [
        {
          kind: "read",
          title: "Meaning as a point in space",
          body: `An **embedding model** turns text, images or audio into a list of numbers — a **vector** — such that similar meanings get **nearby** vectors. Real models use hundreds or thousands of dimensions (e.g. 384, 768, 1536); the idea is the same in 2-D.

Click anywhere in the map to place a query and see its nearest neighbours.`,
          widget: "vector-space",
        },
        {
          kind: "read",
          title: "What it unlocks",
          body: `- **Semantic search**: "can't log in" finds "Resetting your password" without sharing a word.
- **Recommendations**: items near the ones a user liked.
- **Deduplication** and **clustering**.
- **RAG** (retrieval-augmented generation): fetch the most relevant documents and give them to an LLM as context.`,
        },
        {
          kind: "quiz",
          q: {
            type: "choice",
            prompt: "Two support articles have embeddings that are very close. What does that suggest?",
            options: ["They share many exact words", "They mean similar things", "They were written on the same day", "They have the same length"],
            answer: 1,
            explain: "Embeddings capture meaning, not exact wording.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "truefalse",
            prompt: "Vectors produced by two different embedding models can be compared directly.",
            answer: false,
            explain: "Each model has its own space; compare vectors from the same model (and dimension).",
          },
        },
      ],
    },
    {
      id: "similarity",
      title: "Measuring similarity",
      summary: "L2 distance, cosine distance and inner product.",
      minutes: 6,
      steps: [
        {
          kind: "read",
          title: "Three ways to say \"close\"",
          body: `| Metric | pgvector operator | Meaning |
| --- | --- | --- |
| L2 (Euclidean) distance | \`<->\` | straight-line distance |
| Cosine distance | \`<=>\` | 1 − cos(angle): direction only |
| Negative inner product | \`<#>\` | −(a·b): larger dot product = closer |
| L1 (Manhattan) | \`<+>\` | sum of absolute differences |

Most text embedding models are trained for **cosine** similarity. If vectors are **normalised** to length 1, cosine, inner product and L2 give the **same ranking** — and inner product is the cheapest to compute.

Switch metrics in the playground to see when rankings differ.`,
          widget: "vector-space",
        },
        {
          kind: "quiz",
          q: {
            type: "match",
            prompt: "Match the pgvector operator to its metric.",
            pairs: [
              ["<->", "L2 distance"],
              ["<=>", "Cosine distance"],
              ["<#>", "Negative inner product"],
              ["<+>", "L1 distance"],
            ],
            explain: "All four sort ascending: smaller = more similar.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "choice",
            prompt: "Cosine distance between [1, 0] and [10, 0]?",
            options: ["0", "9", "1", "0.5"],
            answer: 0,
            explain: "Same direction means angle 0, so cosine distance is 0 — length is ignored.",
          },
        },
      ],
    },
    {
      id: "pgvector",
      title: "pgvector in practice",
      summary: "Store embeddings in PostgreSQL and query nearest neighbours.",
      minutes: 8,
      steps: [
        {
          kind: "read",
          title: "Vectors are just a column type",
          body: `\`\`\`sql
CREATE EXTENSION IF NOT EXISTS vector;
CREATE TABLE docs (
  id bigserial PRIMARY KEY,
  title text,
  embedding vector(1536)       -- or halfvec(1536) to halve storage
);
SELECT title FROM docs
ORDER BY embedding <=> $1      -- $1 = the query's embedding
LIMIT 5;
\`\`\`

- \`vector(n)\` stores 32-bit floats; \`halfvec(n)\` stores 16-bit floats (half the size, tiny accuracy loss); \`sparsevec(n)\` stores only non-zero entries.
- Combine with normal SQL: \`WHERE tenant_id = 7 ORDER BY embedding <=> $1\`.

TwinBase's PostgreSQL mocks emulate pgvector, so this lab runs locally.`,
        },
        {
          kind: "lab",
          lab: {
            engine: "postgres",
            title: "Find the closest help article",
            task: "The table `docs(id, title, embedding vector(3))` holds tiny embeddings of help articles. A user asks something whose embedding is **[0.15, 0.85, 0.25]**. Return the **titles of the 2 closest articles by cosine distance**, closest first.",
            setup: `CREATE EXTENSION IF NOT EXISTS vector;\n${DOCS_SQL}`,
            starter: "SELECT title\nFROM docs\nORDER BY embedding ",
            hint: "`ORDER BY embedding <=> '[0.15, 0.85, 0.25]' LIMIT 2`",
            solution: "SELECT title FROM docs ORDER BY embedding <=> '[0.15, 0.85, 0.25]' LIMIT 2;",
            check: { kind: "result" },
          },
        },
        {
          kind: "quiz",
          q: {
            type: "choice",
            prompt: "Your 1536-dimension embeddings use too much disk. What does pgvector offer?",
            options: ["Store them as text", "Use halfvec(1536) — 16-bit floats, half the size", "Drop half of the dimensions at random", "Use integer[]"],
            answer: 1,
            explain: "halfvec keeps every dimension at half precision; recall barely changes for most models.",
          },
        },
      ],
    },
    {
      id: "ann-rag",
      title: "ANN indexes and RAG",
      summary: "HNSW and IVFFlat, recall versus speed, and the retrieval pipeline.",
      minutes: 7,
      steps: [
        {
          kind: "read",
          title: "Approximate nearest neighbours",
          body: `Exact search compares the query with **every** vector. For millions of rows that is too slow, so vector indexes are **approximate** (ANN): much faster, occasionally missing a true neighbour. The fraction found is **recall**.

- **HNSW** — a layered graph you greedily walk towards the query. Great recall and speed; slower to build, more memory. Tune \`m\`, \`ef_construction\`, and at query time \`hnsw.ef_search\`.
- **IVFFlat** — clusters vectors into \`lists\`; a query searches the nearest \`probes\` clusters. Faster to build; create it **after** loading data.

\`\`\`sql
CREATE INDEX ON docs USING hnsw (embedding vector_cosine_ops);
\`\`\`

Use the opclass that matches your operator (\`vector_cosine_ops\` for \`<=>\`).`,
        },
        {
          kind: "read",
          title: "Retrieval-augmented generation",
          body: `\`\`\`mermaid
flowchart LR
  Q[User question] --> E[Embed question]
  E --> S[(pgvector: top-k by cosine)]
  S --> P[Build prompt with the k passages]
  P --> L[LLM answers, citing passages]
\`\`\`

Chunk documents into passages (a few hundred tokens), embed each chunk once, and store \`(chunk, embedding, source)\`. At question time, embed the question, fetch the top-k chunks, and let the model answer **from those**.`,
        },
        {
          kind: "quiz",
          q: {
            type: "match",
            prompt: "Match the term to its meaning.",
            pairs: [
              ["Recall", "Share of true neighbours the index finds"],
              ["HNSW", "Layered proximity graph"],
              ["IVFFlat", "Search only the nearest clusters"],
              ["ef_search", "HNSW knob: more = better recall, slower"],
            ],
            explain: "ANN indexes trade a little recall for a lot of speed.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "order",
            prompt: "Order the RAG steps at question time.",
            items: ["Embed the question", "Find the top-k similar chunks", "Put the chunks into the prompt", "Generate the answer"],
            explain: "Retrieve, then generate.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "choice",
            prompt: "Queries use ORDER BY embedding <=> $1. Which index opclass should the HNSW index use?",
            options: ["vector_l2_ops", "vector_ip_ops", "vector_cosine_ops", "text_pattern_ops"],
            answer: 2,
            explain: "The index only helps when its operator class matches the query's distance operator.",
          },
        },
      ],
    },
  ],
};
