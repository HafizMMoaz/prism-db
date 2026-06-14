// prism.cpp — transport (TCP sockets) and the high-level Client. The codec and
// message (de)serialisation live in prism/prism.hpp; this file wires them to a
// synchronous request/reply socket connection.

#include "prism/prism.hpp"

#include <cstdio>

#ifdef _WIN32
#  ifndef WIN32_LEAN_AND_MEAN
#    define WIN32_LEAN_AND_MEAN
#  endif
#  include <winsock2.h>
#  include <ws2tcpip.h>
   using sock_t = SOCKET;
#  define PRISM_INVALID_SOCK INVALID_SOCKET
#  define sock_close closesocket
#else
#  include <sys/types.h>
#  include <sys/socket.h>
#  include <netinet/in.h>
#  include <netinet/tcp.h>
#  include <netdb.h>
#  include <unistd.h>
#  include <fcntl.h>
#  include <cerrno>
#  include <sys/select.h>
   using sock_t = int;
#  define PRISM_INVALID_SOCK (-1)
#  define sock_close ::close
#endif

namespace prism {

namespace {
constexpr int kProtocolVersion = 1;
constexpr std::uint32_t kMaxFrame = 64u * 1024u * 1024u;

int lastSockError() {
#ifdef _WIN32
    return WSAGetLastError();
#else
    return errno;
#endif
}
}  // namespace

// ---- Connection ----------------------------------------------------------

class Connection {
public:
    static std::unique_ptr<Connection> open(const Options& opts) {
#ifdef _WIN32
        WSADATA wsa;
        if (WSAStartup(MAKEWORD(2, 2), &wsa) != 0) throw Error("WSAStartup failed");
#endif
        auto conn = std::unique_ptr<Connection>(new Connection());
        conn->fd_ = dial(opts.host, opts.port, opts.connectTimeoutMs);
        if (conn->fd_ == PRISM_INVALID_SOCK) throw Error("connect to " + opts.host + " failed");
        return conn;
    }

    ~Connection() {
        if (fd_ != PRISM_INVALID_SOCK) sock_close(fd_);
#ifdef _WIN32
        WSACleanup();
#endif
    }

    // Send one message and return the matching reply (type, full payload).
    std::pair<std::uint8_t, std::vector<std::uint8_t>> request(std::uint8_t type,
                                                               const std::vector<std::uint8_t>& body) {
        std::uint32_t reqId = nextId_;
        nextId_ = (nextId_ >= 0xFFFFFFFFu) ? 1 : nextId_ + 1;

        auto frame = detail::frameEncode(detail::encodePacket(reqId, type, body));
        writeAll(frame.data(), frame.size());

        for (;;) {
            std::uint8_t hdr[4];
            readExact(hdr, 4);
            std::uint32_t len = std::uint32_t(hdr[0]) | (std::uint32_t(hdr[1]) << 8) |
                                (std::uint32_t(hdr[2]) << 16) | (std::uint32_t(hdr[3]) << 24);
            if (len < 12 || len > kMaxFrame) throw ProtocolError("invalid frame length");
            std::vector<std::uint8_t> payload(len);
            readExact(payload.data(), len);
            std::uint8_t mtype = payload[0];
            std::uint32_t got = std::uint32_t(payload[4]) | (std::uint32_t(payload[5]) << 8) |
                                (std::uint32_t(payload[6]) << 16) | (std::uint32_t(payload[7]) << 24);
            if (mtype == detail::msg::Notice) continue;  // dispatch point for notices (ignored)
            if (got != reqId) continue;                  // late/unmatched reply
            return {mtype, std::move(payload)};
        }
    }

private:
    Connection() = default;

    static sock_t dial(const std::string& host, int port, int timeoutMs) {
        char portStr[16];
        std::snprintf(portStr, sizeof(portStr), "%d", port);
        addrinfo hints{};
        hints.ai_family = AF_UNSPEC;
        hints.ai_socktype = SOCK_STREAM;
        addrinfo* res = nullptr;
        if (getaddrinfo(host.c_str(), portStr, &hints, &res) != 0) return PRISM_INVALID_SOCK;

        sock_t out = PRISM_INVALID_SOCK;
        for (addrinfo* ai = res; ai; ai = ai->ai_next) {
            sock_t fd = socket(ai->ai_family, ai->ai_socktype, ai->ai_protocol);
            if (fd == PRISM_INVALID_SOCK) continue;
            setNonBlocking(fd, true);
            int cr = ::connect(fd, ai->ai_addr, (int)ai->ai_addrlen);
            bool ok = (cr == 0);
            if (!ok) {
                int e = lastSockError();
#ifdef _WIN32
                bool inProgress = (e == WSAEWOULDBLOCK || e == WSAEINPROGRESS);
#else
                bool inProgress = (e == EINPROGRESS);
#endif
                if (inProgress) {
                    fd_set wf;
                    FD_ZERO(&wf);
                    FD_SET(fd, &wf);
                    timeval tv;
                    tv.tv_sec = timeoutMs / 1000;
                    tv.tv_usec = (timeoutMs % 1000) * 1000;
                    int sel = select((int)(fd + 1), nullptr, &wf, nullptr, timeoutMs > 0 ? &tv : nullptr);
                    if (sel > 0) {
                        int err = 0;
                        socklen_t elen = sizeof(err);
                        if (getsockopt(fd, SOL_SOCKET, SO_ERROR, (char*)&err, &elen) == 0 && err == 0) ok = true;
                    }
                }
            }
            if (ok) {
                setNonBlocking(fd, false);
                int one = 1;
                setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, (const char*)&one, sizeof(one));
                out = fd;
                break;
            }
            sock_close(fd);
        }
        freeaddrinfo(res);
        return out;
    }

    static void setNonBlocking(sock_t fd, bool on) {
#ifdef _WIN32
        u_long mode = on ? 1 : 0;
        ioctlsocket(fd, FIONBIO, &mode);
#else
        int flags = fcntl(fd, F_GETFL, 0);
        fcntl(fd, F_SETFL, on ? (flags | O_NONBLOCK) : (flags & ~O_NONBLOCK));
#endif
    }

    void writeAll(const std::uint8_t* p, std::size_t n) {
        std::size_t off = 0;
        while (off < n) {
            int k = (int)::send(fd_, (const char*)p + off, (int)(n - off), 0);
            if (k <= 0) throw Error("send failed");
            off += (std::size_t)k;
        }
    }
    void readExact(std::uint8_t* p, std::size_t n) {
        std::size_t off = 0;
        while (off < n) {
            int k = (int)::recv(fd_, (char*)p + off, (int)(n - off), 0);
            if (k <= 0) throw Error("connection closed by server");
            off += (std::size_t)k;
        }
    }

    sock_t fd_ = PRISM_INVALID_SOCK;
    std::uint32_t nextId_ = 1;
};

// ---- Client --------------------------------------------------------------

Client::Client(std::unique_ptr<Connection> conn) : conn_(std::move(conn)) {}
Client::Client(Client&&) noexcept = default;
Client& Client::operator=(Client&&) noexcept = default;
Client::~Client() = default;

namespace {

// Decode a reply, verify its type, and position a Reader at the body.
detail::Reader bodyReader(const std::vector<std::uint8_t>& payload, std::uint8_t type,
                          std::uint8_t expected, const char* what) {
    if (type != expected) throw ProtocolError(std::string("expected ") + what);
    detail::Reader r(payload);
    detail::readHeader(r);
    return r;
}

}  // namespace

Client Client::connect(const Options& opts) {
    auto conn = Connection::open(opts);
    Client client(std::move(conn));

    const std::string& database = opts.database;
    std::uint32_t features = database.empty() ? 0 : detail::msg::FeatureConnectDb;

    detail::Writer hb;
    hb.u32(kProtocolVersion);
    hb.strU16(opts.clientName);
    hb.strU16(opts.clientVersion);
    hb.u32(features);
    if (features & detail::msg::FeatureConnectDb) hb.strU16(database);

    auto [ht, hp] = client.conn_->request(detail::msg::Hello, hb.out());
    auto hr = bodyReader(hp, ht, detail::msg::HelloAck, "HelloAck");
    std::uint8_t hstatus = hr.u8();
    hr.strU16();                 // server version
    std::uint32_t feat = hr.u32();
    hr.skip(16);                 // session id
    if (auto e = detail::readTrailer(hr, hstatus)) throw ServerError(*e);
    bool connectDbHonored = (feat & detail::msg::FeatureConnectDb) != 0 && !database.empty();

    if (opts.username) {
        detail::Writer ab;
        ab.u8(detail::msg::AuthPassword);
        ab.strU16(*opts.username);
        ab.strU16(opts.password);
        auto [at, ap] = client.conn_->request(detail::msg::Auth, ab.out());
        auto ar = bodyReader(ap, at, detail::msg::AuthAck, "AuthAck");
        std::uint8_t astatus = ar.u8();
        ar.u64();                // user oid
        if (auto e = detail::readTrailer(ar, astatus)) throw ServerError(*e);
    }

    if (!database.empty() && !connectDbHonored) client.sql("USE " + database);
    return client;
}

SqlResult Client::sql(const std::string& text, const std::vector<Value>& params) {
    detail::Writer b;
    b.strU32(text);
    b.u16(static_cast<std::uint16_t>(params.size()));
    for (const auto& p : params) detail::encodeTagged(b, p);
    b.u32(1);  // options: return_rows

    auto [t, payload] = conn_->request(detail::msg::SqlExecute, b.out());
    auto r = bodyReader(payload, t, detail::msg::SqlResult, "SqlResult");

    std::uint8_t status = r.u8();
    std::uint64_t affected = r.u64();
    std::uint16_t ncols = r.u16();
    std::vector<detail::ColumnDesc> cols;
    cols.reserve(ncols);
    for (std::uint16_t i = 0; i < ncols; i++) {
        detail::ColumnDesc c;
        c.name = r.strU16();
        c.typeTag = r.u8();
        c.nullable = r.u8() != 0;
        cols.push_back(std::move(c));
    }
    std::uint32_t nrows = r.u32();
    SqlResult out;
    out.affectedRows = affected;
    out.columns.reserve(ncols);
    for (const auto& c : cols) out.columns.push_back({c.name, static_cast<Tag>(c.typeTag), c.nullable});

    std::size_t nb = (ncols + 7) / 8;
    out.rows.reserve(nrows);
    for (std::uint32_t row = 0; row < nrows; row++) {
        const std::uint8_t* bitmap = r.raw(nb);
        std::vector<Value> cells;
        cells.reserve(ncols);
        for (std::uint16_t c = 0; c < ncols; c++) {
            bool isNull = (bitmap[c >> 3] & (1 << (c & 7))) != 0;
            cells.push_back(isNull ? Value() : detail::decodeUntagged(r, cols[c].typeTag));
        }
        out.rows.push_back(std::move(cells));
    }
    std::uint8_t more = r.u8();
    if (auto e = detail::readTrailer(r, status)) throw ServerError(*e);
    if (more) throw ProtocolError("streamed SQL results are not yet supported");
    return out;
}

std::uint64_t Client::begin(bool readOnly) {
    detail::Writer b;
    b.u8(readOnly ? detail::msg::TxnReadOnly : detail::msg::TxnReadWrite);
    auto [t, payload] = conn_->request(detail::msg::Begin, b.out());
    auto r = bodyReader(payload, t, detail::msg::TxnAck, "TxnAck");
    std::uint8_t status = r.u8();
    std::uint64_t txnId = r.u64();
    r.u64();  // commit lsn
    if (auto e = detail::readTrailer(r, status)) throw ServerError(*e);
    return txnId;
}

void Client::commit(std::uint64_t idempotencyKey) {
    detail::Writer b;
    b.u128(idempotencyKey);
    auto [t, payload] = conn_->request(detail::msg::Commit, b.out());
    auto r = bodyReader(payload, t, detail::msg::TxnAck, "TxnAck");
    std::uint8_t status = r.u8();
    r.u64();
    r.u64();
    if (auto e = detail::readTrailer(r, status)) throw ServerError(*e);
}

void Client::abort() {
    auto [t, payload] = conn_->request(detail::msg::Abort, {});
    auto r = bodyReader(payload, t, detail::msg::TxnAck, "TxnAck");
    std::uint8_t status = r.u8();
    r.u64();
    r.u64();
    if (auto e = detail::readTrailer(r, status)) throw ServerError(*e);
}

std::optional<std::vector<std::uint8_t>> Client::kvGet(const std::string& ns, std::string_view key) {
    detail::Writer b;
    b.u8(1);
    b.strU16(ns);
    b.u16(static_cast<std::uint16_t>(key.size()));
    b.raw(key.data(), key.size());
    auto [t, payload] = conn_->request(detail::msg::KvOp, b.out());
    auto r = bodyReader(payload, t, detail::msg::KvResult, "KvResult");
    std::uint8_t status = r.u8();
    std::uint8_t op = r.u8();
    if (op != 1) throw ProtocolError("expected a KV get result");
    std::uint8_t found = r.u8();
    std::optional<std::vector<std::uint8_t>> value;
    if (found) value = r.bytesU32();
    if (auto e = detail::readTrailer(r, status)) throw ServerError(*e);
    return value;
}

void Client::kvPut(const std::string& ns, std::string_view key, const std::vector<std::uint8_t>& value) {
    detail::Writer b;
    b.u8(2);
    b.strU16(ns);
    b.u16(static_cast<std::uint16_t>(key.size()));
    b.raw(key.data(), key.size());
    b.u32(static_cast<std::uint32_t>(value.size()));
    b.raw(value.data(), value.size());
    auto [t, payload] = conn_->request(detail::msg::KvOp, b.out());
    auto r = bodyReader(payload, t, detail::msg::KvResult, "KvResult");
    std::uint8_t status = r.u8();
    r.u8();  // op
    if (auto e = detail::readTrailer(r, status)) throw ServerError(*e);
}

void Client::kvPut(const std::string& ns, std::string_view key, std::string_view value) {
    kvPut(ns, key, std::vector<std::uint8_t>(value.begin(), value.end()));
}

void Client::kvDelete(const std::string& ns, std::string_view key) {
    detail::Writer b;
    b.u8(3);
    b.strU16(ns);
    b.u16(static_cast<std::uint16_t>(key.size()));
    b.raw(key.data(), key.size());
    auto [t, payload] = conn_->request(detail::msg::KvOp, b.out());
    auto r = bodyReader(payload, t, detail::msg::KvResult, "KvResult");
    std::uint8_t status = r.u8();
    r.u8();  // op
    if (auto e = detail::readTrailer(r, status)) throw ServerError(*e);
}

// A decoded DocResult.
namespace {
struct DocReply {
    std::uint64_t affected;
    std::vector<ObjectId> ids;
    std::vector<Document> docs;
};
}  // namespace

static DocReply docExchange(Connection& conn, const detail::Writer& b) {
    auto [t, payload] = conn.request(detail::msg::DocOp, b.out());
    auto r = bodyReader(payload, t, detail::msg::DocResult, "DocResult");
    std::uint8_t status = r.u8();
    std::uint64_t affected = r.u64();
    std::uint32_t idc = r.u32();
    std::vector<ObjectId> ids;
    ids.reserve(idc);
    for (std::uint32_t i = 0; i < idc; i++) ids.emplace_back(r.raw(12));
    std::uint32_t dc = r.u32();
    std::vector<Document> docs;
    docs.reserve(dc);
    for (std::uint32_t i = 0; i < dc; i++) {
        auto blob = r.bytesU32();
        docs.push_back(Document::decode(blob.data(), blob.size()));
    }
    std::uint8_t more = r.u8();
    if (auto e = detail::readTrailer(r, status)) throw ServerError(*e);
    if (more) throw ProtocolError("streamed document results are not yet supported");
    return {affected, std::move(ids), std::move(docs)};
}

ObjectId Client::insertOne(const std::string& coll, const Document& doc) {
    detail::Writer b;
    b.u8(1);
    b.strU16(coll);
    auto blob = doc.encode();
    b.u32(static_cast<std::uint32_t>(blob.size()));
    b.raw(blob.data(), blob.size());
    auto reply = docExchange(*conn_, b);
    if (reply.ids.empty()) throw ProtocolError("insert returned no _id");
    return reply.ids.front();
}

std::vector<ObjectId> Client::insertMany(const std::string& coll, const std::vector<Document>& docs) {
    detail::Writer b;
    b.u8(2);
    b.strU16(coll);
    b.u32(static_cast<std::uint32_t>(docs.size()));
    for (const auto& d : docs) {
        auto blob = d.encode();
        b.u32(static_cast<std::uint32_t>(blob.size()));
        b.raw(blob.data(), blob.size());
    }
    return docExchange(*conn_, b).ids;
}

static void writeBlob(detail::Writer& b, const std::vector<std::uint8_t>& blob) {
    b.u32(static_cast<std::uint32_t>(blob.size()));
    b.raw(blob.data(), blob.size());
}

std::vector<Document> Client::find(const std::string& coll, const Query& q) {
    detail::Writer b;
    b.u8(3);
    b.strU16(coll);
    writeBlob(b, q.encode());
    writeBlob(b, {});  // options
    return docExchange(*conn_, b).docs;
}

std::optional<Document> Client::findOne(const std::string& coll, const Query& q) {
    detail::Writer b;
    b.u8(4);
    b.strU16(coll);
    writeBlob(b, q.encode());
    writeBlob(b, {});
    auto reply = docExchange(*conn_, b);
    if (reply.docs.empty()) return std::nullopt;
    return reply.docs.front();
}

std::uint64_t Client::count(const std::string& coll, const Query& q) {
    detail::Writer b;
    b.u8(9);
    b.strU16(coll);
    writeBlob(b, q.encode());
    writeBlob(b, {});
    return docExchange(*conn_, b).affected;
}

std::uint64_t Client::updateOne(const std::string& coll, const Query& q, const Update& u) {
    detail::Writer b;
    b.u8(5);
    b.strU16(coll);
    writeBlob(b, q.encode());
    writeBlob(b, u.encode());
    writeBlob(b, {});
    return docExchange(*conn_, b).affected;
}

std::uint64_t Client::updateMany(const std::string& coll, const Query& q, const Update& u) {
    detail::Writer b;
    b.u8(6);
    b.strU16(coll);
    writeBlob(b, q.encode());
    writeBlob(b, u.encode());
    writeBlob(b, {});
    return docExchange(*conn_, b).affected;
}

std::uint64_t Client::deleteOne(const std::string& coll, const Query& q) {
    detail::Writer b;
    b.u8(7);
    b.strU16(coll);
    writeBlob(b, q.encode());
    writeBlob(b, {});
    return docExchange(*conn_, b).affected;
}

std::uint64_t Client::deleteMany(const std::string& coll, const Query& q) {
    detail::Writer b;
    b.u8(8);
    b.strU16(coll);
    writeBlob(b, q.encode());
    writeBlob(b, {});
    return docExchange(*conn_, b).affected;
}

void Client::ping() {
    auto reply = conn_->request(detail::msg::Ping, {});
    if (reply.first != detail::msg::Pong) throw ProtocolError("expected Pong");
}

void Client::close() { conn_.reset(); }

}  // namespace prism
