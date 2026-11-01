CREATE TABLE input_pause_audit(value integer);
CREATE FUNCTION retained_input(value integer) RETURNS integer LANGUAGE sql VOLATILE AS 'INSERT INTO input_pause_audit VALUES ($1); SELECT $1';
SELECT set_config('application_name','0',false);
SELECT retained_input(set_config('application_name',(current_setting('application_name')::integer+1)::text,false)::integer);
SELECT current_setting('application_name');
SELECT value FROM input_pause_audit;
DROP FUNCTION retained_input(integer);
DROP TABLE input_pause_audit;
RESET application_name;
