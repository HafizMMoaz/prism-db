# prismdb/client (PHP)

A **pure-PHP** client for [PrismDB](https://github.com/HafizMMoaz/prism-db), speaking the binary wire
protocol directly over a TCP (or TLS) stream. No PHP extension required beyond
the standard `sockets`/`openssl` streams that ship with PHP.

> Implements `docs/specs/wire-protocol.md`. The byte layouts are kept in lockstep
> with the Rust `prism-protocol` crate and the reference Node SDK.

## Install

```bash
composer require prismdb/client
```

Requires PHP ≥ 8.1.

## Quick start

```php
use PrismDb\Client;
use PrismDb\Q;
use PrismDb\U;

$db = Client::connect(host: '127.0.0.1', port: 4444, username: 'admin', password: 'admin');

// SQL
$db->sql('CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT, age BIGINT)');
$db->sql("INSERT INTO users VALUES (1,'alice',30),(2,'bob',25)");
$res = $db->sql('SELECT name, age FROM users WHERE age >= 30 ORDER BY age');
foreach ($res->rows as $row) echo "{$row['name']} {$row['age']}\n";

// Key/value
$db->kv->put('sessions', 'sid-1', 'payload');
$v = $db->kv->get('sessions', 'sid-1');           // string|null

// Documents, with query operators
$db->doc->insertOne('people', ['name' => 'carol', 'age' => 41, 'city' => 'NYC']);
$adults = $db->doc->find('people', Q::and(Q::eq('city', 'NYC'), Q::gt('age', 30)));

// A transaction is atomic across all three models
$db->begin();
$db->sql("INSERT INTO users VALUES (3,'dave',50)");
$db->kv->put('sessions', 'sid-2', 'tx');
$db->commit();                                     // or $db->abort()

$db->close();
```

## API

### `Client::connect(host, port, username, password, database, useTls, tls, ...)`

Performs the `Hello`/`Auth` handshake. Omit `username` to skip authentication.
Pass `useTls: true` (with optional `tls:` stream SSL context options) for TLS. On
a multi-database server, pass `database:` to select it at connect; otherwise run
`$db->sql('USE <name>')` yourself.

### SQL — `$db->sql(string $text, array $params = [], bool $returnRows = true)`

Returns a `SqlResult` with `->columns`, `->rows` (associative arrays keyed by
column name), `->raw` (cells in column order), and `->affectedRows` (int).

### KV — `$db->kv`

`get(ns, key): ?string`, `put(ns, key, value)`, `delete(ns, key)`. Keys and
values are byte strings.

### Documents — `$db->doc`

`insertOne` / `insertMany` (return the assigned `ObjectId`s), `find` / `findOne`,
`count`, `updateOne` / `updateMany`, `deleteOne` / `deleteMany`. Build filters with
`Q` and updates with `U`:

```php
Q::all();
Q::eq('f', $v); Q::ne; Q::gt; Q::lt; Q::gte; Q::lte;
Q::in('f', [$a, $b]); Q::nin('f', [$a, $b]);
Q::exists('f', true);
Q::and($a, $b); Q::or($a, $b); Q::not($a);

$db->doc->updateOne('people', Q::eq('name', 'carol'), [
    U::set('city', 'Boston'),
    U::inc('age', 1),
    U::unset('temp'),
]);
```

### Transactions — `$db->begin(bool $readOnly = false)`, `$db->commit(int $idempotencyKey = 0)`, `$db->abort()`

One `Client` is one server session, so calls between `begin()` and `commit()` run
in that transaction.

### Value mapping

PHP → wire: `null`→Null, `bool`→Bool, `int`→Int64, `float`→Double, `string`→Str,
`DateTimeInterface`→Timestamp, `ObjectId`→ObjectId. PHP has no separate byte type,
so a plain string is text; use `Prism::binary($bytes)` for a BLOB and
`Prism::int32($n)` / `Prism::float64($n)` / `Prism::timestamp($us)` to force the
other wire types. Integers at or above 2^63 round-trip as negative PHP ints.

## Develop

```bash
php tests/run.php          # unit tests (no server, no composer install needed)

# end-to-end against a running server:
prismd run ./data 127.0.0.1:4444
PRISM_HOST=127.0.0.1 PRISM_PORT=4444 php examples/quickstart.php
```

## Status / limitations

- Streamed (multi-frame) SQL/document results are not yet reassembled.
- KV `range`/`scan` are follow-ups.
- The client is synchronous; one `Client` owns one connection.
