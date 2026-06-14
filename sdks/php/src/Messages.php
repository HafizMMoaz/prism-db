<?php

declare(strict_types=1);

namespace PrismDb;

/**
 * Protocol messages: the 12-byte header plus per-message bodies.
 *
 * Mirrors crates/prism-protocol/src/message.rs. The SDK encodes client messages
 * (as [typeCode, body] pairs) and decodes server messages; both directions share
 * the header and the error trailer.
 */
final class Msg
{
    public const HELLO = 0x01;
    public const HELLO_ACK = 0x02;
    public const AUTH = 0x03;
    public const AUTH_ACK = 0x04;
    public const BEGIN = 0x10;
    public const COMMIT = 0x11;
    public const ABORT = 0x12;
    public const TXN_ACK = 0x13;
    public const SQL_EXECUTE = 0x20;
    public const SQL_RESULT = 0x21;
    public const DOC_OP = 0x30;
    public const DOC_RESULT = 0x31;
    public const KV_OP = 0x40;
    public const KV_RESULT = 0x41;
    public const CANCEL = 0x50;
    public const NOTICE = 0x60;
    public const PING = 0x70;
    public const PONG = 0x71;

    public const AUTH_PASSWORD = 1;
    public const TXN_READ_WRITE = 0;
    public const TXN_READ_ONLY = 1;
    public const FEATURE_CONNECT_DB = 1;
}

final class ColumnDesc
{
    public function __construct(public string $name, public int $typeTag, public bool $nullable)
    {
    }
}

final class HelloAckMsg
{
    public function __construct(public int $status, public string $serverVersion, public int $features, public ?ErrorInfo $error)
    {
    }
}

final class AuthAckMsg
{
    public function __construct(public int $status, public int $userOid, public ?ErrorInfo $error)
    {
    }
}

final class TxnAckMsg
{
    public function __construct(public int $status, public int $txnId, public int $commitLsn, public ?ErrorInfo $error)
    {
    }
}

final class SqlResultMsg
{
    /**
     * @param ColumnDesc[] $columns
     * @param array<int, array<int, mixed>> $rows
     */
    public function __construct(
        public int $status,
        public int $affectedRows,
        public array $columns,
        public array $rows,
        public bool $moreFrames,
        public ?ErrorInfo $error,
    ) {
    }
}

final class DocResultMsg
{
    /**
     * @param ObjectId[] $insertedIds
     * @param string[] $docs
     */
    public function __construct(
        public int $status,
        public int $affected,
        public array $insertedIds,
        public array $docs,
        public bool $moreFrames,
        public ?ErrorInfo $error,
    ) {
    }
}

final class KvResultMsg
{
    /** @param array<int, array{0: string, 1: string}> $entries */
    public function __construct(
        public int $status,
        public int $op,
        public ?string $value = null,
        public array $entries = [],
        public bool $moreFrames = false,
        public ?ErrorInfo $error = null,
    ) {
    }
}

final class NoticeMsg
{
    public function __construct(public int $severity, public int $code, public string $message)
    {
    }
}

final class PongMsg
{
}

final class ServerPacket
{
    public function __construct(public int $requestId, public object $message)
    {
    }
}

final class Protocol
{
    public static function encodePacket(int $requestId, int $typeCode, string $body): string
    {
        $w = new Writer();
        $w->u8($typeCode);
        $w->raw("\x00\x00\x00");
        $w->u32($requestId);
        $w->raw("\x00\x00\x00\x00");
        $w->raw($body);
        return $w->out();
    }

    // ---- client bodies -> [typeCode, body] -------------------------------

    /** @return array{0:int,1:string} */
    public static function helloBody(int $protocolVersion, string $clientName, string $clientVersion, int $features, string $database): array
    {
        $w = new Writer();
        $w->u32($protocolVersion);
        $w->strU16($clientName);
        $w->strU16($clientVersion);
        $w->u32($features);
        if ($features & Msg::FEATURE_CONNECT_DB) {
            $w->strU16($database);
        }
        return [Msg::HELLO, $w->out()];
    }

    /** @return array{0:int,1:string} */
    public static function authBody(int $mechanism, string $username, string $password): array
    {
        $w = new Writer();
        $w->u8($mechanism);
        $w->strU16($username);
        if ($mechanism === Msg::AUTH_PASSWORD) {
            $w->strU16($password);
        }
        return [Msg::AUTH, $w->out()];
    }

    /** @return array{0:int,1:string} */
    public static function beginBody(int $mode): array
    {
        $w = new Writer();
        $w->u8($mode);
        return [Msg::BEGIN, $w->out()];
    }

    /** @return array{0:int,1:string} */
    public static function commitBody(int $idempotencyKey): array
    {
        $w = new Writer();
        $w->u128($idempotencyKey);
        return [Msg::COMMIT, $w->out()];
    }

    /** @return array{0:int,1:string} */
    public static function abortBody(): array
    {
        return [Msg::ABORT, ''];
    }

    /**
     * @param mixed[] $params
     * @return array{0:int,1:string}
     */
    public static function sqlBody(string $sql, array $params, int $options): array
    {
        $w = new Writer();
        $w->strU32($sql);
        $w->u16(\count($params));
        foreach ($params as $p) {
            ValueCodec::encodeTagged($w, $p);
        }
        $w->u32($options);
        return [Msg::SQL_EXECUTE, $w->out()];
    }

    /**
     * @param string[] $blobs
     * @return array{0:int,1:string}
     */
    public static function docBody(int $op, string $collection, array $blobs): array
    {
        $w = new Writer();
        $w->u8($op);
        $w->strU16($collection);
        foreach ($blobs as $b) {
            $w->bytesU32($b);
        }
        return [Msg::DOC_OP, $w->out()];
    }

    /**
     * @param string[] $docs
     * @return array{0:int,1:string}
     */
    public static function docInsertManyBody(string $collection, array $docs): array
    {
        $w = new Writer();
        $w->u8(2);
        $w->strU16($collection);
        $w->u32(\count($docs));
        foreach ($docs as $d) {
            $w->bytesU32($d);
        }
        return [Msg::DOC_OP, $w->out()];
    }

    /** @return array{0:int,1:string} */
    public static function kvGetBody(string $ns, string $key): array
    {
        $w = new Writer();
        $w->u8(1);
        $w->strU16($ns);
        $w->bytesU16($key);
        return [Msg::KV_OP, $w->out()];
    }

    /** @return array{0:int,1:string} */
    public static function kvPutBody(string $ns, string $key, string $value): array
    {
        $w = new Writer();
        $w->u8(2);
        $w->strU16($ns);
        $w->bytesU16($key);
        $w->bytesU32($value);
        return [Msg::KV_OP, $w->out()];
    }

    /** @return array{0:int,1:string} */
    public static function kvDeleteBody(string $ns, string $key): array
    {
        $w = new Writer();
        $w->u8(3);
        $w->strU16($ns);
        $w->bytesU16($key);
        return [Msg::KV_OP, $w->out()];
    }

    /** @return array{0:int,1:string} */
    public static function pingBody(): array
    {
        return [Msg::PING, ''];
    }

    // ---- server decode ---------------------------------------------------

    public static function decodePacket(string $payload): ServerPacket
    {
        $r = new Reader($payload);
        $type = $r->u8();
        $r->raw(3);
        $requestId = $r->u32();
        $r->raw(4);
        $message = self::decodeBody($type, $r);
        $r->expectEnd();
        return new ServerPacket($requestId, $message);
    }

    private static function decodeTrailer(Reader $r, int $status): ?ErrorInfo
    {
        if ($status === 0) {
            return null;
        }
        return new ErrorInfo(
            code: $r->u32(),
            message: $r->strU16(),
            sqlstate: $r->raw(5),
            detail: $r->strU16(),
            position: $r->u32(),
        );
    }

    private static function decodeBody(int $type, Reader $r): object
    {
        switch ($type) {
            case Msg::HELLO_ACK:
                $status = $r->u8();
                $ver = $r->strU16();
                $features = $r->u32();
                $r->u128(); // session id
                return new HelloAckMsg($status, $ver, $features, self::decodeTrailer($r, $status));
            case Msg::AUTH_ACK:
                $status = $r->u8();
                $oid = $r->u64();
                return new AuthAckMsg($status, $oid, self::decodeTrailer($r, $status));
            case Msg::TXN_ACK:
                $status = $r->u8();
                $txnId = $r->u64();
                $lsn = $r->u64();
                return new TxnAckMsg($status, $txnId, $lsn, self::decodeTrailer($r, $status));
            case Msg::SQL_RESULT:
                $status = $r->u8();
                $affected = $r->u64();
                $colCount = $r->u16();
                $cols = [];
                for ($i = 0; $i < $colCount; $i++) {
                    $cols[] = new ColumnDesc($r->strU16(), $r->u8(), $r->u8() !== 0);
                }
                $rowCount = $r->u32();
                $rows = self::decodeRows($cols, $rowCount, $r);
                $more = $r->u8() !== 0;
                return new SqlResultMsg($status, $affected, $cols, $rows, $more, self::decodeTrailer($r, $status));
            case Msg::DOC_RESULT:
                $status = $r->u8();
                $affected = $r->u64();
                $idCount = $r->u32();
                $ids = [];
                for ($i = 0; $i < $idCount; $i++) {
                    $ids[] = new ObjectId($r->raw(12));
                }
                $docCount = $r->u32();
                $docs = [];
                for ($i = 0; $i < $docCount; $i++) {
                    $docs[] = $r->bytesU32();
                }
                $more = $r->u8() !== 0;
                return new DocResultMsg($status, $affected, $ids, $docs, $more, self::decodeTrailer($r, $status));
            case Msg::KV_RESULT:
                $status = $r->u8();
                $op = $r->u8();
                return self::decodeKvBody($status, $op, $r);
            case Msg::NOTICE:
                return new NoticeMsg($r->u8(), $r->u32(), $r->strU16());
            case Msg::PONG:
                return new PongMsg();
            default:
                throw new ProtocolException(\sprintf('unexpected server message type 0x%x', $type));
        }
    }

    /**
     * @param ColumnDesc[] $columns
     * @return array<int, array<int, mixed>>
     */
    private static function decodeRows(array $columns, int $rowCount, Reader $r): array
    {
        $nb = \intdiv(\count($columns) + 7, 8);
        $rows = [];
        for ($i = 0; $i < $rowCount; $i++) {
            $bitmap = $r->raw($nb);
            $row = [];
            foreach ($columns as $c => $col) {
                $isNull = (\ord($bitmap[$c >> 3]) & (1 << ($c & 7))) !== 0;
                $row[] = $isNull ? null : ValueCodec::decodeUntagged($r, $col->typeTag);
            }
            $rows[] = $row;
        }
        return $rows;
    }

    private static function decodeKvBody(int $status, int $op, Reader $r): KvResultMsg
    {
        switch ($op) {
            case 1:
                $found = $r->u8() !== 0;
                $value = $found ? $r->bytesU32() : null;
                return new KvResultMsg($status, $op, value: $value, error: self::decodeTrailer($r, $status));
            case 2:
            case 3:
                return new KvResultMsg($status, $op, error: self::decodeTrailer($r, $status));
            case 4:
            case 5:
                $count = $r->u32();
                $entries = [];
                for ($i = 0; $i < $count; $i++) {
                    $entries[] = [$r->bytesU16(), $r->bytesU32()];
                }
                $more = $r->u8() !== 0;
                return new KvResultMsg($status, $op, entries: $entries, moreFrames: $more, error: self::decodeTrailer($r, $status));
            default:
                throw new ProtocolException(\sprintf('unknown kv result op 0x%x', $op));
        }
    }
}
