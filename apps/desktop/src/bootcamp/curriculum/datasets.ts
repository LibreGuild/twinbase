// Seed scripts the labs reset their mock servers to.

export const SHOP_SQL = `DROP TABLE IF EXISTS order_items;
DROP TABLE IF EXISTS orders;
DROP TABLE IF EXISTS products;
DROP TABLE IF EXISTS customers;
CREATE TABLE customers (
  id integer PRIMARY KEY,
  name text NOT NULL,
  city text NOT NULL,
  joined date NOT NULL
);
CREATE TABLE products (
  id integer PRIMARY KEY,
  name text NOT NULL,
  category text NOT NULL,
  price numeric(8,2) NOT NULL
);
CREATE TABLE orders (
  id integer PRIMARY KEY,
  customer_id integer NOT NULL REFERENCES customers(id),
  ordered_at date NOT NULL,
  status text NOT NULL
);
CREATE TABLE order_items (
  order_id integer NOT NULL REFERENCES orders(id),
  product_id integer NOT NULL REFERENCES products(id),
  quantity integer NOT NULL,
  PRIMARY KEY (order_id, product_id)
);
INSERT INTO customers VALUES
  (1, 'Ada', 'London', '2024-01-12'),
  (2, 'Grace', 'New York', '2024-03-02'),
  (3, 'Linus', 'Helsinki', '2024-03-20'),
  (4, 'Margaret', 'Boston', '2024-06-08'),
  (5, 'Ken', 'New York', '2024-09-15'),
  (6, 'Barbara', 'Boston', '2025-01-04');
INSERT INTO products VALUES
  (1, 'Mechanical keyboard', 'hardware', 129.00),
  (2, 'USB-C hub', 'hardware', 39.50),
  (3, 'SQL pocket guide', 'books', 18.00),
  (4, 'Database internals', 'books', 54.00),
  (5, 'Coffee beans 1kg', 'pantry', 24.90),
  (6, 'Standing desk', 'furniture', 489.00);
INSERT INTO orders VALUES
  (101, 1, '2025-02-01', 'shipped'),
  (102, 2, '2025-02-03', 'shipped'),
  (103, 1, '2025-02-10', 'pending'),
  (104, 3, '2025-02-11', 'shipped'),
  (105, 5, '2025-02-14', 'cancelled'),
  (106, 4, '2025-02-20', 'pending');
INSERT INTO order_items VALUES
  (101, 1, 1), (101, 3, 2),
  (102, 4, 1), (102, 5, 3),
  (103, 2, 2),
  (104, 6, 1), (104, 3, 1),
  (105, 5, 1),
  (106, 4, 2), (106, 2, 1);`;

export const DOCS_SQL = `DROP TABLE IF EXISTS docs;
CREATE TABLE docs (
  id integer PRIMARY KEY,
  title text NOT NULL,
  embedding vector(3) NOT NULL
);
INSERT INTO docs VALUES
  (1, 'Resetting your password', '[0.9, 0.1, 0.0]'),
  (2, 'Changing your login email', '[0.8, 0.3, 0.1]'),
  (3, 'Refund policy', '[0.1, 0.9, 0.2]'),
  (4, 'Requesting a refund for a duplicate charge', '[0.2, 0.8, 0.3]'),
  (5, 'Shipping times by country', '[0.0, 0.2, 0.9]'),
  (6, 'Tracking your parcel', '[0.1, 0.3, 0.8]');`;
