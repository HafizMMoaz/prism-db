// End-to-end tests against a real prismd. Gated on PRISM_E2E_ADDR (host:port);
// skipped otherwise so `npm test` needs no server. Start a server first, e.g.:
//
//   cargo run -q -p prism-server --bin prismd -- run ./data 127.0.0.1:4455
//   PRISM_E2E_ADDR=127.0.0.1:4455 npm test

import assert from "node:assert/strict";
import { test } from "node:test";

import { Client, ObjectId, Q } from "../src/index.js";

const addr = process.env.PRISM_E2E_ADDR;
const [host, portStr] = (addr ?? "").split(":");
const port = Number(portStr);

const opts = {
  host,
  port,
  username: process.env.PRISM_USER ?? "admin",
  password: process.env.PRISM_PASSWORD ?? "admin",
};

// A unique suffix so reruns against the same server don't collide.
const sfx = Date.now().toString(36);

test("e2e: SQL CRUD + aggregates", { skip: !addr }, async () => {
  const c = await Client.connect(opts);
  try {
    const t = `t_${sfx}`;
    await c.sql(`CREATE TABLE ${t} (id BIGINT PRIMARY KEY, name TEXT, age BIGINT)`);
    const ins = await c.sql(`INSERT INTO ${t} VALUES (1,'alice',30),(2,'bob',25),(3,'carol',41)`);
    assert.equal(ins.affectedRows, 3n);

    const upd = await c.sql(`UPDATE ${t} SET age = age + 1 WHERE id = 2`);
    assert.equal(upd.affectedRows, 1n);

    const sel = await c.sql(`SELECT id, name, age FROM ${t} WHERE age >= 30 ORDER BY age`);
    assert.deepEqual(
      sel.rows.map((r) => r.name),
      ["alice", "carol"],
    );
    assert.equal(sel.rows[0]!.age, 30n);

    const agg = await c.sql(`SELECT COUNT(*), MAX(age) FROM ${t}`);
    assert.equal(agg.raw[0]![0], 3n);
    assert.equal(agg.raw[0]![1], 41n);

    const del = await c.sql(`DELETE FROM ${t} WHERE id = 1`);
    assert.equal(del.affectedRows, 1n);
  } finally {
    c.close();
  }
});

test("e2e: KV round-trip", { skip: !addr }, async () => {
  const c = await Client.connect(opts);
  try {
    const ns = `kv_${sfx}`;
    await c.kv.put(ns, "k1", "hello");
    assert.equal((await c.kv.get(ns, "k1"))?.toString(), "hello");
    assert.equal(await c.kv.get(ns, "missing"), null);
    await c.kv.delete(ns, "k1");
    assert.equal(await c.kv.get(ns, "k1"), null);
  } finally {
    c.close();
  }
});

test("e2e: documents with query operators", { skip: !addr }, async () => {
  const c = await Client.connect(opts);
  try {
    const coll = `people_${sfx}`;
    const ids = await c.doc.insertMany(coll, [
      { name: "alice", age: 30, city: "NYC" },
      { name: "bob", age: 25, city: "LA" },
      { name: "carol", age: 41, city: "NYC" },
    ]);
    assert.equal(ids.length, 3);
    assert.ok(ids[0] instanceof ObjectId);

    const all = await c.doc.find(coll);
    assert.equal(all.length, 3);

    const nyAdults = await c.doc.find(coll, Q.and(Q.eq("city", "NYC"), Q.gt("age", 30)));
    assert.deepEqual(
      nyAdults.map((d) => d.name),
      ["carol"],
    );

    const inSet = await c.doc.find(coll, Q.in("name", ["alice", "bob"]));
    assert.equal(inSet.length, 2);

    const n = await c.doc.deleteMany(coll, Q.lt("age", 31));
    assert.equal(n, 2n); // alice (30) + bob (25)
  } finally {
    c.close();
  }
});

test("e2e: transaction commit and abort across models", { skip: !addr }, async () => {
  const c = await Client.connect(opts);
  try {
    const t = `tx_${sfx}`;
    const ns = `txkv_${sfx}`;
    await c.sql(`CREATE TABLE ${t} (id BIGINT PRIMARY KEY)`);

    // Commit: both writes persist.
    await c.begin();
    await c.sql(`INSERT INTO ${t} VALUES (1)`);
    await c.kv.put(ns, "a", "1");
    await c.commit();
    assert.equal((await c.sql(`SELECT COUNT(*) FROM ${t}`)).raw[0]![0], 1n);
    assert.equal((await c.kv.get(ns, "a"))?.toString(), "1");

    // Abort: neither write persists.
    await c.begin();
    await c.sql(`INSERT INTO ${t} VALUES (2)`);
    await c.kv.put(ns, "b", "2");
    await c.abort();
    assert.equal((await c.sql(`SELECT COUNT(*) FROM ${t}`)).raw[0]![0], 1n);
    assert.equal(await c.kv.get(ns, "b"), null);
  } finally {
    c.close();
  }
});

test("e2e: ping keep-alive", { skip: !addr }, async () => {
  const c = await Client.connect(opts);
  try {
    await c.ping();
  } finally {
    c.close();
  }
});
