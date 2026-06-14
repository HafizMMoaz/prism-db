// A quick tour of the C++ SDK against a running prismd.
//
//   prismd run ./data 127.0.0.1:4444
//   make example && ./quickstart
//   (or set PRISM_HOST / PRISM_PORT / PRISM_USER / PRISM_PASSWORD)

#include "prism/prism.hpp"

#include <cstdlib>
#include <iostream>

static std::string env(const char* k, const char* fallback) {
    const char* v = std::getenv(k);
    return (v && *v) ? v : fallback;
}

int main() {
    prism::Options opts;
    opts.host = env("PRISM_HOST", "127.0.0.1");
    opts.port = std::stoi(env("PRISM_PORT", "4444"));
    opts.username = env("PRISM_USER", "admin");
    opts.password = env("PRISM_PASSWORD", "admin");

    try {
        prism::Client db = prism::Client::connect(opts);

        // --- SQL ---
        db.sql("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT, age BIGINT)");
        db.sql("INSERT INTO users VALUES (1,'alice',30),(2,'bob',25),(3,'carol',41)");
        db.sql("UPDATE users SET age = age + 1 WHERE id = 2");
        auto adults = db.sql("SELECT id, name, age FROM users WHERE age >= 30 ORDER BY age");
        int nameCol = adults.column("name"), ageCol = adults.column("age");
        for (const auto& row : adults.rows)
            std::cout << "  " << row[nameCol].asString() << " (" << row[ageCol].asInt64() << ")\n";

        // --- KV ---
        db.kvPut("sessions", "sid-1", "payload");
        if (auto v = db.kvGet("sessions", "sid-1"))
            std::cout << "kv get: " << std::string(v->begin(), v->end()) << "\n";

        // --- Documents (with query operators) ---
        db.insertMany("people", {
            prism::Document{{"name", "alice"}, {"age", std::int64_t(30)}, {"city", "NYC"}},
            prism::Document{{"name", "bob"}, {"age", std::int64_t(25)}, {"city", "LA"}},
            prism::Document{{"name", "carol"}, {"age", std::int64_t(41)}, {"city", "NYC"}},
        });
        auto nyAdults = db.find("people",
            prism::Query::and_(prism::Query::eq("city", "NYC"), prism::Query::gt("age", std::int64_t(30))));
        std::cout << "NYC adults: " << nyAdults.size() << "\n";

        // --- Transaction (atomic across models) ---
        db.begin();
        db.sql("INSERT INTO users VALUES (4,'dave',50)");
        db.kvPut("sessions", "sid-2", "tx");
        db.commit();
        auto count = db.sql("SELECT COUNT(*) FROM users");
        std::cout << "user count: " << count.rows[0][0].asInt64() << "\n";
    } catch (const prism::ServerError& e) {
        std::cerr << "server error " << e.info.code << " (" << e.info.sqlstate << "): " << e.what() << "\n";
        return 1;
    } catch (const prism::Error& e) {
        std::cerr << "error: " << e.what() << "\n";
        return 1;
    }
    return 0;
}
