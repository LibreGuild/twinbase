import type { Chapter } from "../types";

export const caching: Chapter = {
  id: "caching",
  title: "Caching with Redis",
  subtitle: "Key-value thinking, Redis data structures, TTLs, cache-aside and the classic pitfalls.",
  color: "#df5a49",
  badge: "cache",
  lessons: [
    {
      id: "redis-structures",
      title: "Redis data structures",
      summary: "Strings, hashes, lists, sets and sorted sets — and when to use each.",
      minutes: 7,
      steps: [
        {
          kind: "read",
          title: "A server of data structures",
          body: `Redis keeps everything **in memory** and answers in microseconds. Every key holds one typed value:

| Type | Think | Example commands |
| --- | --- | --- |
| String | a value or counter | \`SET\`, \`GET\`, \`INCR\` |
| Hash | a small object | \`HSET user:1 name Ada\`, \`HGETALL\` |
| List | a queue / timeline | \`LPUSH\`, \`RPOP\`, \`LRANGE\` |
| Set | unique members | \`SADD\`, \`SISMEMBER\` |
| Sorted set | members ranked by score | \`ZADD\`, \`ZREVRANGE\` |

Keys are usually namespaced with colons — \`user:42:cart\` — which TwinBase shows as folders.`,
        },
        {
          kind: "quiz",
          q: {
            type: "match",
            prompt: "Pick the Redis type for each job.",
            pairs: [
              ["Page view counter", "String (INCR)"],
              ["User profile fields", "Hash"],
              ["Game leaderboard", "Sorted set"],
              ["Who liked a post (no duplicates)", "Set"],
            ],
            explain: "Choosing the right structure keeps operations O(1) or O(log n).",
          },
        },
        {
          kind: "lab",
          lab: {
            engine: "redis",
            title: "Store a user as a hash",
            task: "Create a hash `user:1` with fields `name` = **Ada** and `lang` = **sql**, then increment the counter `stats:visits` (it starts empty) **twice**.",
            setup: "FLUSHDB",
            starter: "HSET user:1 name Ada lang sql\n",
            hint: "`INCR stats:visits` creates the key at 0 and adds one — run it two times.",
            solution: "HSET user:1 name Ada lang sql\nINCR stats:visits\nINCR stats:visits",
            check: { kind: "state", query: "HGET user:1 lang\nGET stats:visits", expect: "sql|2" },
          },
        },
        {
          kind: "lab",
          lab: {
            engine: "redis",
            title: "Leaderboard",
            task: "Add three players to the sorted set `leaderboard`: **ada** with 320 points, **linus** with 150 and **grace** with 410. Then ask for the top player with `ZREVRANGE leaderboard 0 0`.",
            setup: "FLUSHDB",
            starter: "ZADD leaderboard 320 ada\n",
            hint: "`ZADD leaderboard 320 ada 150 linus 410 grace` adds all three at once.",
            solution: "ZADD leaderboard 320 ada 150 linus 410 grace\nZREVRANGE leaderboard 0 0",
            check: { kind: "state", query: "ZREVRANGE leaderboard 0 0\nZCARD leaderboard", expect: '["grace"]|3' },
          },
        },
      ],
    },
    {
      id: "ttl-cache-aside",
      title: "TTLs and cache-aside",
      summary: "Expiry, eviction and the most common caching pattern.",
      minutes: 7,
      steps: [
        {
          kind: "read",
          title: "Cache-aside",
          body: `The app checks the cache first and only goes to the database on a **miss**:

\`\`\`mermaid
sequenceDiagram
  participant App
  participant Redis
  participant DB as PostgreSQL
  App->>Redis: GET product:7
  alt hit
    Redis-->>App: cached JSON
  else miss
    App->>DB: SELECT … WHERE id = 7
    DB-->>App: row
    App->>Redis: SET product:7 json EX 300
  end
\`\`\`

A **TTL** (\`EX 300\`) bounds staleness: after five minutes the entry expires and the next read refreshes it. When memory is full, Redis **evicts** keys according to \`maxmemory-policy\` (e.g. \`allkeys-lru\`).

Play with the simulator: watch hit rate climb as the cache warms up, and drop when the TTL is short.`,
          widget: "cache-sim",
        },
        {
          kind: "quiz",
          q: {
            type: "order",
            prompt: "Order a cache-aside read that misses.",
            items: ["Look up the key in Redis", "Miss: query the database", "Write the result to Redis with a TTL", "Return the data"],
            explain: "Read-through on miss, then populate the cache.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "choice",
            prompt: "Memory is full and the policy is allkeys-lru. Which key is evicted first?",
            options: ["The newest key", "The least recently used key", "The largest key", "A random key"],
            answer: 1,
            explain: "LRU = least recently used.",
          },
        },
        {
          kind: "lab",
          lab: {
            engine: "redis",
            title: "Cache with an expiry",
            task: "Cache the session `session:42` with the value **ken** so it expires after **60 seconds**.",
            setup: "FLUSHDB",
            starter: "SET session:42 ken ",
            hint: "`SET key value EX 60` sets the value and the TTL in one atomic command.",
            solution: "SET session:42 ken EX 60",
            check: { kind: "state", query: "GET session:42\nTTL session:42", expect: "/^ken\\|([1-9]|[1-5][0-9]|60)$/" },
          },
        },
      ],
    },
    {
      id: "cache-pitfalls",
      title: "Caching pitfalls",
      summary: "Invalidation, stampedes and hot keys.",
      minutes: 6,
      steps: [
        {
          kind: "read",
          title: "The two hard things",
          body: `> There are only two hard things in computer science: cache invalidation and naming things. — Phil Karlton

- **Stale data**: the DB changed but the cache didn't. Delete (or update) the key when you write, and keep TTLs as a safety net.
- **Cache stampede**: a hot key expires and 1,000 requests hit the database at once. Fix with a lock (\`SET lock NX EX 5\`) so one request refreshes, or refresh *before* expiry, and add random jitter to TTLs.
- **Hot key**: one key gets most traffic. Replicate it, or keep a tiny in-process cache in front.
- **Cache penetration**: requests for keys that don't exist always miss. Cache the "not found" briefly too.`,
        },
        {
          kind: "quiz",
          q: {
            type: "match",
            prompt: "Match the problem to a fix.",
            pairs: [
              ["Stampede on expiry", "Lock with SET NX + TTL jitter"],
              ["Stale value after UPDATE", "Delete the key on write"],
              ["Lookups for missing ids", "Cache the miss briefly"],
              ["One very hot key", "Local in-process cache"],
            ],
            explain: "Each failure mode has a standard countermeasure.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "truefalse",
            prompt: "Giving every key exactly the same TTL helps avoid stampedes.",
            answer: false,
            explain: "Identical TTLs make keys expire together; add random jitter.",
          },
        },
      ],
    },
    {
      id: "redis-beyond-cache",
      title: "Redis beyond caching",
      summary: "Rate limits, queues, pub/sub and locks.",
      minutes: 5,
      steps: [
        {
          kind: "read",
          title: "Building blocks",
          body: `- **Rate limiting**: \`INCR rate:ip:1.2.3.4:minute\` + \`EXPIRE\`; reject above a threshold.
- **Queues**: \`LPUSH jobs …\` / \`BRPOP jobs\`, or **Streams** (\`XADD\`, consumer groups) for durable, replayable queues.
- **Pub/Sub**: \`PUBLISH chat:1 "hi"\` reaches every current subscriber (fire-and-forget).
- **Distributed lock**: \`SET lock:report <token> NX PX 30000\`; release only if the token is yours.

Redis is single-threaded per command, so each command is **atomic**; \`MULTI/EXEC\` and Lua scripts make several commands atomic together.`,
        },
        {
          kind: "quiz",
          q: {
            type: "choice",
            prompt: "Which command only sets a key if it doesn't already exist — the basis of a simple lock?",
            options: ["SET key value XX", "SET key value NX", "SETRANGE", "GETSET"],
            answer: 1,
            explain: "NX = only if Not eXists; XX = only if it exists.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "truefalse",
            prompt: "A Pub/Sub message is delivered to subscribers who connect after it was published.",
            answer: false,
            explain: "Pub/Sub is fire-and-forget; use Streams when messages must be kept.",
          },
        },
      ],
    },
  ],
};
