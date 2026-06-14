<?php

declare(strict_types=1);

namespace PrismDb;

/**
 * The document query filter and its wire encoding.
 *
 * Mirrors prism_protocol::DocQuery (crates/prism-protocol/src/data.rs): a tag
 * byte, then the operator-specific body. Operand values reuse the tagged Value
 * encoding. Build queries with the Q helpers.
 */
final class DocQuery
{
    public const T_ALL = 0;
    public const T_EQ = 1;
    public const T_NE = 2;
    public const T_GT = 3;
    public const T_LT = 4;
    public const T_GTE = 5;
    public const T_LTE = 6;
    public const T_IN = 7;
    public const T_NIN = 8;
    public const T_EXISTS = 9;
    public const T_AND = 10;
    public const T_OR = 11;
    public const T_NOT = 12;

    public function __construct(
        public string $kind,
        public int $tag = 0,
        public string $field = '',
        public mixed $value = null,
        public array $values = [],
        public bool $present = true,
        public array $subs = [],
        public ?DocQuery $sub = null,
    ) {
    }
}

/** Query builders mirroring the engine's filter set. */
final class Q
{
    public static function all(): DocQuery
    {
        return new DocQuery('all');
    }

    public static function eq(string $field, mixed $value): DocQuery
    {
        return new DocQuery('field', tag: DocQuery::T_EQ, field: $field, value: $value);
    }

    public static function ne(string $field, mixed $value): DocQuery
    {
        return new DocQuery('field', tag: DocQuery::T_NE, field: $field, value: $value);
    }

    public static function gt(string $field, mixed $value): DocQuery
    {
        return new DocQuery('field', tag: DocQuery::T_GT, field: $field, value: $value);
    }

    public static function lt(string $field, mixed $value): DocQuery
    {
        return new DocQuery('field', tag: DocQuery::T_LT, field: $field, value: $value);
    }

    public static function gte(string $field, mixed $value): DocQuery
    {
        return new DocQuery('field', tag: DocQuery::T_GTE, field: $field, value: $value);
    }

    public static function lte(string $field, mixed $value): DocQuery
    {
        return new DocQuery('field', tag: DocQuery::T_LTE, field: $field, value: $value);
    }

    public static function in(string $field, array $values): DocQuery
    {
        return new DocQuery('set', tag: DocQuery::T_IN, field: $field, values: \array_values($values));
    }

    public static function nin(string $field, array $values): DocQuery
    {
        return new DocQuery('set', tag: DocQuery::T_NIN, field: $field, values: \array_values($values));
    }

    public static function exists(string $field, bool $present = true): DocQuery
    {
        return new DocQuery('exists', field: $field, present: $present);
    }

    public static function and(DocQuery ...$subs): DocQuery
    {
        return new DocQuery('group', tag: DocQuery::T_AND, subs: $subs);
    }

    public static function or(DocQuery ...$subs): DocQuery
    {
        return new DocQuery('group', tag: DocQuery::T_OR, subs: $subs);
    }

    public static function not(DocQuery $sub): DocQuery
    {
        return new DocQuery('not', sub: $sub);
    }
}

final class QueryCodec
{
    public static function encode(DocQuery $q): string
    {
        $w = new Writer();
        self::encodeInto($w, $q);
        return $w->out();
    }

    private static function encodeInto(Writer $w, DocQuery $q): void
    {
        switch ($q->kind) {
            case 'all':
                $w->u8(DocQuery::T_ALL);
                break;
            case 'field':
                $w->u8($q->tag);
                $w->strU16($q->field);
                ValueCodec::encodeTagged($w, $q->value);
                break;
            case 'set':
                $w->u8($q->tag);
                $w->strU16($q->field);
                $w->u32(\count($q->values));
                foreach ($q->values as $v) {
                    ValueCodec::encodeTagged($w, $v);
                }
                break;
            case 'exists':
                $w->u8(DocQuery::T_EXISTS);
                $w->strU16($q->field);
                $w->u8($q->present ? 1 : 0);
                break;
            case 'group':
                $w->u8($q->tag);
                $w->u32(\count($q->subs));
                foreach ($q->subs as $s) {
                    self::encodeInto($w, $s);
                }
                break;
            case 'not':
                $w->u8(DocQuery::T_NOT);
                self::encodeInto($w, $q->sub);
                break;
        }
    }
}
