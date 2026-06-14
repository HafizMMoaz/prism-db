package dev.prism.client;

import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.util.function.Consumer;
import javax.net.ssl.SSLSocket;
import javax.net.ssl.SSLSocketFactory;

/**
 * The transport: a TCP (optionally TLS) socket that frames outgoing messages,
 * reads back full frames, and matches each reply to its request by the echoed
 * request_id. Server-initiated notices (request_id 0) go to a handler.
 *
 * Synchronous: {@link #request} writes a frame and blocks until the matching
 * reply arrives, dispatching any notices seen in between.
 */
final class Connection {
    private static final long MAX_FRAME = 64L * 1024 * 1024;

    private final Socket socket;
    private final InputStream in;
    private final OutputStream out;
    private final Consumer<NoticeMsg> onNotice;
    private long nextId = 1;

    private Connection(Socket socket, Consumer<NoticeMsg> onNotice) throws IOException {
        this.socket = socket;
        this.in = socket.getInputStream();
        this.out = socket.getOutputStream();
        this.onNotice = onNotice;
    }

    static Connection open(String host, int port, boolean tls, int connectTimeoutMs, Consumer<NoticeMsg> onNotice) {
        try {
            Socket raw = new Socket();
            raw.connect(new InetSocketAddress(host, port), connectTimeoutMs);
            raw.setTcpNoDelay(true);
            Socket sock = raw;
            if (tls) {
                SSLSocketFactory factory = (SSLSocketFactory) SSLSocketFactory.getDefault();
                SSLSocket ssl = (SSLSocket) factory.createSocket(raw, host, port, true);
                ssl.startHandshake();
                sock = ssl;
            }
            return new Connection(sock, onNotice);
        } catch (IOException e) {
            throw new PrismException("connect to " + host + ":" + port + " failed: " + e.getMessage());
        }
    }

    Object request(int type, byte[] body) {
        long requestId = nextId;
        nextId = (nextId >= 0xFFFFFFFFL) ? 1 : nextId + 1;
        try {
            byte[] frame = Frame.encode(Protocol.encodePacket(requestId, type, body));
            out.write(frame);
            out.flush();
        } catch (IOException e) {
            throw new PrismException("send failed: " + e.getMessage());
        }

        while (true) {
            byte[] header = readExact(4);
            long len = (header[0] & 0xFFL) | ((header[1] & 0xFFL) << 8)
                    | ((header[2] & 0xFFL) << 16) | ((header[3] & 0xFFL) << 24);
            if (len < 12 || len > MAX_FRAME) throw new ProtocolException("invalid frame length " + len);
            byte[] payload = readExact((int) len);
            ServerPacket packet = Protocol.decodePacket(payload);
            if (packet.message instanceof NoticeMsg) {
                if (onNotice != null) onNotice.accept((NoticeMsg) packet.message);
                continue;
            }
            if (packet.requestId == requestId) return packet.message;
            // An unmatched reply (e.g. a late response) is ignored.
        }
    }

    void close() {
        try {
            socket.close();
        } catch (IOException ignored) {
            // closing best-effort
        }
    }

    private byte[] readExact(int n) {
        byte[] buf = new byte[n];
        int off = 0;
        try {
            while (off < n) {
                int k = in.read(buf, off, n - off);
                if (k < 0) throw new PrismException("connection closed by server");
                off += k;
            }
        } catch (IOException e) {
            throw new PrismException("read failed: " + e.getMessage());
        }
        return buf;
    }
}
