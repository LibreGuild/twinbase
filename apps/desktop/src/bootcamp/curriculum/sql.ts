import type { Chapter } from "../types";
import { SHOP_SQL } from "./datasets";

export const sql: Chapter = {
  id: "sql",
  title: "SQL Essentials",
  subtitle: "Ask questions of your data: filter, sort, aggregate, join and change rows.",
  color: "#5b8fd9",
  badge: "sql",
  lessons: [
    {
      id: "select-where",
      title: "SELECT and WHERE",
      summary: "Read the columns you need, keep the rows you want.",
      minutes: 6,
      steps: [
        {
          kind: "read",
          title: "Your first query",
          body: `SQL is **declarative**: you describe *what* you want, the database figures out *how*.

\`\`\`sql
SELECT name, city        -- which columns
FROM customers           -- from which table
WHERE city = 'Boston';   -- which rows
\`\`\`

- \`SELECT *\` returns every column (handy while exploring, avoid in app code).
- Text goes in **single quotes**; \`"double quotes"\` are for identifiers.
- Combine conditions with \`AND\`, \`OR\`, \`NOT\`; match patterns with \`LIKE 'Ada%'\`; test ranges with \`BETWEEN\` and sets with \`IN (...)\`.`,
        },
        {
          kind: "read",
          title: "NULL is not a value",
          body: `\`NULL\` means *unknown*. Comparisons with it are unknown too, so \`WHERE phone = NULL\` matches **nothing**. Use:

\`\`\`sql
WHERE phone IS NULL
WHERE phone IS NOT NULL
\`\`\``,
        },
        {
          kind: "quiz",
          q: {
            type: "blank",
            prompt: "To keep only the rows from New York you add: ___ city = 'New York'",
            answers: ["where"],
            explain: "WHERE filters rows before they are returned.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "choice",
            prompt: "Which query finds customers without an email address?",
            options: ["WHERE email = NULL", "WHERE email IS NULL", "WHERE email = ''", "WHERE NOT email"],
            answer: 1,
            explain: "NULL never equals anything (not even NULL); use IS NULL.",
          },
        },
        {
          kind: "lab",
          lab: {
            engine: "postgres",
            title: "Customers in Boston and New York",
            task: "The `customers` table is ready. Return the **name** and **city** of every customer who lives in **Boston** or **New York**.",
            setup: SHOP_SQL,
            starter: "SELECT name, city\nFROM customers\nWHERE ",
            hint: "Use `city IN ('Boston', 'New York')` — or two conditions joined with OR.",
            solution: "SELECT name, city FROM customers WHERE city IN ('Boston', 'New York');",
            check: { kind: "result" },
          },
        },
      ],
    },
    {
      id: "sort-aggregate",
      title: "Sorting and aggregating",
      summary: "ORDER BY, LIMIT, COUNT/SUM/AVG and GROUP BY.",
      minutes: 7,
      steps: [
        {
          kind: "read",
          title: "Order and limit",
          body: `Rows come back in **no guaranteed order** unless you ask:

\`\`\`sql
SELECT name, price FROM products
ORDER BY price DESC   -- highest first
LIMIT 3;              -- only the top three
\`\`\``,
        },
        {
          kind: "read",
          title: "Aggregates and GROUP BY",
          body: `Aggregate functions squash many rows into one value: \`COUNT(*)\`, \`SUM(x)\`, \`AVG(x)\`, \`MIN(x)\`, \`MAX(x)\`.

\`GROUP BY\` makes one result row **per group**:

\`\`\`sql
SELECT category, COUNT(*) AS products, AVG(price) AS avg_price
FROM products
GROUP BY category;
\`\`\`

Filter *groups* with \`HAVING\` (WHERE runs before grouping, HAVING after):

\`\`\`sql
... GROUP BY category HAVING COUNT(*) > 1;
\`\`\``,
        },
        {
          kind: "quiz",
          q: {
            type: "order",
            prompt: "Put the clauses in the order you write them.",
            items: ["SELECT", "FROM", "WHERE", "GROUP BY", "HAVING", "ORDER BY", "LIMIT"],
            explain: "SELECT … FROM … WHERE … GROUP BY … HAVING … ORDER BY … LIMIT.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "choice",
            prompt: "You want categories whose total stock value is over 1000. Where does the condition go?",
            options: ["WHERE SUM(price) > 1000", "HAVING SUM(price) > 1000", "ORDER BY SUM(price) > 1000", "GROUP BY SUM(price) > 1000"],
            answer: 1,
            explain: "Conditions on aggregates belong in HAVING, which runs after grouping.",
          },
        },
        {
          kind: "lab",
          lab: {
            engine: "postgres",
            title: "Orders per status",
            task: "Count the orders in each **status**. Return two columns, `status` and `orders`, sorted by `orders` from most to fewest (ties by status A→Z).",
            setup: SHOP_SQL,
            starter: "SELECT status, COUNT(*) AS orders\nFROM orders\n",
            hint: "Add `GROUP BY status` and then `ORDER BY orders DESC, status`.",
            solution: "SELECT status, COUNT(*) AS orders FROM orders GROUP BY status ORDER BY orders DESC, status;",
            check: { kind: "result" },
          },
        },
      ],
    },
    {
      id: "joins",
      title: "Joining tables",
      summary: "INNER, LEFT, RIGHT and FULL joins, visually.",
      minutes: 8,
      steps: [
        {
          kind: "read",
          title: "Putting rows back together",
          body: `Normalised data is spread across tables. A **JOIN** matches rows using a condition, usually *foreign key = primary key*:

\`\`\`sql
SELECT o.id, c.name, o.status
FROM orders o
JOIN customers c ON c.id = o.customer_id;
\`\`\`

Try the join types below. Watch which rows survive when a customer has no orders, or an order has no matching customer.`,
          widget: "join-explorer",
        },
        {
          kind: "quiz",
          q: {
            type: "choice",
            prompt: "You need every customer, including those who never ordered. Which join from customers to orders?",
            options: ["INNER JOIN", "LEFT JOIN", "CROSS JOIN", "SELF JOIN"],
            answer: 1,
            explain: "LEFT JOIN keeps every row of the left table; missing matches become NULLs.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "match",
            prompt: "Match the join to what it keeps.",
            pairs: [
              ["INNER", "Only matching pairs"],
              ["LEFT", "All left rows + matches"],
              ["FULL", "All rows from both sides"],
              ["CROSS", "Every combination"],
            ],
            explain: "CROSS JOIN has no condition: rows × rows.",
          },
        },
        {
          kind: "lab",
          lab: {
            engine: "postgres",
            title: "Revenue per customer",
            task: "For each customer who has at least one **shipped** order, return `name` and `revenue` = the sum of `quantity * price` over their shipped orders. Sort by revenue, highest first.",
            setup: SHOP_SQL,
            starter:
              "SELECT c.name, SUM(oi.quantity * p.price) AS revenue\nFROM customers c\nJOIN orders o ON o.customer_id = c.id\nJOIN order_items oi ON oi.order_id = o.id\nJOIN products p ON p.id = oi.product_id\n",
            hint: "Filter with `WHERE o.status = 'shipped'`, then `GROUP BY c.name ORDER BY revenue DESC`.",
            solution:
              "SELECT c.name, SUM(oi.quantity * p.price) AS revenue FROM customers c JOIN orders o ON o.customer_id = c.id JOIN order_items oi ON oi.order_id = o.id JOIN products p ON p.id = oi.product_id WHERE o.status = 'shipped' GROUP BY c.name ORDER BY revenue DESC;",
            check: { kind: "result" },
          },
        },
      ],
    },
    {
      id: "changing-data",
      title: "Changing data safely",
      summary: "INSERT, UPDATE and DELETE — and how not to wipe a table.",
      minutes: 6,
      steps: [
        {
          kind: "read",
          title: "Writes",
          body: `\`\`\`sql
INSERT INTO products (id, name, category, price) VALUES (7, 'Mouse', 'hardware', 25);
UPDATE products SET price = price * 0.9 WHERE category = 'books';
DELETE FROM orders WHERE status = 'cancelled';
\`\`\`

⚠️ An \`UPDATE\` or \`DELETE\` **without WHERE** touches *every* row. TwinBase warns you before running one, and its **change preview** runs the statement on a local twin first so you can see exactly which rows change.`,
        },
        {
          kind: "quiz",
          q: {
            type: "truefalse",
            prompt: "DELETE FROM orders; removes only the most recent order.",
            answer: false,
            explain: "Without WHERE it deletes every row in the table.",
          },
        },
        {
          kind: "quiz",
          q: {
            type: "blank",
            prompt: "UPDATE products ___ price = 10 WHERE id = 3;",
            answers: ["set"],
            explain: "UPDATE table SET column = value WHERE condition.",
          },
        },
        {
          kind: "lab",
          lab: {
            engine: "postgres",
            title: "Clean up and discount",
            task: "Two changes:\n\n1. Delete the **cancelled** order. It has rows in `order_items`, so delete those first.\n2. Give every **book** a 10% discount (`price * 0.9`).",
            setup: SHOP_SQL,
            starter: "DELETE FROM order_items WHERE order_id IN (SELECT id FROM orders WHERE status = 'cancelled');\n",
            hint: "Then `DELETE FROM orders WHERE status = 'cancelled';` and `UPDATE products SET price = price * 0.9 WHERE category = 'books';`",
            solution:
              "DELETE FROM order_items WHERE order_id IN (SELECT id FROM orders WHERE status = 'cancelled');\nDELETE FROM orders WHERE status = 'cancelled';\nUPDATE products SET price = price * 0.9 WHERE category = 'books';",
            check: {
              kind: "state",
              query: "SELECT (SELECT COUNT(*) FROM orders WHERE status = 'cancelled') AS cancelled, (SELECT SUM(price) FROM products WHERE category = 'books') AS books",
              expect: "0|64.8",
            },
          },
        },
      ],
    },
  ],
};
