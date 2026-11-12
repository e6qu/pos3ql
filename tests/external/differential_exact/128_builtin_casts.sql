-- pg_cast lists PostgreSQL's built-in casts with their catalog OIDs.
SELECT count(*), min(oid), max(oid) FROM pg_cast WHERE oid < 16384;
SELECT oid, castsource, casttarget, castfunc, castcontext, castmethod
FROM pg_cast WHERE oid < 16384 ORDER BY oid;
