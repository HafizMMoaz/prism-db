<?php

declare(strict_types=1);

namespace PrismDb;

/**
 * The document tagged-binary codec.
 *
 * Mirrors crates/prism-doc/src/value.rs (Document::encode/decode). A document is
 * [total:u32][count:u16] followed by, per field, [tag:u8][nameLen:u16][name]
 * [value bytes]. Field value bytes use the same encoding as scalar values,
 * except documents have no Binary type. A document is a PHP associative array.
 */
final class DocumentCodec
{
    /** Encode a document (associative array) to its tagged-binary payload. */
    public static function encode(array $doc): string
    {
        $body = new Writer();
        if (\count($doc) > 0xFFFF) {
            throw new ProtocolException('too many document fields');
        }
        $body->u16(\count($doc));
        foreach ($doc as $name => $value) {
            $tag = ValueCodec::tagOf($value);
            if ($tag === Tag::BINARY) {
                throw new ProtocolException("field \"{$name}\": binary values are not supported in documents");
            }
            $body->u8($tag);
            $body->strU16((string) $name);
            ValueCodec::encodeUntagged($body, $tag, $value);
        }
        $inner = $body->out();
        $out = new Writer();
        $out->u32(4 + \strlen($inner)); // total length, including this u32
        $out->raw($inner);
        return $out->out();
    }

    /** Decode a document from its tagged-binary payload into an associative array. */
    public static function decode(string $bytes): array
    {
        $r = new Reader($bytes);
        $r->u32(); // total length (redundant with the frame's blob length)
        $count = $r->u16();
        $doc = [];
        for ($i = 0; $i < $count; $i++) {
            $tag = $r->u8();
            $name = $r->strU16();
            $doc[$name] = ValueCodec::decodeUntagged($r, $tag);
        }
        return $doc;
    }
}
