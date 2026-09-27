-- Enum label catalogs cross the former inline 64-label boundary.
CREATE TYPE differential_wide_enum AS ENUM ('label_000','label_001','label_002','label_003','label_004','label_005','label_006','label_007','label_008','label_009','label_010','label_011','label_012','label_013','label_014','label_015','label_016','label_017','label_018','label_019','label_020','label_021','label_022','label_023','label_024','label_025','label_026','label_027','label_028','label_029','label_030','label_031','label_032','label_033','label_034','label_035','label_036','label_037','label_038','label_039','label_040','label_041','label_042','label_043','label_044','label_045','label_046','label_047','label_048','label_049','label_050','label_051','label_052','label_053','label_054','label_055','label_056','label_057','label_058','label_059','label_060','label_061','label_062','label_063','label_064','label_065','label_066','label_067','label_068','label_069','label_070','label_071','label_072','label_073','label_074','label_075','label_076','label_077','label_078','label_079','label_080','label_081','label_082','label_083','label_084','label_085','label_086','label_087','label_088','label_089','label_090','label_091','label_092','label_093','label_094','label_095');
SELECT count(*) FROM pg_enum WHERE enumtypid = 'differential_wide_enum'::regtype;
SELECT enumlabel FROM pg_enum WHERE enumtypid = 'differential_wide_enum'::regtype ORDER BY enumsortorder LIMIT 2;
SELECT enumlabel FROM pg_enum WHERE enumtypid = 'differential_wide_enum'::regtype ORDER BY enumsortorder DESC LIMIT 2;
BEGIN;
ALTER TYPE differential_wide_enum ADD VALUE 'label_096';
SAVEPOINT wide_enum_savepoint;
ALTER TYPE differential_wide_enum ADD VALUE 'label_097';
ROLLBACK TO SAVEPOINT wide_enum_savepoint;
COMMIT;
SELECT count(*) FROM pg_enum WHERE enumtypid = 'differential_wide_enum'::regtype;
SELECT 'label_096'::differential_wide_enum;
DROP TYPE differential_wide_enum;
