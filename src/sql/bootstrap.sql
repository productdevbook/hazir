CREATE SCHEMA IF NOT EXISTS hazir;

CREATE TABLE IF NOT EXISTS hazir.snapshot (
    fingerprint text PRIMARY KEY,
    ddl         text        NOT NULL,
    shape       text        NOT NULL,
    -- How the ddl names the schema it builds into.
    --
    -- 'search_path' for sql written by hand, which names no schema at all and
    -- so can simply be pointed at one. 'placeholder' for the output of
    -- pg_dump, which qualifies every object and has had the name it was taken
    -- from swapped for a token.
    apply       text        NOT NULL DEFAULT 'search_path'
                            CHECK (apply IN ('search_path', 'placeholder')),
    made_at     timestamptz NOT NULL DEFAULT now()
);

-- Which snapshot a test that names none should be given. One row, and the
-- check is what keeps it one row.
CREATE TABLE IF NOT EXISTS hazir.current (
    sole        boolean PRIMARY KEY DEFAULT true CHECK (sole),
    fingerprint text NOT NULL REFERENCES hazir.snapshot (fingerprint) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS hazir.pool (
    id          bigserial PRIMARY KEY,
    schema_name text NOT NULL UNIQUE,
    fingerprint text NOT NULL,
    state       text NOT NULL CHECK (state IN ('ready', 'leased')),
    -- A schema handed to a test that is going to change its shape. It is
    -- dropped when it comes back rather than emptied and passed on.
    burn        boolean NOT NULL DEFAULT false,
    -- host/boot/pid of whoever holds it. The boot id is in there because a
    -- pid on its own is a lie across a restart: the number comes round again
    -- and a live process inherits a dead one's claim.
    holder      text,
    held_since  timestamptz
);

CREATE INDEX IF NOT EXISTS pool_ready ON hazir.pool (fingerprint, id) WHERE state = 'ready';
CREATE INDEX IF NOT EXISTS pool_held ON hazir.pool (held_since) WHERE state = 'leased';

-- What a schema is, cheaply enough to ask on every release.
--
-- A recycled schema is only sound if it still has the tables the snapshot
-- built. A test that alters one — and the tests of a migration all do — must
-- not hand it back to the next test looking clean. Comparing this against
-- what the snapshot recorded is how that is caught, and it is one index scan
-- of the catalogue rather than a dump.
CREATE OR REPLACE FUNCTION hazir.shape(target text) RETURNS text
LANGUAGE sql STABLE AS $$
    SELECT md5(coalesce(string_agg(line, E'\n' ORDER BY line), ''))
    FROM (
        SELECT c.relname || ':' || a.attname || ':' || a.atttypid::text
               || ':' || a.attnotnull::text AS line
        FROM pg_class c
        JOIN pg_attribute a ON a.attrelid = c.oid
        -- By the namespace's oid rather than by its name: the name would have
        -- to be joined and then filtered, which reads every attribute in the
        -- database. That is work proportional to how many schemas the pool
        -- holds, on a call made once per test — the shape of the problem this
        -- whole crate exists to get rid of.
        WHERE c.relnamespace = to_regnamespace(target)::oid
          AND c.relkind IN ('r', 'p')
          AND a.attnum > 0
          AND NOT a.attisdropped
    ) shape;
$$;

-- Emptying a schema without dropping anything.
--
-- Every table in one statement, because the alternative is a lock per
-- statement and a catalogue write per table. Dropping and recreating was the
-- old way and it is what makes a long test run get slower as it goes: the
-- rows go into pg_class and pg_attribute, autovacuum chases them, and the
-- suite ends up writing harder to the catalogue than to its own tables.
CREATE OR REPLACE FUNCTION hazir.wipe(target text) RETURNS void
LANGUAGE plpgsql AS $$
DECLARE
    tables text;
BEGIN
    SELECT string_agg(format('%I.%I', schemaname, tablename), ', ')
      INTO tables
      FROM pg_tables
     WHERE schemaname = target;

    IF tables IS NOT NULL THEN
        EXECUTE 'TRUNCATE TABLE ' || tables || ' RESTART IDENTITY CASCADE';
    END IF;
END;
$$;
