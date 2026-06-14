// The scalar value model and its tagged/untagged wire codec.
//
// Mirrors crates/prism-protocol/src/data.rs (Value) and the type tags in
// docs/specs/record-format.md. The SDK accepts plain CLR values (boxed as
// object?) and maps them to wire types; on decode it returns plain CLR values.

using System;

namespace PrismDb
{
    /// <summary>Record-format type tags.</summary>
    public static class Tag
    {
        public const int Null = 0x00;
        public const int Bool = 0x01;
        public const int Int32 = 0x02;
        public const int Int64 = 0x03;
        public const int Double = 0x04;
        public const int String = 0x05;
        public const int Binary = 0x06;
        public const int Timestamp = 0x09;
        public const int ObjectId = 0x0A;
    }

    /// <summary>A 12-byte document identifier.</summary>
    public sealed class ObjectId : IEquatable<ObjectId>
    {
        public byte[] Bytes { get; }

        public ObjectId(byte[] bytes)
        {
            if (bytes.Length != 12) throw new ProtocolException("ObjectId must be 12 bytes");
            Bytes = (byte[])bytes.Clone();
        }

        /// <summary>Lowercase 24-character hex.</summary>
        public string ToHex()
        {
            var c = new char[24];
            const string hex = "0123456789abcdef";
            for (int i = 0; i < 12; i++)
            {
                c[i * 2] = hex[Bytes[i] >> 4];
                c[i * 2 + 1] = hex[Bytes[i] & 0xF];
            }
            return new string(c);
        }

        public static ObjectId FromHex(string hex)
        {
            if (hex.Length != 24) throw new ProtocolException("ObjectId hex must be 24 chars");
            var b = new byte[12];
            for (int i = 0; i < 12; i++)
                b[i] = Convert.ToByte(hex.Substring(i * 2, 2), 16);
            return new ObjectId(b);
        }

        public override string ToString() => ToHex();

        public bool Equals(ObjectId? other)
        {
            if (other is null || other.Bytes.Length != Bytes.Length) return false;
            for (int i = 0; i < Bytes.Length; i++)
                if (Bytes[i] != other.Bytes[i]) return false;
            return true;
        }

        public override bool Equals(object? obj) => Equals(obj as ObjectId);

        public override int GetHashCode()
        {
            int h = 17;
            foreach (var x in Bytes) h = h * 31 + x;
            return h;
        }
    }

    /// <summary>An explicitly-typed value, for cases where the default mapping of a
    /// CLR value is not what you want (e.g. a 32-bit int, a float that happens to
    /// be integral, or a timestamp). Build with the <see cref="Prism"/> helpers.</summary>
    public sealed class Typed
    {
        public int TypeTag { get; }
        public object Value { get; }

        public Typed(int tag, object value)
        {
            TypeTag = tag;
            Value = value;
        }
    }

    /// <summary>Helpers to build explicitly-typed values.</summary>
    public static class Prism
    {
        /// <summary>Force a value to wire Int32.</summary>
        public static Typed Int32(int n) => new Typed(Tag.Int32, n);
        /// <summary>Force a value to wire Int64.</summary>
        public static Typed Int64(long n) => new Typed(Tag.Int64, n);
        /// <summary>Force a value to wire Double.</summary>
        public static Typed Float64(double n) => new Typed(Tag.Double, n);
        /// <summary>Force a value to wire Timestamp (microseconds since the Unix epoch).</summary>
        public static Typed Timestamp(long us) => new Typed(Tag.Timestamp, us);
    }

    internal static class ValueCodec
    {
        private static readonly DateTime Epoch = new DateTime(1970, 1, 1, 0, 0, 0, DateTimeKind.Utc);

        /// <summary>Resolve a CLR value to its wire type tag. Integer types map to
        /// Int64 to match the reference SDK; use Prism.Int32 to force Int32.</summary>
        public static int TagOf(object? v)
        {
            switch (v)
            {
                case null: return Tag.Null;
                case Typed t: return t.TypeTag;
                case ObjectId: return Tag.ObjectId;
                case bool: return Tag.Bool;
                case sbyte:
                case byte:
                case short:
                case ushort:
                case int:
                case uint:
                case long:
                case ulong: return Tag.Int64;
                case float:
                case double: return Tag.Double;
                case string: return Tag.String;
                case DateTime:
                case DateTimeOffset: return Tag.Timestamp;
                case byte[]: return Tag.Binary;
            }
            throw new ProtocolException($"unsupported value: {v.GetType().Name}");
        }

        private static long ToLong(object v) => Convert.ToInt64(v);
        private static double ToDouble(object v) => Convert.ToDouble(v);

        private static long EpochMicros(object v)
        {
            DateTimeOffset dto;
            if (v is DateTimeOffset d)
            {
                dto = d;
            }
            else
            {
                var dt = (DateTime)v;
                if (dt.Kind == DateTimeKind.Unspecified) dt = DateTime.SpecifyKind(dt, DateTimeKind.Utc);
                dto = new DateTimeOffset(dt);
            }
            return (dto.UtcDateTime - Epoch).Ticks / 10; // 100ns ticks -> microseconds
        }

        public static void EncodeUntagged(Writer w, int tag, object? v)
        {
            object? raw = v is Typed t ? t.Value : v;
            switch (tag)
            {
                case Tag.Null: break;
                case Tag.Bool: w.U8((bool)raw! ? 1 : 0); break;
                case Tag.Int32: w.I32((int)ToLong(raw!)); break;
                case Tag.Int64: w.I64(ToLong(raw!)); break;
                case Tag.Double: w.F64(ToDouble(raw!)); break;
                case Tag.Timestamp:
                    w.I64(raw is DateTime || raw is DateTimeOffset ? EpochMicros(raw!) : ToLong(raw!));
                    break;
                case Tag.String: w.StrU32((string)raw!); break;
                case Tag.ObjectId: w.Raw(((ObjectId)raw!).Bytes); break;
                case Tag.Binary:
                    var b = (byte[])raw!;
                    w.U32(b.Length);
                    w.U8(0); // subtype
                    w.Raw(b);
                    break;
                default:
                    throw new ProtocolException($"cannot encode value tag 0x{tag:x}");
            }
        }

        public static void EncodeTagged(Writer w, object? v)
        {
            int tag = TagOf(v);
            w.U8(tag);
            EncodeUntagged(w, tag, v);
        }

        public static object? DecodeUntagged(Reader r, int tag)
        {
            switch (tag)
            {
                case Tag.Null: return null;
                case Tag.Bool: return r.U8() != 0;
                case Tag.Int32: return r.I32();
                case Tag.Int64: return r.I64();
                case Tag.Double: return r.F64();
                case Tag.Timestamp: return r.I64();
                case Tag.String: return r.StrU32();
                case Tag.ObjectId: return new ObjectId(r.Raw(12));
                case Tag.Binary:
                    int len = (int)r.U32();
                    r.U8(); // subtype (discarded)
                    return r.Raw(len);
                default:
                    throw new ProtocolException($"unknown value tag 0x{tag:x}");
            }
        }

        public static object? DecodeTagged(Reader r) => DecodeUntagged(r, r.U8());
    }
}
