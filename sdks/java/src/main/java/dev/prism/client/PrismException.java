package dev.prism.client;

/** Base class for every (unchecked) error raised by this SDK. */
public class PrismException extends RuntimeException {
    public PrismException(String message) {
        super(message);
    }
}
