// Document update operators and their wire encoding.
//
// Mirrors prism_protocol::DocUpdate: an ordered list of $set / $unset / $inc
// mutations. Build with the U helpers. Operand values reuse the tagged Value
// encoding. Carried as the `update` blob of an update command.

using System.Collections.Generic;

namespace PrismDb
{
    /// <summary>One field mutation. Construct via the <see cref="U"/> helpers.</summary>
    public sealed class DocUpdate
    {
        internal string Op = "";
        internal string Field = "";
        internal object? Value;
        internal long Delta;
    }

    /// <summary>Update builders mirroring the engine's update operators.</summary>
    public static class U
    {
        /// <summary>$set - set <paramref name="field"/> to <paramref name="value"/>.</summary>
        public static DocUpdate Set(string field, object? value) =>
            new DocUpdate { Op = "set", Field = field, Value = value };

        /// <summary>$unset - remove <paramref name="field"/>.</summary>
        public static DocUpdate Unset(string field) =>
            new DocUpdate { Op = "unset", Field = field };

        /// <summary>$inc - add <paramref name="delta"/> to the integer <paramref name="field"/>.</summary>
        public static DocUpdate Inc(string field, long delta) =>
            new DocUpdate { Op = "inc", Field = field, Delta = delta };
    }

    internal static class UpdateCodec
    {
        public static byte[] Encode(IReadOnlyList<DocUpdate> ops)
        {
            var w = new Writer();
            w.U32(ops.Count);
            foreach (var op in ops)
            {
                switch (op.Op)
                {
                    case "set":
                        w.U8(1);
                        w.StrU16(op.Field);
                        ValueCodec.EncodeTagged(w, op.Value);
                        break;
                    case "unset":
                        w.U8(2);
                        w.StrU16(op.Field);
                        break;
                    case "inc":
                        w.U8(3);
                        w.StrU16(op.Field);
                        w.I64(op.Delta);
                        break;
                }
            }
            return w.Out();
        }
    }
}
