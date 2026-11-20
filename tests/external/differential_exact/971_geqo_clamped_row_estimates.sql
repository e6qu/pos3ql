-- GEQO over 110 relations whose cross product passes the largest
-- double: each join's rows clamp at MAXIMUM_ROWCOUNT (1e100), so every
-- tour has a finite cost and the search finds PostgreSQL's plan.
CREATE TABLE ec (id int);
INSERT INTO ec SELECT g FROM generate_series(1, 1000) g;
ANALYZE ec;
EXPLAIN (COSTS OFF) SELECT ec1.id + ec110.id FROM ec ec1, ec ec2, ec ec3, ec ec4, ec ec5, ec ec6, ec ec7, ec ec8, ec ec9, ec ec10, ec ec11, ec ec12, ec ec13, ec ec14, ec ec15, ec ec16, ec ec17, ec ec18, ec ec19, ec ec20, ec ec21, ec ec22, ec ec23, ec ec24, ec ec25, ec ec26, ec ec27, ec ec28, ec ec29, ec ec30, ec ec31, ec ec32, ec ec33, ec ec34, ec ec35, ec ec36, ec ec37, ec ec38, ec ec39, ec ec40, ec ec41, ec ec42, ec ec43, ec ec44, ec ec45, ec ec46, ec ec47, ec ec48, ec ec49, ec ec50, ec ec51, ec ec52, ec ec53, ec ec54, ec ec55, ec ec56, ec ec57, ec ec58, ec ec59, ec ec60, ec ec61, ec ec62, ec ec63, ec ec64, ec ec65, ec ec66, ec ec67, ec ec68, ec ec69, ec ec70, ec ec71, ec ec72, ec ec73, ec ec74, ec ec75, ec ec76, ec ec77, ec ec78, ec ec79, ec ec80, ec ec81, ec ec82, ec ec83, ec ec84, ec ec85, ec ec86, ec ec87, ec ec88, ec ec89, ec ec90, ec ec91, ec ec92, ec ec93, ec ec94, ec ec95, ec ec96, ec ec97, ec ec98, ec ec99, ec ec100, ec ec101, ec ec102, ec ec103, ec ec104, ec ec105, ec ec106, ec ec107, ec ec108, ec ec109, ec ec110;
DROP TABLE ec;
