<?php

declare(strict_types=1);

namespace PrismDb;

/** A SQL result set. $rows are keyed by column name; $raw keeps cell order. */
final class SqlResult
{
    /**
     * @param ColumnDesc[] $columns
     * @param array<int, array<string, mixed>> $rows
     * @param array<int, array<int, mixed>> $raw
     */
    public function __construct(
        public array $columns,
        public array $rows,
        public array $raw,
        public int $affectedRows,
    ) {
    }
}

/** client->kv - namespaced key/value operations. */
final class KvSurface
{
    public function __construct(private Client $c)
    {
    }

    public function get(string $namespace, string $key): ?string
    {
        $reply = $this->c->kvReply(...Protocol::kvGetBody($namespace, $key));
        if ($reply->op !== 1) {
            throw new ProtocolException('expected a KV get result');
        }
        return $reply->value;
    }

    public function put(string $namespace, string $key, string $value): void
    {
        $this->c->kvReply(...Protocol::kvPutBody($namespace, $key, $value));
    }

    public function delete(string $namespace, string $key): void
    {
        $this->c->kvReply(...Protocol::kvDeleteBody($namespace, $key));
    }
}

/** client->doc - document collection operations. */
final class DocSurface
{
    public function __construct(private Client $c)
    {
    }

    public function insertOne(string $collection, array $document): ObjectId
    {
        $reply = $this->c->docReply(...Protocol::docBody(1, $collection, [DocumentCodec::encode($document)]));
        if (\count($reply->insertedIds) === 0) {
            throw new ProtocolException('insert returned no _id');
        }
        return $reply->insertedIds[0];
    }

    /**
     * @param array<int, array<string, mixed>> $documents
     * @return ObjectId[]
     */
    public function insertMany(string $collection, array $documents): array
    {
        $blobs = \array_map(static fn (array $d): string => DocumentCodec::encode($d), $documents);
        return $this->c->docReply(...Protocol::docInsertManyBody($collection, $blobs))->insertedIds;
    }

    /** @return array<int, array<string, mixed>> */
    public function find(string $collection, ?DocQuery $query = null): array
    {
        $reply = $this->c->docReply(...Protocol::docBody(3, $collection, [QueryCodec::encode($query ?? Q::all()), '']));
        return \array_map(static fn (string $d): array => DocumentCodec::decode($d), $reply->docs);
    }

    /** @return array<string, mixed>|null */
    public function findOne(string $collection, ?DocQuery $query = null): ?array
    {
        $reply = $this->c->docReply(...Protocol::docBody(4, $collection, [QueryCodec::encode($query ?? Q::all()), '']));
        return \count($reply->docs) > 0 ? DocumentCodec::decode($reply->docs[0]) : null;
    }

    public function count(string $collection, ?DocQuery $query = null): int
    {
        return $this->c->docReply(...Protocol::docBody(9, $collection, [QueryCodec::encode($query ?? Q::all()), '']))->affected;
    }

    /** @param DocUpdate[] $update */
    public function updateOne(string $collection, DocQuery $query, array $update): int
    {
        return $this->c->docReply(...Protocol::docBody(5, $collection, [QueryCodec::encode($query), UpdateCodec::encode($update), '']))->affected;
    }

    /** @param DocUpdate[] $update */
    public function updateMany(string $collection, DocQuery $query, array $update): int
    {
        return $this->c->docReply(...Protocol::docBody(6, $collection, [QueryCodec::encode($query), UpdateCodec::encode($update), '']))->affected;
    }

    public function deleteOne(string $collection, DocQuery $query): int
    {
        return $this->c->docReply(...Protocol::docBody(7, $collection, [QueryCodec::encode($query), '']))->affected;
    }

    public function deleteMany(string $collection, DocQuery $query): int
    {
        return $this->c->docReply(...Protocol::docBody(8, $collection, [QueryCodec::encode($query), '']))->affected;
    }
}

/**
 * The high-level client: connect + handshake, then SQL / KV / document calls and
 * transaction control. One client owns one connection = one server session, so a
 * begin() ... commit() brackets the calls in between.
 */
final class Client
{
    private const PROTOCOL_VERSION = 1;

    public readonly KvSurface $kv;
    public readonly DocSurface $doc;

    private function __construct(private Connection $conn)
    {
        $this->kv = new KvSurface($this);
        $this->doc = new DocSurface($this);
    }

    /**
     * Connect, perform the handshake, and (if $username is set) authenticate.
     *
     * @param array<string, mixed> $tls Stream SSL context options (used when $useTls is true).
     */
    public static function connect(
        string $host = '127.0.0.1',
        int $port = 4444,
        ?string $username = null,
        ?string $password = null,
        ?string $database = null,
        bool $useTls = false,
        array $tls = [],
        float $connectTimeout = 10.0,
        string $clientName = 'prismdb-php',
        string $clientVersion = '0.1.0',
        ?callable $onNotice = null,
    ): self {
        $conn = Connection::connect($host, $port, $useTls, $tls, $connectTimeout, $onNotice);
        $client = new self($conn);
        try {
            $connectDbHonored = $client->handshake($username, $password, $database ?? '', $clientName, $clientVersion);
            if (($database ?? '') !== '' && !$connectDbHonored) {
                $client->sql("USE {$database}", returnRows: false);
            }
        } catch (\Throwable $e) {
            $conn->close();
            throw $e;
        }
        return $client;
    }

    private function handshake(?string $username, ?string $password, string $database, string $clientName, string $clientVersion): bool
    {
        $features = $database !== '' ? Msg::FEATURE_CONNECT_DB : 0;
        $ack = $this->conn->request(...Protocol::helloBody(self::PROTOCOL_VERSION, $clientName, $clientVersion, $features, $database));
        if (!$ack instanceof HelloAckMsg) {
            throw new ProtocolException('expected HelloAck');
        }
        if ($ack->status !== 0) {
            self::fail($ack->error);
        }
        $connectDbHonored = ($ack->features & Msg::FEATURE_CONNECT_DB) !== 0 && $database !== '';

        if ($username !== null) {
            $authAck = $this->conn->request(...Protocol::authBody(Msg::AUTH_PASSWORD, $username, $password ?? ''));
            if (!$authAck instanceof AuthAckMsg) {
                throw new ProtocolException('expected AuthAck');
            }
            if ($authAck->status !== 0) {
                self::fail($authAck->error);
            }
        }
        return $connectDbHonored;
    }

    // ---- SQL -------------------------------------------------------------

    /**
     * Execute a SQL statement. Returns rows for SELECT, counts otherwise.
     *
     * @param mixed[] $params
     */
    public function sql(string $text, array $params = [], bool $returnRows = true): SqlResult
    {
        $reply = $this->conn->request(...Protocol::sqlBody($text, $params, $returnRows ? 1 : 0));
        if (!$reply instanceof SqlResultMsg) {
            throw new ProtocolException('expected SqlResult');
        }
        if ($reply->status !== 0) {
            self::fail($reply->error);
        }
        if ($reply->moreFrames) {
            throw new ProtocolException('streamed SQL results are not yet supported');
        }
        $names = \array_map(static fn (ColumnDesc $c): string => $c->name, $reply->columns);
        $rows = [];
        foreach ($reply->rows as $cells) {
            $obj = [];
            foreach ($names as $i => $name) {
                $obj[$name] = $cells[$i] ?? null;
            }
            $rows[] = $obj;
        }
        return new SqlResult($reply->columns, $rows, $reply->rows, $reply->affectedRows);
    }

    // ---- transactions ----------------------------------------------------

    /** Begin a transaction; returns the assigned transaction id. */
    public function begin(bool $readOnly = false): int
    {
        return $this->txn(...Protocol::beginBody($readOnly ? Msg::TXN_READ_ONLY : Msg::TXN_READ_WRITE))->txnId;
    }

    /** Commit the current transaction (optionally idempotent). */
    public function commit(int $idempotencyKey = 0): void
    {
        $this->txn(...Protocol::commitBody($idempotencyKey));
    }

    /** Abort the current transaction. */
    public function abort(): void
    {
        $this->txn(...Protocol::abortBody());
    }

    private function txn(int $typeCode, string $body): TxnAckMsg
    {
        $reply = $this->conn->request($typeCode, $body);
        if (!$reply instanceof TxnAckMsg) {
            throw new ProtocolException('expected TxnAck');
        }
        if ($reply->status !== 0) {
            self::fail($reply->error);
        }
        return $reply;
    }

    // ---- misc ------------------------------------------------------------

    /** Round-trip a keep-alive ping. */
    public function ping(): void
    {
        $reply = $this->conn->request(...Protocol::pingBody());
        if (!$reply instanceof PongMsg) {
            throw new ProtocolException('expected Pong');
        }
    }

    public function close(): void
    {
        $this->conn->close();
    }

    /** @internal */
    public function kvReply(int $typeCode, string $body): KvResultMsg
    {
        $reply = $this->conn->request($typeCode, $body);
        if (!$reply instanceof KvResultMsg) {
            throw new ProtocolException('expected KvResult');
        }
        if ($reply->status !== 0) {
            self::fail($reply->error);
        }
        return $reply;
    }

    /** @internal */
    public function docReply(int $typeCode, string $body): DocResultMsg
    {
        $reply = $this->conn->request($typeCode, $body);
        if (!$reply instanceof DocResultMsg) {
            throw new ProtocolException('expected DocResult');
        }
        if ($reply->status !== 0) {
            self::fail($reply->error);
        }
        if ($reply->moreFrames) {
            throw new ProtocolException('streamed document results are not yet supported');
        }
        return $reply;
    }

    private static function fail(?ErrorInfo $error): never
    {
        throw new PrismServerException($error ?? new ErrorInfo(0, 'server error', 'XX000'));
    }
}
