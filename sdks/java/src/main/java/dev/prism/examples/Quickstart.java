package dev.prism.examples;

import dev.prism.client.Client;
import dev.prism.client.Document;
import dev.prism.client.Q;
import dev.prism.client.ServerException;
import dev.prism.client.SqlResult;
import java.util.Arrays;
import java.util.List;
import java.util.Map;

/**
 * A quick tour of the SDK against a running prismd.
 *
 * <pre>
 *   prismd run ./data 127.0.0.1:4444
 *   mvn -q compile
 *   java -cp target/classes dev.prism.examples.Quickstart
 *   (or set PRISM_HOST / PRISM_PORT / PRISM_USER / PRISM_PASSWORD)
 * </pre>
 */
public final class Quickstart {
    private static String env(String key, String fallback) {
        String v = System.getenv(key);
        return (v == null || v.isEmpty()) ? fallback : v;
    }

    public static void main(String[] args) {
        try (Client db = Client.builder()
                .host(env("PRISM_HOST", "127.0.0.1"))
                .port(Integer.parseInt(env("PRISM_PORT", "4444")))
                .username(env("PRISM_USER", "admin"))
                .password(env("PRISM_PASSWORD", "admin"))
                .connect()) {

            // --- SQL ---
            db.sql("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT, age BIGINT)");
            db.sql("INSERT INTO users VALUES (1,'alice',30),(2,'bob',25),(3,'carol',41)");
            db.sql("UPDATE users SET age = age + 1 WHERE id = 2");
            SqlResult adults = db.sql("SELECT id, name, age FROM users WHERE age >= 30 ORDER BY age");
            for (Map<String, Object> row : adults.rows) {
                System.out.println("  " + row.get("name") + " (" + row.get("age") + ")");
            }

            // --- KV ---
            db.kv.put("sessions", "sid-1", "payload");
            System.out.println("kv get: " + db.kv.getString("sessions", "sid-1"));

            // --- Documents (with query operators) ---
            db.doc.insertMany("people", Arrays.asList(
                    new Document().set("name", "alice").set("age", 30L).set("city", "NYC"),
                    new Document().set("name", "bob").set("age", 25L).set("city", "LA"),
                    new Document().set("name", "carol").set("age", 41L).set("city", "NYC")));
            List<Document> ny = db.doc.find("people", Q.and(Q.eq("city", "NYC"), Q.gt("age", 30L)));
            System.out.println("NYC adults: " + ny.size());

            // --- Transaction (atomic across models) ---
            db.begin();
            db.sql("INSERT INTO users VALUES (4,'dave',50)");
            db.kv.put("sessions", "sid-2", "tx");
            db.commit();
            System.out.println("user count: " + db.sql("SELECT COUNT(*) FROM users").raw.get(0)[0]);

        } catch (ServerException e) {
            System.err.println("server error " + e.code + " (" + e.sqlstate + "): " + e.getMessage());
            System.exit(1);
        }
    }
}
