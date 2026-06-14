<?php

declare(strict_types=1);

namespace PrismDb;

/**
 * The transport: a TCP (optionally TLS) socket that frames outgoing messages,
 * reads back full frames, and matches each reply to its request by the echoed
 * request_id. Server-initiated notices (request_id 0) go to a handler.
 *
 * The connection is synchronous: each request() writes a frame and blocks until
 * the matching reply arrives, dispatching any notices seen in between.
 */
final class Connection
{
    /** @var resource */
    private $sock;
    private int $nextId = 1;
    private ?\Throwable $closed = null;
    /** @var (callable(NoticeMsg): void)|null */
    private $onNotice;

    /**
     * @param resource $sock
     * @param (callable(NoticeMsg): void)|null $onNotice
     */
    private function __construct($sock, ?callable $onNotice)
    {
        $this->sock = $sock;
        $this->onNotice = $onNotice;
    }

    /**
     * @param array<string, mixed> $tls Stream SSL context options (used when $useTls is true).
     * @param (callable(NoticeMsg): void)|null $onNotice
     */
    public static function connect(
        string $host = '127.0.0.1',
        int $port = 4444,
        bool $useTls = false,
        array $tls = [],
        float $connectTimeout = 10.0,
        ?callable $onNotice = null,
    ): self {
        $scheme = $useTls ? 'tls' : 'tcp';
        $ctx = \stream_context_create($useTls ? ['ssl' => $tls] : []);
        $errno = 0;
        $errstr = '';
        $sock = @\stream_socket_client(
            "{$scheme}://{$host}:{$port}",
            $errno,
            $errstr,
            $connectTimeout,
            \STREAM_CLIENT_CONNECT,
            $ctx,
        );
        if ($sock === false) {
            throw new ProtocolException("connect failed: {$errstr} ({$errno})");
        }
        \stream_set_timeout($sock, 0, 0);
        if (\function_exists('socket_import_stream')) {
            // Best-effort TCP_NODELAY; ignore if unavailable on this build.
        }
        return new self($sock, $onNotice);
    }

    public function request(int $typeCode, string $body): object
    {
        if ($this->closed !== null) {
            throw $this->closed;
        }
        $requestId = $this->nextId;
        $this->nextId = $this->nextId >= 0xFFFFFFFF ? 1 : $this->nextId + 1;

        $frame = Frame::encode(Protocol::encodePacket($requestId, $typeCode, $body));
        $this->writeAll($frame);

        while (true) {
            $payload = $this->readFrame();
            $packet = Protocol::decodePacket($payload);
            if ($packet->message instanceof NoticeMsg) {
                if ($this->onNotice !== null) {
                    ($this->onNotice)($packet->message);
                }
                continue;
            }
            if ($packet->requestId === $requestId) {
                return $packet->message;
            }
            // An unmatched reply (e.g. a late response) is ignored.
        }
    }

    public function close(): void
    {
        $this->fail(new ProtocolException('connection closed by client'));
        if (\is_resource($this->sock)) {
            @\fclose($this->sock);
        }
    }

    private function writeAll(string $data): void
    {
        $len = \strlen($data);
        $off = 0;
        while ($off < $len) {
            $n = @\fwrite($this->sock, \substr($data, $off));
            if ($n === false || $n === 0) {
                throw $this->fail(new ProtocolException('send failed'));
            }
            $off += $n;
        }
    }

    private function readFrame(): string
    {
        $header = $this->readExact(4);
        $len = \unpack('V', $header)[1];
        return $this->readExact($len);
    }

    private function readExact(int $n): string
    {
        $out = '';
        while (\strlen($out) < $n) {
            $chunk = @\fread($this->sock, $n - \strlen($out));
            if ($chunk === false || $chunk === '') {
                if (\feof($this->sock)) {
                    throw $this->fail(new ProtocolException('connection closed by server'));
                }
                throw $this->fail(new ProtocolException('read failed'));
            }
            $out .= $chunk;
        }
        return $out;
    }

    private function fail(\Throwable $err): \Throwable
    {
        $this->closed ??= $err;
        return $this->closed;
    }
}
