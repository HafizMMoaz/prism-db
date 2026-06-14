<?php

declare(strict_types=1);

namespace PrismDb;

/**
 * Low-level binary codec: a growable little-endian Writer, a bounds-checked
 * Reader, and the length-prefixed frame helper. The byte layouts mirror
 * crates/prism-protocol/src/codec.rs exactly (all multi-byte integers LE).
 *
 * 64-bit integers are read/written as two 32-bit little-endian halves so the
 * encoding is independent of the platform's native byte order. PHP integers are
 * signed 64-bit; values at or above 2^63 round-trip as negative ints.
 */
final class Writer
{
    private string $buf = '';

    public function u8(int $v): void
    {
        $this->buf .= \chr($v & 0xFF);
    }

    public function u16(int $v): void
    {
        $this->buf .= \pack('v', $v & 0xFFFF);
    }

    public function u32(int $v): void
    {
        $this->buf .= \pack('V', $v & 0xFFFFFFFF);
    }

    public function i32(int $v): void
    {
        $this->buf .= \pack('V', $v & 0xFFFFFFFF);
    }

    public function u64(int $v): void
    {
        $this->buf .= \pack('V', $v & 0xFFFFFFFF) . \pack('V', ($v >> 32) & 0xFFFFFFFF);
    }

    public function i64(int $v): void
    {
        $this->u64($v);
    }

    public function f64(float $v): void
    {
        $this->buf .= \pack('e', $v);
    }

    /** A 128-bit unsigned integer as 16 little-endian bytes (low 64 bits = $lo, high = 0). */
    public function u128(int $lo): void
    {
        $this->u64($lo);
        $this->buf .= "\x00\x00\x00\x00\x00\x00\x00\x00";
    }

    public function raw(string $bytes): void
    {
        $this->buf .= $bytes;
    }

    /** A UTF-8 (byte) string with a u16 length prefix. */
    public function strU16(string $s): void
    {
        $this->u16(\strlen($s));
        $this->buf .= $s;
    }

    /** A UTF-8 (byte) string with a u32 length prefix. */
    public function strU32(string $s): void
    {
        $this->u32(\strlen($s));
        $this->buf .= $s;
    }

    /** A byte string with a u16 length prefix. */
    public function bytesU16(string $b): void
    {
        $this->u16(\strlen($b));
        $this->buf .= $b;
    }

    /** A byte string with a u32 length prefix. */
    public function bytesU32(string $b): void
    {
        $this->u32(\strlen($b));
        $this->buf .= $b;
    }

    public function out(): string
    {
        return $this->buf;
    }
}

final class Reader
{
    private int $p = 0;

    public function __construct(private string $buf)
    {
    }

    private function need(int $n): void
    {
        if ($this->p + $n > \strlen($this->buf)) {
            throw new ProtocolException("truncated: need {$n} bytes at offset {$this->p}");
        }
    }

    public function u8(): int
    {
        $this->need(1);
        return \ord($this->buf[$this->p++]);
    }

    public function u16(): int
    {
        $this->need(2);
        $v = \unpack('v', $this->buf, $this->p)[1];
        $this->p += 2;
        return $v;
    }

    public function u32(): int
    {
        $this->need(4);
        $v = \unpack('V', $this->buf, $this->p)[1];
        $this->p += 4;
        return $v;
    }

    public function i32(): int
    {
        $v = $this->u32();
        return $v >= 0x80000000 ? $v - 0x100000000 : $v;
    }

    public function u64(): int
    {
        $this->need(8);
        $parts = \unpack('Vlo/Vhi', $this->buf, $this->p);
        $this->p += 8;
        return ($parts['hi'] << 32) | $parts['lo'];
    }

    public function i64(): int
    {
        return $this->u64();
    }

    public function f64(): float
    {
        $this->need(8);
        $v = \unpack('e', $this->buf, $this->p)[1];
        $this->p += 8;
        return $v;
    }

    /** Reads 16 bytes (a u128) and returns them raw; callers typically discard. */
    public function u128(): string
    {
        return $this->raw(16);
    }

    public function raw(int $n): string
    {
        $this->need($n);
        $s = \substr($this->buf, $this->p, $n);
        $this->p += $n;
        return $s;
    }

    public function strU16(): string
    {
        return $this->raw($this->u16());
    }

    public function strU32(): string
    {
        return $this->raw($this->u32());
    }

    public function bytesU16(): string
    {
        return $this->raw($this->u16());
    }

    public function bytesU32(): string
    {
        return $this->raw($this->u32());
    }

    public function remaining(): int
    {
        return \strlen($this->buf) - $this->p;
    }

    /** Throw unless every byte has been consumed. */
    public function expectEnd(): void
    {
        if ($this->remaining() !== 0) {
            throw new ProtocolException("{$this->remaining()} trailing byte(s) after message");
        }
    }
}

final class Frame
{
    /** Wrap a payload in a [len:u32][payload] frame. */
    public static function encode(string $payload): string
    {
        return \pack('V', \strlen($payload)) . $payload;
    }
}
