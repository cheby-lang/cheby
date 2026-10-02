-- The database the shop service expects, with the products and codes
-- used in the examples of `src/main.cheby`. Prices are in cents.

CREATE TABLE products (
  id          INTEGER PRIMARY KEY,
  name        TEXT    NOT NULL,
  price_cents INTEGER NOT NULL CHECK (price_cents > 0),
  stock       INTEGER NOT NULL CHECK (stock >= 0)
);

-- A code takes either a percentage or a fixed amount off, and only from
-- a subtotal of at least `minimum_cents`.
CREATE TABLE discount_codes (
  code          TEXT    PRIMARY KEY,
  percent_off   INTEGER,
  cents_off     INTEGER,
  minimum_cents INTEGER NOT NULL,
  CHECK ((percent_off IS NULL) <> (cents_off IS NULL))
);

CREATE TABLE orders (
  id             SERIAL  PRIMARY KEY,
  email          TEXT    NOT NULL,
  subtotal_cents INTEGER NOT NULL,
  discount_cents INTEGER NOT NULL,
  shipping_cents INTEGER NOT NULL,
  total_cents    INTEGER NOT NULL
);

CREATE TABLE order_lines (
  order_id   INTEGER NOT NULL REFERENCES orders (id),
  product_id INTEGER NOT NULL REFERENCES products (id),
  quantity   INTEGER NOT NULL,
  unit_cents INTEGER NOT NULL,
  PRIMARY KEY (order_id, product_id)
);

INSERT INTO products (id, name, price_cents, stock) VALUES
  (1, 'Green tea', 450, 40),
  (2, 'Teapot', 3200, 3),
  (3, 'Mug', 1195, 12),
  (4, 'Honey', 875, 0);

INSERT INTO discount_codes (code, percent_off, cents_off, minimum_cents) VALUES
  ('SPRING10', 10, NULL, 3000),
  ('WELCOME5', NULL, 500, 2000);
