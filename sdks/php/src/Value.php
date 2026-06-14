<?php

declare(strict_types=1);

namespace PrismDb;

/**
 * The scalar value model and its tagged/untagged wire codec.
 *
 * Mirrors crates/prism-protocol/src/data.rs (Value) and the type tags in
 * docs/specs/record-format.md. Plain PHP values map as: null->Null, bool->Bool,
 * int->Int64, float->Double, string->String, DateTimeInterface->Timestamp,
 * ObjectId->ObjectId. PHP has no distinct byte type, so a plain string is text;
 * use Prism::binary() for a BLOB and Prism::int32()/float64()/timestamp() to
 * force the other wire types.
 */
final class Tag
{
    public const NULL = 0x00;
    public const BOOL = 0x01;
    public const INT32 = 0x02;
    public const INT64 = 0x03;
    public const DOUBLE = 0x04;
    public const STRING = 0x05;
    public const BINARY = 0x06;
    public const TIMESTAMP = 0x09;
    public const OBJECTID = 0x0A;
}

/** A 12-byte document identifier. */
final class ObjectId
{
    public string $bytes;

    public function __construct(string $bytes)
    {
        if (\strlen($bytes) !== 12) {
            throw new ProtocolException('ObjectId must be 12 bytes');
        }
        $this->bytes = $bytes;
    }

    /** Lowercase 24-character hex. */
    public function toHex(): string
    {
        return \bin2hex($this->bytes);
    }

    public static function fromHex(string $hex): self
    {
        if (\strlen($hex) !== 24) {
            throw new ProtocolException('ObjectId hex must be 24 chars');
        }
        return new self((string) \hex2bin($hex));
    }

    public function __toString(): string
    {
        return $this->toHex();
    }

    public function equals(ObjectId $other): bool
    {
        return $other->bytes === $this->bytes;
    }
}

/**
 * An explicitly-typed value, for cases where the default mapping of a PHP value
 * is not what you want. Build with the Prism helpers.
 */
final class Typed
{
    public function __construct(public int $tag, public mixed $value)
    {
    }
}

/** Helpers to build explicitly-typed values. */
final class Prism
{
    /** Force a value to wire Int32. */
    public static function int32(int $n): Typed
    {
        return new Typed(Tag::INT32, $n);
    }

    /** Force a value to wire Int64. */
    public static function int64(int $n): Typed
    {
        return new Typed(Tag::INT64, $n);
    }

    /** Force a value to wire Double. */
    public static function float64(float $n): Typed
    {
        return new Typed(Tag::DOUBLE, $n);
    }

    /** Force a value to wire Timestamp (microseconds since the Unix epoch). */
    public static function timestamp(int $us): Typed
    {
        return new Typed(Tag::TIMESTAMP, $us);
    }

    /** Treat a string as a BLOB rather than text. */
    public static function binary(string $bytes): Typed
    {
        return new Typed(Tag::BINARY, $bytes);
    }
}

final class ValueCodec
{
    /** Resolve a PHP value to its wire type tag. */
    public static function tagOf(mixed $v): int
    {
        if ($v === null) {
            return Tag::NULL;
        }
        if ($v instanceof Typed) {
            return $v->tag;
        }
        if ($v instanceof ObjectId) {
            return Tag::OBJECTID;
        }
        if (\is_bool($v)) {
            return Tag::BOOL;
        }
        if (\is_int($v)) {
            return Tag::INT64;
        }
        if (\is_float($v)) {
            return Tag::DOUBLE;
        }
        if (\is_string($v)) {
            return Tag::STRING;
        }
        if ($v instanceof \DateTimeInterface) {
            return Tag::TIMESTAMP;
        }
        throw new ProtocolException('unsupported value: ' . \get_debug_type($v));
    }

    public static function encodeUntagged(Writer $w, int $tag, mixed $v): void
    {
        $raw = $v instanceof Typed ? $v->value : $v;
        switch ($tag) {
            case Tag::NULL:
                break;
            case Tag::BOOL:
                $w->u8($raw ? 1 : 0);
                break;
            case Tag::INT32:
                $w->i32((int) $raw);
                break;
            case Tag::INT64:
                $w->i64((int) $raw);
                break;
            case Tag::DOUBLE:
                $w->f64((float) $raw);
                break;
            case Tag::TIMESTAMP:
                $w->i64($raw instanceof \DateTimeInterface
                    ? (int) ($raw->format('U') * 1000000 + (int) $raw->format('u'))
                    : (int) $raw);
                break;
            case Tag::STRING:
                $w->strU32((string) $raw);
                break;
            case Tag::OBJECTID:
                $w->raw($raw->bytes);
                break;
            case Tag::BINARY:
                $b = (string) $raw;
                $w->u32(\strlen($b));
                $w->u8(0); // subtype
                $w->raw($b);
                break;
            default:
                throw new ProtocolException(\sprintf('cannot encode value tag 0x%x', $tag));
        }
    }

    public static function encodeTagged(Writer $w, mixed $v): void
    {
        $tag = self::tagOf($v);
        $w->u8($tag);
        self::encodeUntagged($w, $tag, $v);
    }

    public static function decodeUntagged(Reader $r, int $tag): mixed
    {
        switch ($tag) {
            case Tag::NULL:
                return null;
            case Tag::BOOL:
                return $r->u8() !== 0;
            case Tag::INT32:
                return $r->i32();
            case Tag::INT64:
                return $r->i64();
            case Tag::DOUBLE:
                return $r->f64();
            case Tag::TIMESTAMP:
                return $r->i64();
            case Tag::STRING:
                return $r->strU32();
            case Tag::OBJECTID:
                return new ObjectId($r->raw(12));
            case Tag::BINARY:
                $len = $r->u32();
                $r->u8(); // subtype (discarded)
                return $r->raw($len);
            default:
                throw new ProtocolException(\sprintf('unknown value tag 0x%x', $tag));
        }
    }

    public static function decodeTagged(Reader $r): mixed
    {
        return self::decodeUntagged($r, $r->u8());
    }
}
