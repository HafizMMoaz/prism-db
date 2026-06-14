// Low-level binary codec: a growable little-endian Writer, a bounds-checked
// Reader, and the length-prefixed frame helpers. The byte layouts mirror
// crates/prism-protocol/src/codec.rs exactly (all multi-byte integers LE).

using System;
using System.Text;

namespace PrismDb
{
    /// <summary>A growable little-endian writer over a byte buffer.</summary>
    public sealed class Writer
    {
        private byte[] _buf;
        private int _len;

        public Writer(int capacity = 64)
        {
            _buf = new byte[capacity < 8 ? 8 : capacity];
            _len = 0;
        }

        private void Ensure(int extra)
        {
            int needed = _len + extra;
            if (needed <= _buf.Length) return;
            int cap = _buf.Length * 2;
            while (cap < needed) cap *= 2;
            var next = new byte[cap];
            Array.Copy(_buf, next, _len);
            _buf = next;
        }

        public void U8(int v)
        {
            Ensure(1);
            _buf[_len++] = (byte)(v & 0xFF);
        }

        public void U16(int v)
        {
            Ensure(2);
            _buf[_len++] = (byte)(v & 0xFF);
            _buf[_len++] = (byte)((v >> 8) & 0xFF);
        }

        public void U32(long v)
        {
            Ensure(4);
            _buf[_len++] = (byte)(v & 0xFF);
            _buf[_len++] = (byte)((v >> 8) & 0xFF);
            _buf[_len++] = (byte)((v >> 16) & 0xFF);
            _buf[_len++] = (byte)((v >> 24) & 0xFF);
        }

        public void I32(int v) => U32(unchecked((uint)v));

        public void U64(ulong v)
        {
            Ensure(8);
            for (int i = 0; i < 8; i++) _buf[_len++] = (byte)((v >> (i * 8)) & 0xFF);
        }

        public void I64(long v) => U64(unchecked((ulong)v));

        public void F64(double v) => U64(unchecked((ulong)BitConverter.DoubleToInt64Bits(v)));

        /// <summary>A 128-bit unsigned integer as 16 little-endian bytes.</summary>
        public void U128(ulong lo, ulong hi)
        {
            U64(lo);
            U64(hi);
        }

        public void Raw(byte[] bytes)
        {
            Ensure(bytes.Length);
            Array.Copy(bytes, 0, _buf, _len, bytes.Length);
            _len += bytes.Length;
        }

        /// <summary>A UTF-8 string with a u16 length prefix.</summary>
        public void StrU16(string s)
        {
            var b = Encoding.UTF8.GetBytes(s);
            U16(b.Length);
            Raw(b);
        }

        /// <summary>A UTF-8 string with a u32 length prefix.</summary>
        public void StrU32(string s)
        {
            var b = Encoding.UTF8.GetBytes(s);
            U32(b.Length);
            Raw(b);
        }

        /// <summary>A byte string with a u16 length prefix.</summary>
        public void BytesU16(byte[] b)
        {
            U16(b.Length);
            Raw(b);
        }

        /// <summary>A byte string with a u32 length prefix.</summary>
        public void BytesU32(byte[] b)
        {
            U32(b.Length);
            Raw(b);
        }

        /// <summary>The written bytes (a fresh copy).</summary>
        public byte[] Out()
        {
            var o = new byte[_len];
            Array.Copy(_buf, o, _len);
            return o;
        }
    }

    /// <summary>A bounds-checked little-endian reader over a byte array.</summary>
    public sealed class Reader
    {
        private readonly byte[] _buf;
        private int _p;

        public Reader(byte[] buf)
        {
            _buf = buf;
            _p = 0;
        }

        private void Need(int n)
        {
            if (_p + n > _buf.Length)
                throw new ProtocolException($"truncated: need {n} bytes at offset {_p}");
        }

        public int U8()
        {
            Need(1);
            return _buf[_p++];
        }

        public int U16()
        {
            Need(2);
            int v = _buf[_p] | (_buf[_p + 1] << 8);
            _p += 2;
            return v;
        }

        public long U32()
        {
            Need(4);
            long v = (uint)(_buf[_p] | (_buf[_p + 1] << 8) | (_buf[_p + 2] << 16) | (_buf[_p + 3] << 24));
            _p += 4;
            return v;
        }

        public int I32()
        {
            Need(4);
            int v = _buf[_p] | (_buf[_p + 1] << 8) | (_buf[_p + 2] << 16) | (_buf[_p + 3] << 24);
            _p += 4;
            return v;
        }

        public ulong U64()
        {
            Need(8);
            ulong v = 0;
            for (int i = 0; i < 8; i++) v |= (ulong)_buf[_p + i] << (i * 8);
            _p += 8;
            return v;
        }

        public long I64() => unchecked((long)U64());

        public double F64() => BitConverter.Int64BitsToDouble(unchecked((long)U64()));

        /// <summary>Returns the (lo, hi) 64-bit halves of a 128-bit value.</summary>
        public (ulong Lo, ulong Hi) U128()
        {
            ulong lo = U64();
            ulong hi = U64();
            return (lo, hi);
        }

        public byte[] Raw(int n)
        {
            Need(n);
            var s = new byte[n];
            Array.Copy(_buf, _p, s, 0, n);
            _p += n;
            return s;
        }

        public string StrU16() => Encoding.UTF8.GetString(Raw(U16()));

        public string StrU32() => Encoding.UTF8.GetString(Raw((int)U32()));

        public byte[] BytesU16() => Raw(U16());

        public byte[] BytesU32() => Raw((int)U32());

        public int Remaining => _buf.Length - _p;

        /// <summary>Throw unless every byte has been consumed.</summary>
        public void ExpectEnd()
        {
            if (Remaining != 0)
                throw new ProtocolException($"{Remaining} trailing byte(s) after message");
        }
    }

    public static class Frame
    {
        /// <summary>Wrap a payload in a [len:u32][payload] frame.</summary>
        public static byte[] Encode(byte[] payload)
        {
            var o = new byte[4 + payload.Length];
            uint len = (uint)payload.Length;
            o[0] = (byte)(len & 0xFF);
            o[1] = (byte)((len >> 8) & 0xFF);
            o[2] = (byte)((len >> 16) & 0xFF);
            o[3] = (byte)((len >> 24) & 0xFF);
            Array.Copy(payload, 0, o, 4, payload.Length);
            return o;
        }
    }
}
