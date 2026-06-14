<?php

declare(strict_types=1);

// A quick tour of the SDK against a running prismd.
//
//   prismd run ./data 127.0.0.1:4444
//   php examples/quickstart.php
//   (or set PRISM_HOST / PRISM_PORT / PRISM_USER / PRISM_PASSWORD)

namespace PrismDb;

$src = \dirname(__DIR__) . '/src';
foreach (['Errors', 'Codec', 'Value', 'Document', 'Query', 'Update', 'Messages', 'Connection', 'Client'] as $f) {
    require "{$src}/{$f}.php";
}

$db = Client::connect(
    host: \getenv('PRISM_HOST') ?: '127.0.0.1',
    port: (int) (\getenv('PRISM_PORT') ?: 4444),
    username: \getenv('PRISM_USER') ?: 'admin',
    password: \getenv('PRISM_PASSWORD') ?: 'admin',
);

try {
    // --- SQL ---
    $db->sql('CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT, age BIGINT)');
    $db->sql("INSERT INTO users VALUES (1,'alice',30),(2,'bob',25),(3,'carol',41)");
    $db->sql('UPDATE users SET age = age + 1 WHERE id = 2');
    $adults = $db->sql('SELECT id, name, age FROM users WHERE age >= 30 ORDER BY age');
    foreach ($adults->rows as $row) {
        echo "  {$row['name']} ({$row['age']})\n";
    }

    // --- KV ---
    $db->kv->put('sessions', 'sid-1', 'payload');
    echo 'kv get: ' . $db->kv->get('sessions', 'sid-1') . "\n";

    // --- Documents (with query operators) ---
    $db->doc->insertMany('people', [
        ['name' => 'alice', 'age' => 30, 'city' => 'NYC'],
        ['name' => 'bob', 'age' => 25, 'city' => 'LA'],
        ['name' => 'carol', 'age' => 41, 'city' => 'NYC'],
    ]);
    $nyAdults = $db->doc->find('people', Q::and(Q::eq('city', 'NYC'), Q::gt('age', 30)));
    echo 'NYC adults: ' . \count($nyAdults) . "\n";

    // --- Transaction (atomic across models) ---
    $db->begin();
    $db->sql("INSERT INTO users VALUES (4,'dave',50)");
    $db->kv->put('sessions', 'sid-2', 'tx');
    $db->commit();
    echo 'user count: ' . $db->sql('SELECT COUNT(*) FROM users')->raw[0][0] . "\n";
} finally {
    $db->close();
}
