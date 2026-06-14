// The document query filter and its wire encoding.
//
// Mirrors prism_protocol::DocQuery (crates/prism-protocol/src/data.rs): a tag
// byte, then the operator-specific body. Operand values reuse the tagged Value
// encoding. Build queries with the Q helpers.

using System.Collections.Generic;

namespace PrismDb
{
    /// <summary>A document query filter. Construct via the <see cref="Q"/> helpers.</summary>
    public sealed class DocQuery
    {
        internal const int TAll = 0, TEq = 1, TNe = 2, TGt = 3, TLt = 4, TGte = 5, TLte = 6,
            TIn = 7, TNin = 8, TExists = 9, TAnd = 10, TOr = 11, TNot = 12;

        internal string Kind = "";
        internal int Tag;
        internal string Field = "";
        internal object? Value;
        internal IReadOnlyList<object?>? Values;
        internal bool Present;
        internal IReadOnlyList<DocQuery>? Subs;
        internal DocQuery? Sub;
    }

    /// <summary>Query builders mirroring the engine's filter set.</summary>
    public static class Q
    {
        public static DocQuery All() => new DocQuery { Kind = "all" };
        public static DocQuery Eq(string field, object? value) => Field(DocQuery.TEq, field, value);
        public static DocQuery Ne(string field, object? value) => Field(DocQuery.TNe, field, value);
        public static DocQuery Gt(string field, object? value) => Field(DocQuery.TGt, field, value);
        public static DocQuery Lt(string field, object? value) => Field(DocQuery.TLt, field, value);
        public static DocQuery Gte(string field, object? value) => Field(DocQuery.TGte, field, value);
        public static DocQuery Lte(string field, object? value) => Field(DocQuery.TLte, field, value);
        public static DocQuery In(string field, IEnumerable<object?> values) => Set(DocQuery.TIn, field, values);
        public static DocQuery Nin(string field, IEnumerable<object?> values) => Set(DocQuery.TNin, field, values);
        public static DocQuery Exists(string field, bool present = true) =>
            new DocQuery { Kind = "exists", Field = field, Present = present };
        public static DocQuery And(params DocQuery[] subs) =>
            new DocQuery { Kind = "group", Tag = DocQuery.TAnd, Subs = subs };
        public static DocQuery Or(params DocQuery[] subs) =>
            new DocQuery { Kind = "group", Tag = DocQuery.TOr, Subs = subs };
        public static DocQuery Not(DocQuery sub) => new DocQuery { Kind = "not", Sub = sub };

        private static DocQuery Field(int tag, string field, object? value) =>
            new DocQuery { Kind = "field", Tag = tag, Field = field, Value = value };

        private static DocQuery Set(int tag, string field, IEnumerable<object?> values) =>
            new DocQuery { Kind = "set", Tag = tag, Field = field, Values = new List<object?>(values) };
    }

    internal static class QueryCodec
    {
        public static byte[] Encode(DocQuery q)
        {
            var w = new Writer();
            EncodeInto(w, q);
            return w.Out();
        }

        private static void EncodeInto(Writer w, DocQuery q)
        {
            switch (q.Kind)
            {
                case "all":
                    w.U8(DocQuery.TAll);
                    break;
                case "field":
                    w.U8(q.Tag);
                    w.StrU16(q.Field);
                    ValueCodec.EncodeTagged(w, q.Value);
                    break;
                case "set":
                    w.U8(q.Tag);
                    w.StrU16(q.Field);
                    w.U32(q.Values!.Count);
                    foreach (var v in q.Values!) ValueCodec.EncodeTagged(w, v);
                    break;
                case "exists":
                    w.U8(DocQuery.TExists);
                    w.StrU16(q.Field);
                    w.U8(q.Present ? 1 : 0);
                    break;
                case "group":
                    w.U8(q.Tag);
                    w.U32(q.Subs!.Count);
                    foreach (var s in q.Subs!) EncodeInto(w, s);
                    break;
                case "not":
                    w.U8(DocQuery.TNot);
                    EncodeInto(w, q.Sub!);
                    break;
            }
        }
    }
}
