<?php

declare(strict_types=1);

namespace PrismDb;

/**
 * Document update operators and their wire encoding.
 *
 * Mirrors prism_protocol::DocUpdate: an ordered list of $set / $unset / $inc
 * mutations. Build with the U helpers. Operand values reuse the tagged Value
 * encoding. Carried as the `update` blob of an update command.
 */
final class DocUpdate
{
    public function __construct(
        public string $op,
        public string $field,
        public mixed $value = null,
        public int $delta = 0,
    ) {
    }
}

/** Update builders mirroring the engine's update operators. */
final class U
{
    /** $set — set $field to $value. */
    public static function set(string $field, mixed $value): DocUpdate
    {
        return new DocUpdate('set', $field, value: $value);
    }

    /** $unset — remove $field. */
    public static function unset(string $field): DocUpdate
    {
        return new DocUpdate('unset', $field);
    }

    /** $inc — add $delta to the integer $field. */
    public static function inc(string $field, int $delta): DocUpdate
    {
        return new DocUpdate('inc', $field, delta: $delta);
    }
}

final class UpdateCodec
{
    /** @param DocUpdate[] $ops */
    public static function encode(array $ops): string
    {
        $w = new Writer();
        $w->u32(\count($ops));
        foreach ($ops as $op) {
            switch ($op->op) {
                case 'set':
                    $w->u8(1);
                    $w->strU16($op->field);
                    ValueCodec::encodeTagged($w, $op->value);
                    break;
                case 'unset':
                    $w->u8(2);
                    $w->strU16($op->field);
                    break;
                case 'inc':
                    $w->u8(3);
                    $w->strU16($op->field);
                    $w->i64($op->delta);
                    break;
            }
        }
        return $w->out();
    }
}
