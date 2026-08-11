-- A bare minimum bookings table to prove a booking can be created and read back.

CREATE TABLE bookings (
    id         TEXT    PRIMARY KEY,
    show_id    INTEGER NOT NULL REFERENCES shows (id),
    status     TEXT    NOT NULL CHECK (status IN ('PENDING', 'CONFIRMED', 'FAILED')),
    created_at TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
) STRICT;
 
-- One row per seat within a booking. Rows are never deleted,
-- only moved to RELEASED

CREATE TABLE booking_seats (
    id         INTEGER PRIMARY KEY,
    booking_id TEXT    NOT NULL REFERENCES bookings (id),
    show_id    INTEGER NOT NULL REFERENCES shows (id),
    seat_id    TEXT    NOT NULL REFERENCES seats (id),
    status     TEXT    NOT NULL CHECK (status IN ('HELD', 'CONFIRMED', 'RELEASED'))
) STRICT;
CREATE UNIQUE INDEX idx_active_seat_hold
ON booking_seats (show_id, seat_id)
WHERE status IN ('HELD', 'CONFIRMED');
