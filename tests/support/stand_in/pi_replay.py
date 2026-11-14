"""Pure subset of pi-atm's best-effort transport, not SDK conformance.

Verified against the repository extension's send/enqueue/close implementation:
no sends during close backoff; bounded drop-oldest FIFO outside backoff. Runtime
opens a socket only on a later accepted send, never on a timer.
"""
BACKOFF_MS = 1000
OUTBOX_MAX = 256


def enqueue(outbox, frame, now_ms, retry_at):
    if now_ms < retry_at:
        return outbox, False
    return (outbox + [frame])[-OUTBOX_MAX:], True
