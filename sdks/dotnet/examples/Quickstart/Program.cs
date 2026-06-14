// A quick tour of the SDK against a running prismd.
//
//   prismd run ./data 127.0.0.1:4444
//   dotnet run --project examples/Quickstart
//   (or set PRISM_HOST / PRISM_PORT / PRISM_USER / PRISM_PASSWORD)

using System;
using System.Collections.Generic;
using PrismDb;

string Env(string k, string fallback) => Environment.GetEnvironmentVariable(k) ?? fallback;

using var db = Client.Connect(
    host: Env("PRISM_HOST", "127.0.0.1"),
    port: int.Parse(Env("PRISM_PORT", "4444")),
    username: Env("PRISM_USER", "admin"),
    password: Env("PRISM_PASSWORD", "admin"));

// --- SQL ---
db.Sql("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT, age BIGINT)");
db.Sql("INSERT INTO users VALUES (1,'alice',30),(2,'bob',25),(3,'carol',41)");
db.Sql("UPDATE users SET age = age + 1 WHERE id = 2");
var adults = db.Sql("SELECT id, name, age FROM users WHERE age >= 30 ORDER BY age");
foreach (var row in adults.Rows)
    Console.WriteLine($"  {row["name"]} ({row["age"]})");

// --- KV ---
db.Kv.Put("sessions", "sid-1", "payload");
Console.WriteLine("kv get: " + db.Kv.GetString("sessions", "sid-1"));

// --- Documents (with query operators) ---
db.Doc.InsertMany("people", new List<IDictionary<string, object?>>
{
    new Document { ["name"] = "alice", ["age"] = 30L, ["city"] = "NYC" },
    new Document { ["name"] = "bob", ["age"] = 25L, ["city"] = "LA" },
    new Document { ["name"] = "carol", ["age"] = 41L, ["city"] = "NYC" },
});
var nyAdults = db.Doc.Find("people", Q.And(Q.Eq("city", "NYC"), Q.Gt("age", 30L)));
Console.WriteLine($"NYC adults: {nyAdults.Count}");

// --- Transaction (atomic across models) ---
db.Begin();
db.Sql("INSERT INTO users VALUES (4,'dave',50)");
db.Kv.Put("sessions", "sid-2", "tx");
db.Commit();
var countRow = db.Sql("SELECT COUNT(*) FROM users").Raw[0];
Console.WriteLine("user count: " + countRow[0]);
