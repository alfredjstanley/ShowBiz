-- Bookings, their held/confirmed seats, and a record of every payment webhook event
-- we have ever acted on.
--
-- Design principle used throughout: never "check, then act" in two separate steps —
-- that leaves a gap two concurrent requests can both slip through. Instead, let a
-- UNIQUE constraint (or a partial UNIQUE index) do the arbitration as part of a single
-- INSERT/UPDATE. SQLite serialises writers database-wide, so whichever write reaches
-- the constraint first wins, atomically, with no window for a second writer to sneak in.

-- ---------------------------------------------------------------------------------------
-- One row per booking attempt. `id` is the value you hand the gateway as
-- `booking_reference` — you generate it (a uuid) before you have a transaction_id, since
-- the gateway needs *something* to key its idempotency_key on for the very first call.
-- ---------------------------------------------------------------------------------------
CREATE TABLE bookings (
    id             TEXT    PRIMARY KEY,
    show_id        INTEGER NOT NULL REFERENCES shows (id),
    amount_minor   INTEGER NOT NULL CHECK (amount_minor > 0),
    currency       TEXT    NOT NULL DEFAULT 'INR',
    -- PENDING: payment started, no answer yet. CONFIRMED / FAILED: the webhook (or a
    -- poll of the gateway, in the advanced section) has told us the outcome.
    status         TEXT    NOT NULL CHECK (status IN ('PENDING', 'CONFIRMED', 'FAILED')),
    -- Null until the gateway's POST /v1/payments response comes back. The webhook
    -- correlates back to a booking via this column, so it must be persisted before
    -- the create-booking call returns — the webhook can otherwise arrive before you
    -- would have set it (README rule 5).
    transaction_id TEXT,
    created_at     TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    updated_at     TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
) STRICT;

-- Webhooks look bookings up by transaction_id, so this needs an index — without one
-- every webhook delivery does a full table scan.
CREATE INDEX idx_bookings_transaction_id ON bookings (transaction_id);

-- ---------------------------------------------------------------------------------------
-- One row per seat within a booking. Rows are never deleted, only moved to RELEASED —
-- that keeps a full audit trail of every hold that was ever attempted, which matters
-- when you're debugging a race after the fact.
-- ---------------------------------------------------------------------------------------
CREATE TABLE booking_seats (
    id         INTEGER PRIMARY KEY,
    booking_id TEXT    NOT NULL REFERENCES bookings (id),
    show_id    INTEGER NOT NULL REFERENCES shows (id),
    seat_id    TEXT    NOT NULL REFERENCES seats (id),
    status     TEXT    NOT NULL CHECK (status IN ('HELD', 'CONFIRMED', 'RELEASED')),
    created_at TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
) STRICT;

-- THE core safety mechanism for "two people, one seat" (README rule 4). A normal
-- UNIQUE(show_id, seat_id) would be too strict — it would also block a seat that was
-- HELD and then RELEASED from ever being taken again. Scoping the index to only
-- HELD/CONFIRMED rows means a RELEASED row becomes invisible to the constraint, so the
-- seat is free again, while an active HELD row still blocks every other attempt.
CREATE UNIQUE INDEX idx_active_seat_hold
ON booking_seats (show_id, seat_id)
WHERE status IN ('HELD', 'CONFIRMED');

CREATE INDEX idx_booking_seats_booking_id ON booking_seats (booking_id);

-- ---------------------------------------------------------------------------------------
-- One row per payment-webhook event we have ever accepted. Its only job is to be a
-- bouncer at the door: attempt to INSERT the event_id before acting on a webhook, and
-- let the PRIMARY KEY (itself a UNIQUE constraint) reject the second of two concurrent
-- duplicate deliveries (README rule 3). No payload is stored here on purpose — this
-- table exists purely for the atomic claim, not as a log.
-- ---------------------------------------------------------------------------------------
CREATE TABLE payment_events (
    event_id    TEXT PRIMARY KEY,
    received_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
) STRICT;
