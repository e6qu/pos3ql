-- The manifest v14 constraint shape accepts exactly 64 checks per modeled
-- kind. PostgreSQL accepts this shape; pos3ql separately rejects the next item
-- before catalog mutation with SQLSTATE 54000.
CREATE DOMAIN differential_constraint_domain AS integer CONSTRAINT differential_domain_check_00 CHECK (VALUE >= -0) CONSTRAINT differential_domain_check_01 CHECK (VALUE >= -1) CONSTRAINT differential_domain_check_02 CHECK (VALUE >= -2) CONSTRAINT differential_domain_check_03 CHECK (VALUE >= -3) CONSTRAINT differential_domain_check_04 CHECK (VALUE >= -4) CONSTRAINT differential_domain_check_05 CHECK (VALUE >= -5) CONSTRAINT differential_domain_check_06 CHECK (VALUE >= -6) CONSTRAINT differential_domain_check_07 CHECK (VALUE >= -7) CONSTRAINT differential_domain_check_08 CHECK (VALUE >= -8) CONSTRAINT differential_domain_check_09 CHECK (VALUE >= -9) CONSTRAINT differential_domain_check_10 CHECK (VALUE >= -10) CONSTRAINT differential_domain_check_11 CHECK (VALUE >= -11) CONSTRAINT differential_domain_check_12 CHECK (VALUE >= -12) CONSTRAINT differential_domain_check_13 CHECK (VALUE >= -13) CONSTRAINT differential_domain_check_14 CHECK (VALUE >= -14) CONSTRAINT differential_domain_check_15 CHECK (VALUE >= -15) CONSTRAINT differential_domain_check_16 CHECK (VALUE >= -16) CONSTRAINT differential_domain_check_17 CHECK (VALUE >= -17) CONSTRAINT differential_domain_check_18 CHECK (VALUE >= -18) CONSTRAINT differential_domain_check_19 CHECK (VALUE >= -19) CONSTRAINT differential_domain_check_20 CHECK (VALUE >= -20) CONSTRAINT differential_domain_check_21 CHECK (VALUE >= -21) CONSTRAINT differential_domain_check_22 CHECK (VALUE >= -22) CONSTRAINT differential_domain_check_23 CHECK (VALUE >= -23) CONSTRAINT differential_domain_check_24 CHECK (VALUE >= -24) CONSTRAINT differential_domain_check_25 CHECK (VALUE >= -25) CONSTRAINT differential_domain_check_26 CHECK (VALUE >= -26) CONSTRAINT differential_domain_check_27 CHECK (VALUE >= -27) CONSTRAINT differential_domain_check_28 CHECK (VALUE >= -28) CONSTRAINT differential_domain_check_29 CHECK (VALUE >= -29) CONSTRAINT differential_domain_check_30 CHECK (VALUE >= -30) CONSTRAINT differential_domain_check_31 CHECK (VALUE >= -31) CONSTRAINT differential_domain_check_32 CHECK (VALUE >= -32) CONSTRAINT differential_domain_check_33 CHECK (VALUE >= -33) CONSTRAINT differential_domain_check_34 CHECK (VALUE >= -34) CONSTRAINT differential_domain_check_35 CHECK (VALUE >= -35) CONSTRAINT differential_domain_check_36 CHECK (VALUE >= -36) CONSTRAINT differential_domain_check_37 CHECK (VALUE >= -37) CONSTRAINT differential_domain_check_38 CHECK (VALUE >= -38) CONSTRAINT differential_domain_check_39 CHECK (VALUE >= -39) CONSTRAINT differential_domain_check_40 CHECK (VALUE >= -40) CONSTRAINT differential_domain_check_41 CHECK (VALUE >= -41) CONSTRAINT differential_domain_check_42 CHECK (VALUE >= -42) CONSTRAINT differential_domain_check_43 CHECK (VALUE >= -43) CONSTRAINT differential_domain_check_44 CHECK (VALUE >= -44) CONSTRAINT differential_domain_check_45 CHECK (VALUE >= -45) CONSTRAINT differential_domain_check_46 CHECK (VALUE >= -46) CONSTRAINT differential_domain_check_47 CHECK (VALUE >= -47) CONSTRAINT differential_domain_check_48 CHECK (VALUE >= -48) CONSTRAINT differential_domain_check_49 CHECK (VALUE >= -49) CONSTRAINT differential_domain_check_50 CHECK (VALUE >= -50) CONSTRAINT differential_domain_check_51 CHECK (VALUE >= -51) CONSTRAINT differential_domain_check_52 CHECK (VALUE >= -52) CONSTRAINT differential_domain_check_53 CHECK (VALUE >= -53) CONSTRAINT differential_domain_check_54 CHECK (VALUE >= -54) CONSTRAINT differential_domain_check_55 CHECK (VALUE >= -55) CONSTRAINT differential_domain_check_56 CHECK (VALUE >= -56) CONSTRAINT differential_domain_check_57 CHECK (VALUE >= -57) CONSTRAINT differential_domain_check_58 CHECK (VALUE >= -58) CONSTRAINT differential_domain_check_59 CHECK (VALUE >= -59) CONSTRAINT differential_domain_check_60 CHECK (VALUE >= -60) CONSTRAINT differential_domain_check_61 CHECK (VALUE >= -61) CONSTRAINT differential_domain_check_62 CHECK (VALUE >= -62) CONSTRAINT differential_domain_check_63 CHECK (VALUE >= -63);

CREATE TABLE differential_constraint_width (
  value integer,
  CONSTRAINT differential_table_check_00 CHECK (value >= -0),
  CONSTRAINT differential_table_check_01 CHECK (value >= -1),
  CONSTRAINT differential_table_check_02 CHECK (value >= -2),
  CONSTRAINT differential_table_check_03 CHECK (value >= -3),
  CONSTRAINT differential_table_check_04 CHECK (value >= -4),
  CONSTRAINT differential_table_check_05 CHECK (value >= -5),
  CONSTRAINT differential_table_check_06 CHECK (value >= -6),
  CONSTRAINT differential_table_check_07 CHECK (value >= -7),
  CONSTRAINT differential_table_check_08 CHECK (value >= -8),
  CONSTRAINT differential_table_check_09 CHECK (value >= -9),
  CONSTRAINT differential_table_check_10 CHECK (value >= -10),
  CONSTRAINT differential_table_check_11 CHECK (value >= -11),
  CONSTRAINT differential_table_check_12 CHECK (value >= -12),
  CONSTRAINT differential_table_check_13 CHECK (value >= -13),
  CONSTRAINT differential_table_check_14 CHECK (value >= -14),
  CONSTRAINT differential_table_check_15 CHECK (value >= -15),
  CONSTRAINT differential_table_check_16 CHECK (value >= -16),
  CONSTRAINT differential_table_check_17 CHECK (value >= -17),
  CONSTRAINT differential_table_check_18 CHECK (value >= -18),
  CONSTRAINT differential_table_check_19 CHECK (value >= -19),
  CONSTRAINT differential_table_check_20 CHECK (value >= -20),
  CONSTRAINT differential_table_check_21 CHECK (value >= -21),
  CONSTRAINT differential_table_check_22 CHECK (value >= -22),
  CONSTRAINT differential_table_check_23 CHECK (value >= -23),
  CONSTRAINT differential_table_check_24 CHECK (value >= -24),
  CONSTRAINT differential_table_check_25 CHECK (value >= -25),
  CONSTRAINT differential_table_check_26 CHECK (value >= -26),
  CONSTRAINT differential_table_check_27 CHECK (value >= -27),
  CONSTRAINT differential_table_check_28 CHECK (value >= -28),
  CONSTRAINT differential_table_check_29 CHECK (value >= -29),
  CONSTRAINT differential_table_check_30 CHECK (value >= -30),
  CONSTRAINT differential_table_check_31 CHECK (value >= -31),
  CONSTRAINT differential_table_check_32 CHECK (value >= -32),
  CONSTRAINT differential_table_check_33 CHECK (value >= -33),
  CONSTRAINT differential_table_check_34 CHECK (value >= -34),
  CONSTRAINT differential_table_check_35 CHECK (value >= -35),
  CONSTRAINT differential_table_check_36 CHECK (value >= -36),
  CONSTRAINT differential_table_check_37 CHECK (value >= -37),
  CONSTRAINT differential_table_check_38 CHECK (value >= -38),
  CONSTRAINT differential_table_check_39 CHECK (value >= -39),
  CONSTRAINT differential_table_check_40 CHECK (value >= -40),
  CONSTRAINT differential_table_check_41 CHECK (value >= -41),
  CONSTRAINT differential_table_check_42 CHECK (value >= -42),
  CONSTRAINT differential_table_check_43 CHECK (value >= -43),
  CONSTRAINT differential_table_check_44 CHECK (value >= -44),
  CONSTRAINT differential_table_check_45 CHECK (value >= -45),
  CONSTRAINT differential_table_check_46 CHECK (value >= -46),
  CONSTRAINT differential_table_check_47 CHECK (value >= -47),
  CONSTRAINT differential_table_check_48 CHECK (value >= -48),
  CONSTRAINT differential_table_check_49 CHECK (value >= -49),
  CONSTRAINT differential_table_check_50 CHECK (value >= -50),
  CONSTRAINT differential_table_check_51 CHECK (value >= -51),
  CONSTRAINT differential_table_check_52 CHECK (value >= -52),
  CONSTRAINT differential_table_check_53 CHECK (value >= -53),
  CONSTRAINT differential_table_check_54 CHECK (value >= -54),
  CONSTRAINT differential_table_check_55 CHECK (value >= -55),
  CONSTRAINT differential_table_check_56 CHECK (value >= -56),
  CONSTRAINT differential_table_check_57 CHECK (value >= -57),
  CONSTRAINT differential_table_check_58 CHECK (value >= -58),
  CONSTRAINT differential_table_check_59 CHECK (value >= -59),
  CONSTRAINT differential_table_check_60 CHECK (value >= -60),
  CONSTRAINT differential_table_check_61 CHECK (value >= -61),
  CONSTRAINT differential_table_check_62 CHECK (value >= -62),
  CONSTRAINT differential_table_check_63 CHECK (value >= -63)
);

INSERT INTO differential_constraint_width VALUES (1);

SELECT count(*), min(conname::text), max(conname::text)
FROM pg_constraint
WHERE contypid = $name$differential_constraint_domain$name$::regtype
  AND contype = $name$c$name$;

SELECT count(*), min(conname::text), max(conname::text)
FROM pg_constraint
WHERE conrelid = $name$differential_constraint_width$name$::regclass
  AND contype = $name$c$name$;

SELECT value, value::differential_constraint_domain
FROM differential_constraint_width;

DROP TABLE differential_constraint_width;
DROP DOMAIN differential_constraint_domain;
