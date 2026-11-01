SELECT set_config('application_name','0',false);
SELECT concat(set_config('application_name',current_setting('application_name')||'.',false),pg_stat_clear_snapshot());
SELECT current_setting('application_name');
SELECT set_config('application_name','0',false);
SELECT lo_unlink(lo_from_bytea(0,convert_to(set_config('application_name',current_setting('application_name')||'.',false),'UTF8')));
SELECT current_setting('application_name');
RESET application_name;
