// The document tagged-binary codec.
//
// Mirrors crates/prism-doc/src/value.rs (Document::encode/decode). A document is
// [total:u32][count:u16] followed by, per field, [tag:u8][nameLen:u16][name]
// [value bytes]. Field value bytes use the same encoding as scalar values,
// except documents have no Binary type.

using System.Collections.Generic;

namespace PrismDb
{
    /// <summary>A document is an ordered string-keyed map of values.</summary>
    public sealed class Document : Dictionary<string, object?>
    {
        public Document() { }
        public Document(IDictionary<string, object?> src) : base(src) { }
    }

    public static class DocumentCodec
    {
        /// <summary>Encode a document to its tagged-binary payload.</summary>
        public static byte[] Encode(IDictionary<string, object?> doc)
        {
            var body = new Writer();
            if (doc.Count > 0xFFFF) throw new ProtocolException("too many document fields");
            body.U16(doc.Count);
            foreach (var kv in doc)
            {
                int tag = ValueCodec.TagOf(kv.Value);
                if (tag == Tag.Binary)
                    throw new ProtocolException($"field \"{kv.Key}\": binary values are not supported in documents");
                body.U8(tag);
                body.StrU16(kv.Key);
                ValueCodec.EncodeUntagged(body, tag, kv.Value);
            }
            var inner = body.Out();
            var outw = new Writer(inner.Length + 4);
            outw.U32(4 + inner.Length); // total length, including this u32
            outw.Raw(inner);
            return outw.Out();
        }

        /// <summary>Decode a document from its tagged-binary payload.</summary>
        public static Document Decode(byte[] bytes)
        {
            var r = new Reader(bytes);
            r.U32(); // total length (redundant with the frame's blob length)
            int count = r.U16();
            var doc = new Document();
            for (int i = 0; i < count; i++)
            {
                int tag = r.U8();
                string name = r.StrU16();
                doc[name] = ValueCodec.DecodeUntagged(r, tag);
            }
            return doc;
        }
    }
}
