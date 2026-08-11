-- Reference data for the theatre: one screen, its seat map, and the show schedule.
--
-- This migration is part of the boilerplate. You should NOT need to change it.
-- Add your own bookings/payments schema in a new file, e.g. `0002_bookings.sql`.
--
-- Conventions used throughout:
--   * Money is always an INTEGER in minor units (paise). Never a float.
--   * Timestamps are ISO-8601 UTC TEXT, e.g. '2026-07-29T10:00:00Z'.
--   * Show times are computed relative to NOW at migration time, so the schedule is
--     always "the next 7 days" whenever you first run the app. `rm -rf data/` re-seeds.

CREATE TABLE movies (
    id               INTEGER PRIMARY KEY,
    title            TEXT    NOT NULL,
    duration_minutes INTEGER NOT NULL,
    rating           TEXT    NOT NULL
) STRICT;

INSERT INTO movies (id, title, duration_minutes, rating) VALUES
    (1, 'The Grand Budapest Heist', 118, 'UA'),
    (2, 'Silent Monsoon',           142, 'U'),
    (3, 'Orbital Decay',            127, 'A');

-- ---------------------------------------------------------------------------------------
-- Seat map: a single screen, rows A-J with 10 seats each = 100 seats.
--   Rows A-D  REGULAR   INR 200.00
--   Rows E-H  PREMIUM   INR 350.00
--   Rows I-J  RECLINER  INR 600.00
-- `id` is the stable seat identifier you will reference from bookings (e.g. 'E7').
-- ---------------------------------------------------------------------------------------
CREATE TABLE seats (
    id          TEXT    PRIMARY KEY,
    row_label   TEXT    NOT NULL,
    seat_number INTEGER NOT NULL,
    seat_type   TEXT    NOT NULL CHECK (seat_type IN ('REGULAR', 'PREMIUM', 'RECLINER')),
    price_minor INTEGER NOT NULL CHECK (price_minor > 0),
    UNIQUE (row_label, seat_number)
) STRICT;

-- Generated with a recursive CTE so the seat map stays readable and easy to resize.
INSERT INTO seats (id, row_label, seat_number, seat_type, price_minor)
WITH rows_cte(row_label, row_index) AS (
    SELECT 'A', 0
    UNION ALL
    SELECT char(unicode('A') + row_index + 1), row_index + 1 FROM rows_cte WHERE row_index < 9
),
nums(seat_number) AS (
    SELECT 1 UNION ALL SELECT seat_number + 1 FROM nums WHERE seat_number < 10
)
SELECT
    r.row_label || CAST(n.seat_number AS TEXT),
    r.row_label,
    n.seat_number,
    CASE WHEN r.row_index <= 3 THEN 'REGULAR'
         WHEN r.row_index <= 7 THEN 'PREMIUM'
         ELSE 'RECLINER' END,
    CASE WHEN r.row_index <= 3 THEN 20000
         WHEN r.row_index <= 7 THEN 35000
         ELSE 60000 END
FROM rows_cte r
CROSS JOIN nums n
ORDER BY r.row_index, n.seat_number;

-- ---------------------------------------------------------------------------------------
-- Shows: 4 slots per day (10:00, 13:30, 17:00, 20:30 UTC) for the next 7 days = 28 shows.
--
-- `price_multiplier_bp` is basis points, where 10000 = 1.0x. Evening slots are priced at
-- 12500 (1.25x). The amount for one seat is therefore, in integer arithmetic:
--
--     seats.price_minor * shows.price_multiplier_bp / 10000
--
-- Only one screen exists, so `starts_at` is unique.
-- ---------------------------------------------------------------------------------------
CREATE TABLE shows (
    id                  INTEGER PRIMARY KEY,
    movie_id            INTEGER NOT NULL REFERENCES movies (id),
    starts_at           TEXT    NOT NULL UNIQUE,
    ends_at             TEXT    NOT NULL,
    price_multiplier_bp INTEGER NOT NULL CHECK (price_multiplier_bp > 0),
    currency            TEXT    NOT NULL DEFAULT 'INR'
) STRICT;

CREATE INDEX idx_shows_starts_at ON shows (starts_at);

INSERT INTO shows (movie_id, starts_at, ends_at, price_multiplier_bp, currency)
WITH days(day_offset) AS (
    SELECT 0 UNION ALL SELECT day_offset + 1 FROM days WHERE day_offset < 6
),
slots(slot_index, slot_minutes, multiplier_bp) AS (
    SELECT 0, 600,  10000 UNION ALL  -- 10:00
    SELECT 1, 810,  10000 UNION ALL  -- 13:30
    SELECT 2, 1020, 12500 UNION ALL  -- 17:00
    SELECT 3, 1230, 12500            -- 20:30
),
scheduled AS (
    SELECT
        d.day_offset,
        s.slot_index,
        s.multiplier_bp,
        -- Midnight tonight, then forward by whole days and the slot offset in minutes.
        -- 'start of day' truncates, so shows always land on clean wall-clock times.
        datetime('now', 'start of day', '+' || (d.day_offset + 1) || ' days',
                 '+' || s.slot_minutes || ' minutes') AS starts_at_naive
    FROM days d
    CROSS JOIN slots s
)
SELECT
    -- Rotate the three movies across the schedule.
    ((sc.day_offset * 4 + sc.slot_index) % 3) + 1,
    strftime('%Y-%m-%dT%H:%M:%SZ', sc.starts_at_naive),
    strftime('%Y-%m-%dT%H:%M:%SZ', sc.starts_at_naive, '+' ||
        (SELECT m.duration_minutes + 20
         FROM movies m
         WHERE m.id = ((sc.day_offset * 4 + sc.slot_index) % 3) + 1) || ' minutes'),
    sc.multiplier_bp,
    'INR'
FROM scheduled sc
ORDER BY sc.day_offset, sc.slot_index;

-- One show that has already started, two hours ago.
--
-- Without it, every show in the table is in the future and any "reject shows that already
-- started" check is untestable — you could write the guard but never see it fire.
INSERT INTO shows (movie_id, starts_at, ends_at, price_multiplier_bp, currency)
SELECT
    1,
    strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-2 hours'),
    strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-2 hours',
             '+' || (SELECT duration_minutes + 20 FROM movies WHERE id = 1) || ' minutes'),
    10000,
    'INR';
