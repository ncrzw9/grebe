CREATE TABLE customers (
    id BIGINT PRIMARY KEY,
    name VARCHAR NOT NULL,
    email VARCHAR UNIQUE,
    signed_up DATE
);

CREATE TABLE orders (
    id BIGINT PRIMARY KEY,
    customer_id BIGINT NOT NULL,
    total DECIMAL(10, 2),
    placed DATE,
    status VARCHAR
);

CREATE TABLE payments (
    id BIGINT PRIMARY KEY,
    order_id BIGINT,
    customer_id BIGINT NOT NULL,
    amount DECIMAL(10, 2)
);

CREATE TABLE refunds (
    id BIGINT PRIMARY KEY,
    customer_id BIGINT NOT NULL,
    amount DECIMAL(10, 2)
);

CREATE INDEX orders_placed ON orders (placed);

INSERT INTO customers
SELECT i, 'customer ' || i, 'c' || i || '@example.com', DATE '2020-01-01' + i::INT
FROM range(1, 1001) r(i);

INSERT INTO orders
SELECT i, 1 + i % 1000, (i % 500) + 0.99, DATE '2023-01-01' + (i % 730)::INT,
       CASE WHEN i % 10 = 0 THEN 'cancelled' ELSE 'shipped' END
FROM range(1, 2000001) r(i);

-- Nine orders in ten are paid; one payment is tied to no order.
INSERT INTO payments
SELECT i, i, 1 + i % 1000, (i % 500) + 0.99
FROM range(1, 2000001) r(i)
WHERE i % 10 <> 7;

INSERT INTO payments VALUES (0, NULL, 1, 5.00);

INSERT INTO refunds
SELECT i, 1 + i % 1000, 1.00
FROM range(1, 3001) r(i);
