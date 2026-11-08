SELECT pg_advisory_lock(11::bigint);
SELECT pg_advisory_unlock(11::bigint);
SELECT pg_advisory_unlock(11::bigint),pg_advisory_unlock_shared(-11::bigint),
    pg_advisory_unlock(-1,2),pg_advisory_unlock_shared(-1,2),
    pg_advisory_unlock(NULL::bigint) IS NULL;
SET client_min_messages=error;
SELECT pg_advisory_unlock(11::bigint);
