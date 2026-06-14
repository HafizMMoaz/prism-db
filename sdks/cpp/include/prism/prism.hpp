// prism/prism.hpp — a modern C++17 client for PrismDB over the binary wire
// protocol (docs/specs/wire-protocol.md). The value model, codec, and message
// (de)serialisation live here inline so they can be unit-tested without linking
// or a socket; the transport and high-level Client are implemented in prism.cpp.
//
// Byte layouts mirror crates/prism-protocol and the reference Node SDK exactly.
#ifndef PRISM_PRISM_HPP
#define PRISM_PRISM_HPP

#include <array>
#include <cstdint>
#include <cstring>
#include <initializer_list>
#include <memory>
#include <optional>
#include <stdexcept>
#include <string>
#include <string_view>
#include <type_traits>
#include <utility>
#include <variant>
#include <vector>

namespace prism {

// ---- errors --------------------------------------------------------------

class Error : public std::runtime_error {
public:
    explicit Error(const std::string& m) : std::runtime_error(m) {}
};

// A malformed frame/message, or a byte-level decode failure.
class ProtocolError : public Error {
public:
    explicit ProtocolError(const std::string& m) : Error(m) {}
};

// The structured error trailer a server attaches to a non-OK response.
struct ErrorInfo {
    std::uint32_t code = 0;       // wire error code
    std::string message;
    std::string sqlstate;        // 5-char SQLSTATE
    std::string detail;
    std::uint32_t position = 0;  // character offset in source SQL, or 0
};

// An error returned by the server (status != 0), carrying its trailer.
class ServerError : public Error {
public:
    ErrorInfo info;
    explicit ServerError(ErrorInfo i)
        : Error(i.message.empty() ? ("server error 0x" + std::to_string(i.code)) : i.message),
          info(std::move(i)) {}
};

// ---- record-format type tags --------------------------------------------

enum class Tag : int {
    Null = 0x00,
    Bool = 0x01,
    Int32 = 0x02,
    Int64 = 0x03,
    Double = 0x04,
    String = 0x05,
    Binary = 0x06,
    Timestamp = 0x09,
    ObjectId = 0x0A,
};

// ---- ObjectId ------------------------------------------------------------

class ObjectId {
public:
    ObjectId() = default;
    explicit ObjectId(const std::array<std::uint8_t, 12>& b) : b_(b) {}
    explicit ObjectId(const std::uint8_t* p) { std::memcpy(b_.data(), p, 12); }

    const std::array<std::uint8_t, 12>& bytes() const { return b_; }

    std::string toHex() const {
        static const char* H = "0123456789abcdef";
        std::string s(24, '0');
        for (int i = 0; i < 12; i++) {
            s[i * 2] = H[b_[i] >> 4];
            s[i * 2 + 1] = H[b_[i] & 0xf];
        }
        return s;
    }
    static ObjectId fromHex(std::string_view hex) {
        if (hex.size() != 24) throw ProtocolError("ObjectId hex must be 24 chars");
        auto nib = [](char c) -> int {
            if (c >= '0' && c <= '9') return c - '0';
            if (c >= 'a' && c <= 'f') return c - 'a' + 10;
            if (c >= 'A' && c <= 'F') return c - 'A' + 10;
            throw ProtocolError("ObjectId hex has a non-hex character");
        };
        std::array<std::uint8_t, 12> b{};
        for (int i = 0; i < 12; i++)
            b[i] = static_cast<std::uint8_t>((nib(hex[i * 2]) << 4) | nib(hex[i * 2 + 1]));
        return ObjectId(b);
    }
    bool operator==(const ObjectId& o) const { return b_ == o.b_; }
    bool operator!=(const ObjectId& o) const { return !(*this == o); }

private:
    std::array<std::uint8_t, 12> b_{};
};

// Wrappers that pin a wire type the default mapping would not pick.
struct Int32 { std::int32_t v; };
struct Timestamp { std::int64_t micros; };  // microseconds since the Unix epoch

// ---- Value ---------------------------------------------------------------

class Value {
public:
    using Var = std::variant<std::monostate, bool, Int32, std::int64_t, double,
                             std::string, std::vector<std::uint8_t>, Timestamp, ObjectId>;

    Value() = default;                       // Null
    Value(std::nullptr_t) {}                 // Null
    Value(bool b) : v_(b) {}
    template <class T, std::enable_if_t<std::is_integral_v<T> && !std::is_same_v<T, bool>, int> = 0>
    Value(T n) : v_(static_cast<std::int64_t>(n)) {}  // integers default to Int64
    Value(double d) : v_(d) {}
    Value(const char* s) : v_(std::string(s)) {}
    Value(std::string s) : v_(std::move(s)) {}
    Value(std::string_view s) : v_(std::string(s)) {}
    Value(std::vector<std::uint8_t> b) : v_(std::move(b)) {}
    Value(Int32 i) : v_(i) {}
    Value(Timestamp t) : v_(t) {}
    Value(ObjectId o) : v_(std::move(o)) {}

    const Var& var() const { return v_; }
    bool isNull() const { return v_.index() == 0; }

    Tag tag() const {
        switch (v_.index()) {
            case 0: return Tag::Null;
            case 1: return Tag::Bool;
            case 2: return Tag::Int32;
            case 3: return Tag::Int64;
            case 4: return Tag::Double;
            case 5: return Tag::String;
            case 6: return Tag::Binary;
            case 7: return Tag::Timestamp;
            case 8: return Tag::ObjectId;
        }
        return Tag::Null;
    }

    bool asBool() const { return get<bool>("bool"); }
    std::int32_t asInt32() const { return std::get<Int32>(checked(2, "int32")).v; }
    std::int64_t asInt64() const {
        if (v_.index() == 3) return std::get<std::int64_t>(v_);
        if (v_.index() == 7) return std::get<Timestamp>(v_).micros;
        throw ProtocolError("value is not an int64/timestamp");
    }
    double asDouble() const { return get<double>("double"); }
    const std::string& asString() const { return get<std::string>("string"); }
    const std::vector<std::uint8_t>& asBytes() const {
        return get<std::vector<std::uint8_t>>("binary");
    }
    const ObjectId& asObjectId() const { return get<ObjectId>("objectid"); }

private:
    template <class T> const T& get(const char* what) const {
        if (auto* p = std::get_if<T>(&v_)) return *p;
        throw ProtocolError(std::string("value is not a ") + what);
    }
    const Var& checked(std::size_t idx, const char* what) const {
        if (v_.index() != idx) throw ProtocolError(std::string("value is not a ") + what);
        return v_;
    }
    Var v_{};
};

// ---- low-level codec -----------------------------------------------------

namespace detail {

class Writer {
public:
    void u8(std::uint8_t v) { buf_.push_back(v); }
    void u16(std::uint16_t v) {
        buf_.push_back(std::uint8_t(v & 0xff));
        buf_.push_back(std::uint8_t((v >> 8) & 0xff));
    }
    void u32(std::uint32_t v) {
        for (int i = 0; i < 4; i++) buf_.push_back(std::uint8_t((v >> (8 * i)) & 0xff));
    }
    void i32(std::int32_t v) { u32(static_cast<std::uint32_t>(v)); }
    void u64(std::uint64_t v) {
        for (int i = 0; i < 8; i++) buf_.push_back(std::uint8_t((v >> (8 * i)) & 0xff));
    }
    void i64(std::int64_t v) { u64(static_cast<std::uint64_t>(v)); }
    void f64(double d) {
        std::uint64_t u;
        std::memcpy(&u, &d, 8);
        u64(u);
    }
    void u128(std::uint64_t lo) { u64(lo); u64(0); }
    void raw(const void* p, std::size_t n) {
        const auto* b = static_cast<const std::uint8_t*>(p);
        buf_.insert(buf_.end(), b, b + n);
    }
    void strU16(std::string_view s) { u16(static_cast<std::uint16_t>(s.size())); raw(s.data(), s.size()); }
    void strU32(std::string_view s) { u32(static_cast<std::uint32_t>(s.size())); raw(s.data(), s.size()); }
    void bytesU16(const std::vector<std::uint8_t>& b) { u16(static_cast<std::uint16_t>(b.size())); raw(b.data(), b.size()); }
    void bytesU32(const std::vector<std::uint8_t>& b) { u32(static_cast<std::uint32_t>(b.size())); raw(b.data(), b.size()); }

    std::vector<std::uint8_t>& out() { return buf_; }
    const std::vector<std::uint8_t>& out() const { return buf_; }

private:
    std::vector<std::uint8_t> buf_;
};

class Reader {
public:
    Reader(const std::uint8_t* p, std::size_t n) : p_(p), len_(n) {}
    explicit Reader(const std::vector<std::uint8_t>& v) : p_(v.data()), len_(v.size()) {}

    std::uint8_t u8() { need(1); return p_[off_++]; }
    std::uint16_t u16() {
        need(2);
        std::uint16_t v = std::uint16_t(p_[off_] | (p_[off_ + 1] << 8));
        off_ += 2;
        return v;
    }
    std::uint32_t u32() {
        need(4);
        std::uint32_t v = 0;
        for (int i = 0; i < 4; i++) v |= std::uint32_t(p_[off_ + i]) << (8 * i);
        off_ += 4;
        return v;
    }
    std::int32_t i32() { return static_cast<std::int32_t>(u32()); }
    std::uint64_t u64() {
        need(8);
        std::uint64_t v = 0;
        for (int i = 0; i < 8; i++) v |= std::uint64_t(p_[off_ + i]) << (8 * i);
        off_ += 8;
        return v;
    }
    std::int64_t i64() { return static_cast<std::int64_t>(u64()); }
    double f64() {
        std::uint64_t u = u64();
        double d;
        std::memcpy(&d, &u, 8);
        return d;
    }
    void skip(std::size_t n) { need(n); off_ += n; }
    const std::uint8_t* raw(std::size_t n) {
        need(n);
        const std::uint8_t* p = p_ + off_;
        off_ += n;
        return p;
    }
    std::string strU16() { auto n = u16(); return std::string(reinterpret_cast<const char*>(raw(n)), n); }
    std::string strU32() { auto n = u32(); return std::string(reinterpret_cast<const char*>(raw(n)), n); }
    std::vector<std::uint8_t> bytesU16() { auto n = u16(); const auto* p = raw(n); return {p, p + n}; }
    std::vector<std::uint8_t> bytesU32() { auto n = u32(); const auto* p = raw(n); return {p, p + n}; }

    std::size_t remaining() const { return len_ - off_; }
    void expectEnd() const {
        if (remaining() != 0) throw ProtocolError(std::to_string(remaining()) + " trailing byte(s) after message");
    }

private:
    void need(std::size_t n) {
        if (off_ + n > len_) throw ProtocolError("truncated: need " + std::to_string(n) + " bytes at offset " + std::to_string(off_));
    }
    const std::uint8_t* p_;
    std::size_t len_;
    std::size_t off_ = 0;
};

inline std::vector<std::uint8_t> frameEncode(const std::vector<std::uint8_t>& payload) {
    std::vector<std::uint8_t> out;
    out.reserve(4 + payload.size());
    std::uint32_t n = static_cast<std::uint32_t>(payload.size());
    for (int i = 0; i < 4; i++) out.push_back(std::uint8_t((n >> (8 * i)) & 0xff));
    out.insert(out.end(), payload.begin(), payload.end());
    return out;
}

// ---- value codec ----------------------------------------------------------

inline void encodeUntagged(Writer& w, const Value& v) {
    switch (v.tag()) {
        case Tag::Null: break;
        case Tag::Bool: w.u8(v.asBool() ? 1 : 0); break;
        case Tag::Int32: w.i32(v.asInt32()); break;
        case Tag::Int64: w.i64(v.asInt64()); break;
        case Tag::Double: w.f64(v.asDouble()); break;
        case Tag::Timestamp: w.i64(v.asInt64()); break;
        case Tag::String: w.strU32(v.asString()); break;
        case Tag::ObjectId: w.raw(v.asObjectId().bytes().data(), 12); break;
        case Tag::Binary: {
            const auto& b = v.asBytes();
            w.u32(static_cast<std::uint32_t>(b.size()));
            w.u8(0);  // subtype
            w.raw(b.data(), b.size());
            break;
        }
    }
}

inline void encodeTagged(Writer& w, const Value& v) {
    w.u8(static_cast<std::uint8_t>(v.tag()));
    encodeUntagged(w, v);
}

inline Value decodeUntagged(Reader& r, int tag) {
    switch (static_cast<Tag>(tag)) {
        case Tag::Null: return Value();
        case Tag::Bool: return Value(r.u8() != 0);
        case Tag::Int32: return Value(Int32{r.i32()});
        case Tag::Int64: return Value(static_cast<std::int64_t>(r.i64()));
        case Tag::Double: return Value(r.f64());
        case Tag::Timestamp: return Value(Timestamp{r.i64()});
        case Tag::String: return Value(r.strU32());
        case Tag::ObjectId: return Value(ObjectId(r.raw(12)));
        case Tag::Binary: {
            auto n = r.u32();
            r.u8();  // subtype
            const auto* p = r.raw(n);
            return Value(std::vector<std::uint8_t>(p, p + n));
        }
    }
    throw ProtocolError("unknown value tag 0x" + std::to_string(tag));
}

inline Value decodeTagged(Reader& r) { return decodeUntagged(r, r.u8()); }

}  // namespace detail

// ---- Document ------------------------------------------------------------

class Document {
public:
    using Field = std::pair<std::string, Value>;

    Document() = default;
    Document(std::initializer_list<Field> init) : fields_(init) {}

    Document& set(std::string key, Value v) {
        fields_.emplace_back(std::move(key), std::move(v));
        return *this;
    }
    bool contains(const std::string& key) const { return find(key) != nullptr; }
    const Value* find(const std::string& key) const {
        for (const auto& f : fields_)
            if (f.first == key) return &f.second;
        return nullptr;
    }
    const Value& at(const std::string& key) const {
        if (const Value* v = find(key)) return *v;
        throw ProtocolError("no such document field: " + key);
    }
    std::size_t size() const { return fields_.size(); }
    auto begin() const { return fields_.begin(); }
    auto end() const { return fields_.end(); }

    std::vector<std::uint8_t> encode() const {
        if (fields_.size() > 0xFFFF) throw ProtocolError("too many document fields");
        detail::Writer body;
        body.u16(static_cast<std::uint16_t>(fields_.size()));
        for (const auto& f : fields_) {
            if (f.second.tag() == Tag::Binary)
                throw ProtocolError("field \"" + f.first + "\": binary values are not supported in documents");
            body.u8(static_cast<std::uint8_t>(f.second.tag()));
            body.strU16(f.first);
            detail::encodeUntagged(body, f.second);
        }
        detail::Writer out;
        out.u32(static_cast<std::uint32_t>(4 + body.out().size()));
        out.raw(body.out().data(), body.out().size());
        return out.out();
    }

    static Document decode(const std::uint8_t* p, std::size_t n) {
        detail::Reader r(p, n);
        r.u32();  // total length
        auto count = r.u16();
        Document d;
        for (std::uint16_t i = 0; i < count; i++) {
            auto tag = r.u8();
            auto name = r.strU16();
            d.set(std::move(name), detail::decodeUntagged(r, tag));
        }
        return d;
    }

private:
    std::vector<Field> fields_;
};

// ---- Query ---------------------------------------------------------------

class Query {
public:
    static Query all() { return Query(Kind::All, 0); }
    static Query eq(std::string f, Value v) { return field(1, std::move(f), std::move(v)); }
    static Query ne(std::string f, Value v) { return field(2, std::move(f), std::move(v)); }
    static Query gt(std::string f, Value v) { return field(3, std::move(f), std::move(v)); }
    static Query lt(std::string f, Value v) { return field(4, std::move(f), std::move(v)); }
    static Query gte(std::string f, Value v) { return field(5, std::move(f), std::move(v)); }
    static Query lte(std::string f, Value v) { return field(6, std::move(f), std::move(v)); }
    static Query in(std::string f, std::vector<Value> vs) { return set(7, std::move(f), std::move(vs)); }
    static Query nin(std::string f, std::vector<Value> vs) { return set(8, std::move(f), std::move(vs)); }
    static Query exists(std::string f, bool present = true) {
        Query q(Kind::Exists, 9);
        q.field_ = std::move(f);
        q.present_ = present;
        return q;
    }
    static Query and_(std::vector<Query> subs) { return group(10, std::move(subs)); }
    static Query or_(std::vector<Query> subs) { return group(11, std::move(subs)); }
    template <class... Qs> static Query and_(Qs... qs) { return and_(std::vector<Query>{std::move(qs)...}); }
    template <class... Qs> static Query or_(Qs... qs) { return or_(std::vector<Query>{std::move(qs)...}); }
    static Query not_(Query sub) {
        Query q(Kind::Not, 12);
        q.subs_.push_back(std::move(sub));
        return q;
    }

    std::vector<std::uint8_t> encode() const {
        detail::Writer w;
        encodeInto(w);
        return w.out();
    }
    void encodeInto(detail::Writer& w) const {
        switch (kind_) {
            case Kind::All: w.u8(0); break;
            case Kind::Field:
                w.u8(static_cast<std::uint8_t>(tag_));
                w.strU16(field_);
                detail::encodeTagged(w, value_);
                break;
            case Kind::Set:
                w.u8(static_cast<std::uint8_t>(tag_));
                w.strU16(field_);
                w.u32(static_cast<std::uint32_t>(values_.size()));
                for (const auto& v : values_) detail::encodeTagged(w, v);
                break;
            case Kind::Exists:
                w.u8(9);
                w.strU16(field_);
                w.u8(present_ ? 1 : 0);
                break;
            case Kind::Group:
                w.u8(static_cast<std::uint8_t>(tag_));
                w.u32(static_cast<std::uint32_t>(subs_.size()));
                for (const auto& s : subs_) s.encodeInto(w);
                break;
            case Kind::Not:
                w.u8(12);
                subs_.front().encodeInto(w);
                break;
        }
    }

private:
    enum class Kind { All, Field, Set, Exists, Group, Not };
    Query(Kind k, int t) : kind_(k), tag_(t) {}
    static Query field(int t, std::string f, Value v) {
        Query q(Kind::Field, t);
        q.field_ = std::move(f);
        q.value_ = std::move(v);
        return q;
    }
    static Query set(int t, std::string f, std::vector<Value> vs) {
        Query q(Kind::Set, t);
        q.field_ = std::move(f);
        q.values_ = std::move(vs);
        return q;
    }
    static Query group(int t, std::vector<Query> subs) {
        Query q(Kind::Group, t);
        q.subs_ = std::move(subs);
        return q;
    }

    Kind kind_;
    int tag_;
    std::string field_;
    Value value_;
    std::vector<Value> values_;
    bool present_ = true;
    std::vector<Query> subs_;
};

// ---- Update --------------------------------------------------------------

class Update {
public:
    Update& set(std::string field, Value v) {
        ops_.push_back({Op::Set, std::move(field), std::move(v), 0});
        return *this;
    }
    Update& unset(std::string field) {
        ops_.push_back({Op::Unset, std::move(field), Value(), 0});
        return *this;
    }
    Update& inc(std::string field, std::int64_t delta) {
        ops_.push_back({Op::Inc, std::move(field), Value(), delta});
        return *this;
    }

    std::vector<std::uint8_t> encode() const {
        detail::Writer w;
        w.u32(static_cast<std::uint32_t>(ops_.size()));
        for (const auto& o : ops_) {
            switch (o.op) {
                case Op::Set:
                    w.u8(1);
                    w.strU16(o.field);
                    detail::encodeTagged(w, o.value);
                    break;
                case Op::Unset:
                    w.u8(2);
                    w.strU16(o.field);
                    break;
                case Op::Inc:
                    w.u8(3);
                    w.strU16(o.field);
                    w.i64(o.delta);
                    break;
            }
        }
        return w.out();
    }

private:
    enum class Op { Set, Unset, Inc };
    struct OpEntry { Op op; std::string field; Value value; std::int64_t delta; };
    std::vector<OpEntry> ops_;
};

// ---- messages (header + per-message bodies) ------------------------------

namespace detail {
namespace msg {
constexpr std::uint8_t Hello = 0x01, HelloAck = 0x02, Auth = 0x03, AuthAck = 0x04;
constexpr std::uint8_t Begin = 0x10, Commit = 0x11, Abort = 0x12, TxnAck = 0x13;
constexpr std::uint8_t SqlExecute = 0x20, SqlResult = 0x21;
constexpr std::uint8_t DocOp = 0x30, DocResult = 0x31;
constexpr std::uint8_t KvOp = 0x40, KvResult = 0x41;
constexpr std::uint8_t Notice = 0x60, Ping = 0x70, Pong = 0x71;
constexpr std::uint8_t AuthPassword = 1;
constexpr std::uint8_t TxnReadWrite = 0, TxnReadOnly = 1;
constexpr std::uint32_t FeatureConnectDb = 1;
}  // namespace msg

struct ColumnDesc { std::string name; int typeTag; bool nullable; };

struct Header { std::uint8_t type; std::uint32_t requestId; };

inline std::vector<std::uint8_t> encodePacket(std::uint32_t reqId, std::uint8_t type,
                                              const std::vector<std::uint8_t>& body) {
    Writer w;
    w.u8(type);
    w.u8(0); w.u8(0); w.u8(0);  // reserved
    w.u32(reqId);
    w.u32(0);                    // reserved
    w.raw(body.data(), body.size());
    return w.out();
}

inline Header readHeader(Reader& r) {
    Header h;
    h.type = r.u8();
    r.skip(3);
    h.requestId = r.u32();
    r.skip(4);
    return h;
}

inline std::optional<ErrorInfo> readTrailer(Reader& r, std::uint8_t status) {
    if (status == 0) return std::nullopt;
    ErrorInfo e;
    e.code = r.u32();
    e.message = r.strU16();
    const auto* sp = r.raw(5);
    e.sqlstate.assign(reinterpret_cast<const char*>(sp), 5);
    e.detail = r.strU16();
    e.position = r.u32();
    return e;
}

}  // namespace detail

// ---- high-level client ---------------------------------------------------

struct ColumnInfo { std::string name; Tag type; bool nullable; };

// A SQL result set: rows are cells in column order; lookup by name via column().
class SqlResult {
public:
    std::vector<ColumnInfo> columns;
    std::vector<std::vector<Value>> rows;
    std::uint64_t affectedRows = 0;

    // Index of a column by name, or -1.
    int column(const std::string& name) const {
        for (std::size_t i = 0; i < columns.size(); i++)
            if (columns[i].name == name) return static_cast<int>(i);
        return -1;
    }
};

struct Options {
    std::string host = "127.0.0.1";
    int port = 4444;
    std::optional<std::string> username;  // unset = skip authentication
    std::string password;
    std::string database;                 // empty = no connect-time database
    int connectTimeoutMs = 10000;
    std::string clientName = "prismdb-cpp";
    std::string clientVersion = "0.1.0";
    // TLS is not yet supported by the C++ core; see README.
};

class Connection;  // defined in prism.cpp

class Client {
public:
    static Client connect(const Options& opts);

    Client(Client&&) noexcept;
    Client& operator=(Client&&) noexcept;
    Client(const Client&) = delete;
    Client& operator=(const Client&) = delete;
    ~Client();

    // SQL. Parameters are positional ($1, $2, ...).
    SqlResult sql(const std::string& text, const std::vector<Value>& params = {});

    // Transactions.
    std::uint64_t begin(bool readOnly = false);
    void commit(std::uint64_t idempotencyKey = 0);
    void abort();

    // Key/value.
    std::optional<std::vector<std::uint8_t>> kvGet(const std::string& ns, std::string_view key);
    void kvPut(const std::string& ns, std::string_view key, std::string_view value);
    void kvPut(const std::string& ns, std::string_view key, const std::vector<std::uint8_t>& value);
    void kvDelete(const std::string& ns, std::string_view key);

    // Documents.
    ObjectId insertOne(const std::string& coll, const Document& doc);
    std::vector<ObjectId> insertMany(const std::string& coll, const std::vector<Document>& docs);
    std::vector<Document> find(const std::string& coll, const Query& q = Query::all());
    std::optional<Document> findOne(const std::string& coll, const Query& q = Query::all());
    std::uint64_t count(const std::string& coll, const Query& q = Query::all());
    std::uint64_t updateOne(const std::string& coll, const Query& q, const Update& u);
    std::uint64_t updateMany(const std::string& coll, const Query& q, const Update& u);
    std::uint64_t deleteOne(const std::string& coll, const Query& q);
    std::uint64_t deleteMany(const std::string& coll, const Query& q);

    void ping();
    void close();

private:
    explicit Client(std::unique_ptr<Connection> conn);
    std::unique_ptr<Connection> conn_;
};

}  // namespace prism

#endif  // PRISM_PRISM_HPP
