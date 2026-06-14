package dev.prism.client;

/** A malformed frame/message, or a byte-level decode failure. */
public final class ProtocolException extends PrismException {
    public ProtocolException(String message) {
        super(message);
    }
}
