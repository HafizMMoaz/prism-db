// A quick tour of the SDK against a running prismd.
//
//   prismd run ./data 127.0.0.1:4444
//   npm run example            # or: PRISM_HOST=… PRISM_PORT=… node dist/examples/quickstart.js

import { Client, ObjectId, Q } from "../src/index.js";

async function main(): Promise<void> {
  const client = await Client.connect({
    host: process.env.PRISM_HOST ?? "127.0.0.1",
    port: Number(process.env.PRISM_PORT ?? 4444),
    username: process.env.PRISM_USER ?? "admin",
    password: process.env.PRISM_PASSWORD ?? "admin",
  });

  // --- SQL ---
  await client.sql("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT, age BIGINT)");
  await client.sql("INSERT INTO users VALUES (1,'alice',30),(2,'bob',25),(3,'carol',41)");
  await client.sql("UPDATE users SET age = age + 1 WHERE id = 2");
  const adults = await client.sql("SELECT id, name, age FROM users WHERE age >= 30 ORDER BY age");
  console.log("adults:", adults.rows);
  const stats = await client.sql("SELECT COUNT(*), MAX(age) FROM users");
  console.log("stats:", stats.rows[0]);

  // --- KV ---
  await client.kv.put("sessions", "sid-1", "payload");
  console.log("kv get:", (await client.kv.get("sessions", "sid-1"))?.toString());

  // --- Documents (with query operators) ---
  await client.doc.insertMany("people", [
    { name: "alice", age: 30, city: "NYC" },
    { name: "bob", age: 25, city: "LA" },
    { name: "carol", age: 41, city: "NYC" },
  ]);
  const nyAdults = await client.doc.find("people", Q.and(Q.eq("city", "NYC"), Q.gt("age", 30)));
  console.log("NYC adults:", nyAdults);

  // --- Transaction (atomic across models) ---
  await client.begin();
  await client.sql("INSERT INTO users VALUES (4,'dave',50)");
  await client.kv.put("sessions", "sid-2", "tx");
  await client.commit();
  console.log("after commit, user count:", (await client.sql("SELECT COUNT(*) FROM users")).rows[0]);

  client.close();
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
