// The transport: a TCP (optionally TLS) socket that frames outgoing messages,
// reads back full frames, and matches each reply to its request by the echoed
// request_id. Server-initiated notices (request_id 0) go to a handler.
//
// The connection is synchronous: each Request writes a frame and blocks until
// the matching reply arrives, dispatching any notices seen in between.

using System;
using System.IO;
using System.Net.Security;
using System.Net.Sockets;

namespace PrismDb
{
    /// <summary>A connection-level notice from the server (e.g. shutdown, idle timeout).</summary>
    public sealed class Notice
    {
        public int Severity { get; }
        public int Code { get; }
        public string Message { get; }

        public Notice(int severity, int code, string message)
        {
            Severity = severity;
            Code = code;
            Message = message;
        }
    }

    /// <summary>Options controlling how a connection is established.</summary>
    public class ConnectionOptions
    {
        public string Host { get; set; } = "127.0.0.1";
        public int Port { get; set; } = 4444;
        /// <summary>Use TLS. When true, validates the server certificate against the OS trust store.</summary>
        public bool Tls { get; set; }
        /// <summary>Override the name validated against the server certificate (defaults to Host).</summary>
        public string? TlsServerName { get; set; }
        /// <summary>Connect timeout in milliseconds (default 10000).</summary>
        public int ConnectTimeoutMs { get; set; } = 10_000;
        public Action<Notice>? OnNotice { get; set; }
    }

    internal sealed class Connection : IDisposable
    {
        private readonly TcpClient _tcp;
        private readonly Stream _stream;
        private readonly Action<Notice>? _onNotice;
        private long _nextId = 1;
        private Exception? _closed;

        private Connection(TcpClient tcp, Stream stream, Action<Notice>? onNotice)
        {
            _tcp = tcp;
            _stream = stream;
            _onNotice = onNotice;
        }

        public static Connection Connect(ConnectionOptions opts)
        {
            var tcp = new TcpClient();
            var ar = tcp.BeginConnect(opts.Host, opts.Port, null, null);
            if (!ar.AsyncWaitHandle.WaitOne(opts.ConnectTimeoutMs))
            {
                tcp.Close();
                throw new ProtocolException($"connect timed out after {opts.ConnectTimeoutMs}ms");
            }
            tcp.EndConnect(ar);
            tcp.NoDelay = true;

            Stream stream = tcp.GetStream();
            if (opts.Tls)
            {
                var ssl = new SslStream(stream, leaveInnerStreamOpen: false);
                ssl.AuthenticateAsClient(opts.TlsServerName ?? opts.Host);
                stream = ssl;
            }
            return new Connection(tcp, stream, opts.OnNotice);
        }

        public ServerMessage Request(int typeCode, byte[] body)
        {
            if (_closed != null) throw _closed;
            long requestId = _nextId;
            _nextId = _nextId >= 0xFFFFFFFF ? 1 : _nextId + 1;
            try
            {
                var frame = Frame.Encode(Protocol.EncodePacket(requestId, typeCode, body));
                _stream.Write(frame, 0, frame.Length);
                _stream.Flush();
            }
            catch (Exception e)
            {
                throw Fail(new ProtocolException($"send failed: {e.Message}"));
            }

            while (true)
            {
                var payload = ReadFrame();
                var packet = Protocol.DecodePacket(payload);
                if (packet.Message is NoticeMsg n)
                {
                    _onNotice?.Invoke(new Notice(n.Severity, n.Code, n.Message));
                    continue;
                }
                if (packet.RequestId == requestId) return packet.Message;
                // An unmatched reply (e.g. a late response) is ignored.
            }
        }

        public void Close()
        {
            Fail(new ProtocolException("connection closed by client"));
            try { _stream.Dispose(); } catch { /* ignore */ }
            try { _tcp.Close(); } catch { /* ignore */ }
        }

        public void Dispose() => Close();

        private byte[] ReadFrame()
        {
            var header = ReadExact(4);
            long len = (uint)(header[0] | (header[1] << 8) | (header[2] << 16) | (header[3] << 24));
            return ReadExact((int)len);
        }

        private byte[] ReadExact(int n)
        {
            var buf = new byte[n];
            int off = 0;
            while (off < n)
            {
                int got;
                try { got = _stream.Read(buf, off, n - off); }
                catch (Exception e) { throw Fail(new ProtocolException($"connection closed by server: {e.Message}")); }
                if (got <= 0) throw Fail(new ProtocolException("connection closed by server"));
                off += got;
            }
            return buf;
        }

        private Exception Fail(Exception err)
        {
            _closed ??= err;
            return _closed;
        }
    }
}
