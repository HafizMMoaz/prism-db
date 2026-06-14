"""A quick tour of the SDK against a running prismd.

    prismd run ./data 127.0.0.1:4444
    python examples/quickstart.py      # or set PRISM_HOST / PRISM_PORT / PRISM_USER / PRISM_PASSWORD
"""

import os

from prismdb import Client, Q


def main() -> None:
    db = Client.connect(
        host=os.environ.get("PRISM_HOST", "127.0.0.1"),
        port=int(os.environ.get("PRISM_PORT", "4444")),
        username=os.environ.get("PRISM_USER", "admin"),
        password=os.environ.get("PRISM_PASSWORD", "admin"),
    )
    with db:
        # --- SQL ---
        db.sql("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT, age BIGINT)")
        db.sql("INSERT INTO users VALUES (1,'alice',30),(2,'bob',25),(3,'carol',41)")
        db.sql("UPDATE users SET age = age + 1 WHERE id = 2")
        adults = db.sql("SELECT id, name, age FROM users WHERE age >= 30 ORDER BY age")
        print("adults:", adults.rows)

        # --- KV ---
        db.kv.put("sessions", "sid-1", "payload")
        print("kv get:", db.kv.get("sessions", "sid-1"))

        # --- Documents (with query operators) ---
        db.doc.insert_many("people", [
            {"name": "alice", "age": 30, "city": "NYC"},
            {"name": "bob", "age": 25, "city": "LA"},
            {"name": "carol", "age": 41, "city": "NYC"},
        ])
        ny_adults = db.doc.find("people", Q.and_(Q.eq("city", "NYC"), Q.gt("age", 30)))
        print("NYC adults:", ny_adults)

        # --- Transaction (atomic across models) ---
        db.begin()
        db.sql("INSERT INTO users VALUES (4,'dave',50)")
        db.kv.put("sessions", "sid-2", "tx")
        db.commit()
        print("user count:", db.sql("SELECT COUNT(*) FROM users").rows[0])


if __name__ == "__main__":
    main()
