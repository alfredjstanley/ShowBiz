-- A bare minimum bookings table to prove a booking can be created and read back.

CREATE TABLE bookings (
    id         TEXT    PRIMARY KEY,
    show_id    INTEGER NOT NULL REFERENCES shows (id),
    status     TEXT    NOT NULL CHECK (status IN ('PENDING', 'CONFIRMED', 'FAILED')),
    created_at TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
) STRICT;
