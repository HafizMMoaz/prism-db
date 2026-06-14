package dev.prism.client;

/** An error returned by the server (status != 0), carrying its trailer. */
public final class ServerException extends PrismException {
    public final int code;
    public final String sqlstate;
    public final String detail;
    public final int position;

    public ServerException(ErrorInfo info) {
        super(info.message.isEmpty() ? String.format("server error 0x%04x", info.code) : info.message);
        this.code = info.code;
        this.sqlstate = info.sqlstate;
        this.detail = info.detail;
        this.position = info.position;
    }
}
