-- =============================================================================
-- PostgreSQL test schema for Trino ODBC driver integration tests.
-- Loaded automatically by docker-entrypoint-initdb.d on first container start.
-- =============================================================================

-- ---------------------------------------------------------------------------
-- public schema: type coverage table
-- ---------------------------------------------------------------------------
CREATE TABLE types_test (
    id              INTEGER PRIMARY KEY,
    col_boolean     BOOLEAN,
    col_smallint    SMALLINT,
    col_integer     INTEGER,
    col_bigint      BIGINT,
    col_real        REAL,
    col_double      DOUBLE PRECISION,
    col_decimal     DECIMAL(10,2),
    col_varchar     VARCHAR(200),
    col_char        CHAR(10),
    col_text        TEXT,
    col_date        DATE,
    col_time        TIME,
    col_timestamp   TIMESTAMP,
    col_timestamptz TIMESTAMP WITH TIME ZONE,
    col_bytea       BYTEA,
    col_uuid        UUID,
    col_json        JSON,
    col_jsonb       JSONB
);
-- TODO: interval and array types (Trino supports them but ODBC mapping is complex)

-- Row 1: typical values
INSERT INTO types_test VALUES (
    1, true, 42, 100000, 9876543210, 3.14, 2.718281828,
    12345.67, 'hello world', 'fixed     ',
    'This is a longer text value for testing.',
    '2025-06-15', '14:30:00', '2025-06-15 14:30:00',
    '2025-06-15 14:30:00+02:00',
    E'\\xDEADBEEF',
    'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11',
    '{"key": "value", "num": 42}',
    '{"nested": {"array": [1, 2, 3]}}'
);

-- Row 2: boundary values
INSERT INTO types_test VALUES (
    2, false, -32768, -2147483648, 9223372036854775807, 1.17549e-38, 1.7976931348623157e+308,
    99999999.99, '', '          ',
    '',
    '1970-01-01', '00:00:00', '1970-01-01 00:00:00',
    '1970-01-01 00:00:00+00:00',
    E'\\x',
    '00000000-0000-0000-0000-000000000000',
    '{}',
    '[]'
);

-- Row 3: all NULLs except id
INSERT INTO types_test VALUES (
    3, NULL, NULL, NULL, NULL, NULL, NULL,
    NULL, NULL, NULL,
    NULL,
    NULL, NULL, NULL,
    NULL,
    NULL,
    NULL,
    NULL,
    NULL
);

-- Row 4: Unicode and special characters
INSERT INTO types_test VALUES (
    4, true, 1, 1, 1, 1.0, 1.0,
    0.01, '日本語テスト 🎉🦀 café résumé', 'Ünïcödé  ',
    E'Line1\nLine2\tTabbed',
    '2099-12-31', '23:59:59', '2099-12-31 23:59:59',
    '2099-12-31 23:59:59+00:00',
    E'\\xCAFEBABE',
    'ffffffff-ffff-ffff-ffff-ffffffffffff',
    '{"emoji": "🦀"}',
    '{"unicode": "日本語"}'
);

-- ---------------------------------------------------------------------------
-- public schema: relational tables for PK/FK/index testing
-- ---------------------------------------------------------------------------
CREATE TABLE customers (
    id    INTEGER PRIMARY KEY,
    name  VARCHAR(100) NOT NULL,
    email VARCHAR(200) UNIQUE
);

INSERT INTO customers VALUES (1, 'Alice', 'alice@example.com');
INSERT INTO customers VALUES (2, 'Bob', 'bob@example.com');
INSERT INTO customers VALUES (3, 'Charlie', 'charlie@example.com');

CREATE TABLE orders (
    id          INTEGER PRIMARY KEY,
    customer_id INTEGER NOT NULL REFERENCES customers(id)
                ON DELETE RESTRICT ON UPDATE CASCADE,
    order_date  DATE NOT NULL,
    amount      DECIMAL(12,2)
);
CREATE INDEX idx_orders_customer ON orders(customer_id);
CREATE INDEX idx_orders_date ON orders(order_date);

INSERT INTO orders VALUES (1, 1, '2025-01-15', 99.99);
INSERT INTO orders VALUES (2, 1, '2025-02-20', 149.50);
INSERT INTO orders VALUES (3, 2, '2025-03-10', 75.00);

-- Composite primary key
CREATE TABLE order_items (
    order_id   INTEGER NOT NULL REFERENCES orders(id)
               ON DELETE CASCADE,
    item_seq   INTEGER NOT NULL,
    product    VARCHAR(100) NOT NULL,
    quantity   INTEGER NOT NULL,
    unit_price DECIMAL(10,2),
    PRIMARY KEY (order_id, item_seq)
);

INSERT INTO order_items VALUES (1, 1, 'Widget', 2, 24.99);
INSERT INTO order_items VALUES (1, 2, 'Gadget', 1, 50.01);
INSERT INTO order_items VALUES (2, 1, 'Widget', 5, 24.99);

-- Self-referencing FK
CREATE TABLE categories (
    id        INTEGER PRIMARY KEY,
    name      VARCHAR(100) NOT NULL,
    parent_id INTEGER REFERENCES categories(id)
              ON DELETE SET NULL
);

INSERT INTO categories VALUES (1, 'Electronics', NULL);
INSERT INTO categories VALUES (2, 'Laptops', 1);
INSERT INTO categories VALUES (3, 'Phones', 1);

-- ---------------------------------------------------------------------------
-- testschema: for schema filtering tests
-- ---------------------------------------------------------------------------
CREATE SCHEMA testschema;

CREATE TABLE testschema.products (
    id          INTEGER PRIMARY KEY,
    name        VARCHAR(200) NOT NULL,
    price       DECIMAL(10,2),
    category_id INTEGER REFERENCES categories(id)
);

INSERT INTO testschema.products VALUES (1, 'MacBook Pro', 2499.99, 2);
INSERT INTO testschema.products VALUES (2, 'iPhone 15', 999.00, 3);
