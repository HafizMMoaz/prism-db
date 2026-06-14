<?php

declare(strict_types=1);

// No-server codec round-trip tests. Run with: php tests/run.php
//
// A tiny zero-dependency harness (no PHPUnit) so it runs offline. Exits 0 if all
// checks pass, 1 otherwise. Requires only the SDK source files.

namespace PrismDb;

$src = \dirname(__DIR__) . '/src';
require "{$src}/Errors.php";
require "{$src}/Codec.php";
require "{$src}/Value.php";
require "{$src}/Document.php";
require "{$src}/Query.php";
require "{$src}/Update.php";
require "{$src}/Messages.php";

$failures = 0;
$check = static function (bool $cond, string $name) use (&$failures): void {
    echo ($cond ? 'ok   ' : 'FAIL ') . $name . "\n";
    if (!$cond) {
        $failures++;
    }
};

// --- Writer / Reader round-trip ---
$w = new Writer();
$w->u8(0x7F);
$w->u16(0xBEEF);
$w->u32(0xDEADBEEF);
$w->i32(-5);
$w->u64(0x1122334455667788);
$w->i64(-9000000000);
$w->f64(3.5);
$w->strU16("héllo");
$w->bytesU32("\x00\x01\x02");
$r = new Reader($w->out());
$check($r->u8() === 0x7F, 'u8 roundtrip');
$check($r->u16() === 0xBEEF, 'u16 roundtrip');
$check($r->u32() === 0xDEADBEEF, 'u32 roundtrip');
$check($r->i32() === -5, 'i32 roundtrip');
$check($r->u64() === 0x1122334455667788, 'u64 roundtrip');
$check($r->i64() === -9000000000, 'i64 roundtrip');
$check(\abs($r->f64() - 3.5) < 1e-12, 'f64 roundtrip');
$check($r->strU16() === "héllo", 'strU16 roundtrip (utf-8)');
$check($r->bytesU32() === "\x00\x01\x02", 'bytesU32 roundtrip');
$r->expectEnd();

// --- frame length prefix ---
$framed = Frame::encode('abcd');
$check(\strlen($framed) === 8 && \ord($framed[0]) === 4, 'frame length prefix');

// --- scalar tagged round-trips ---
$cases = [
    [null, null],
    [true, true],
    [false, false],
    [42, 42],
    [-7, -7],
    [3.25, 3.25],
    ['prism', 'prism'],
    [Prism::int32(123), 123],
    [Prism::float64(5.0), 5.0],
    [Prism::timestamp(1700000000000000), 1700000000000000],
    [Prism::binary("\xDE\xAD"), "\xDE\xAD"],
];
foreach ($cases as [$value, $expected]) {
    $w = new Writer();
    ValueCodec::encodeTagged($w, $value);
    $got = ValueCodec::decodeTagged(new Reader($w->out()));
    $check($got === $expected, 'scalar tagged roundtrip: ' . \var_export($expected, true));
}

// --- int maps to Int64 ---
$w = new Writer();
ValueCodec::encodeTagged($w, 1);
$check(\ord($w->out()[0]) === Tag::INT64, 'int maps to Int64 by default');

// --- ObjectId hex ---
$oid = ObjectId::fromHex('507f1f77bcf86cd799439011');
$check($oid->toHex() === '507f1f77bcf86cd799439011', 'objectid hex roundtrip');
$check($oid->equals(new ObjectId($oid->bytes)), 'objectid equality');

// --- document round-trip ---
$doc = ['name' => 'carol', 'age' => 41, 'active' => true, 'score' => 9.5];
$encoded = DocumentCodec::encode($doc);
$total = \unpack('V', \substr($encoded, 0, 4))[1];
$check($total === \strlen($encoded), 'document total-length prefix');
$check(DocumentCodec::decode($encoded) === $doc, 'document roundtrip');

// --- document rejects binary ---
$threw = false;
try {
    DocumentCodec::encode(['blob' => Prism::binary("\x00")]);
} catch (ProtocolException) {
    $threw = true;
}
$check($threw, 'document rejects binary fields');

// --- query / update ---
$q = Q::and(Q::eq('city', 'NYC'), Q::gt('age', 30), Q::in('tag', ['a', 'b']));
$check(\ord(QueryCodec::encode($q)[0]) === 10, 'query AND discriminant');

$ops = [U::set('city', 'Boston'), U::inc('age', 1), U::unset('temp')];
$check(\unpack('V', \substr(UpdateCodec::encode($ops), 0, 4))[1] === 3, 'update count prefix');

// --- SQL packet header ---
[$type, $body] = Protocol::sqlBody('SELECT 1', [], 1);
$packet = Protocol::encodePacket(7, $type, $body);
$check(\ord($packet[0]) === Msg::SQL_EXECUTE, 'sql packet message type');
$check(\unpack('V', \substr($packet, 4, 4))[1] === 7, 'sql packet request id');

// --- decode AuthAck ---
$w = new Writer();
$w->u8(Msg::AUTH_ACK);
$w->raw("\x00\x00\x00");
$w->u32(99);
$w->raw("\x00\x00\x00\x00");
$w->u8(0);
$w->u64(1234);
$packet = Protocol::decodePacket($w->out());
$check($packet->requestId === 99, 'authack request id');
$check($packet->message instanceof AuthAckMsg && $packet->message->userOid === 1234, 'authack user_oid');

// --- decode error trailer ---
$w = new Writer();
$w->u8(Msg::TXN_ACK);
$w->raw("\x00\x00\x00");
$w->u32(1);
$w->raw("\x00\x00\x00\x00");
$w->u8(2);
$w->u64(0);
$w->u64(0);
$w->u32(0x0201);
$w->strU16('serialization failure');
$w->raw('40001');
$w->strU16('');
$w->u32(0);
$packet = Protocol::decodePacket($w->out());
$err = $packet->message->error ?? null;
$check($err !== null && $err->code === 0x0201 && $err->sqlstate === '40001', 'error trailer decode');

echo "\n" . ($failures === 0 ? "ALL TESTS PASSED\n" : "{$failures} TEST(S) FAILED\n");
exit($failures === 0 ? 0 : 1);
