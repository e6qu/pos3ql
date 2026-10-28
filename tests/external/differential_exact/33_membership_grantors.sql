SET client_min_messages = notice;

CREATE ROLE grant_chain_group;

CREATE ROLE grant_chain_first;

CREATE ROLE grant_chain_second;

GRANT grant_chain_group TO grant_chain_first WITH ADMIN OPTION;

SET ROLE grant_chain_first;

GRANT grant_chain_group TO grant_chain_second WITH ADMIN OPTION;

RESET ROLE;

SET ROLE grant_chain_second;

GRANT grant_chain_group TO grant_chain_first WITH ADMIN OPTION;

GRANT grant_chain_group TO postgres WITH ADMIN OPTION;

RESET ROLE;

SELECT count(*) = 0 FROM pg_auth_members
  WHERE roleid = 'grant_chain_group'::regrole
    AND grantor = 'grant_chain_second'::regrole;

GRANT grant_chain_group TO grant_chain_first WITH ADMIN FALSE;

SET ROLE grant_chain_second;

GRANT grant_chain_group TO grant_chain_first WITH ADMIN OPTION;

RESET ROLE;

SELECT count(*) = 1 FROM pg_auth_members
  WHERE roleid = 'grant_chain_group'::regrole
    AND member = 'grant_chain_first'::regrole
    AND grantor = 'grant_chain_second'::regrole;

GRANT grant_chain_group TO grant_chain_first WITH ADMIN TRUE;

REVOKE grant_chain_group FROM grant_chain_first GRANTED BY grant_chain_second;

GRANT grant_chain_group TO grant_chain_second WITH ADMIN OPTION;

SET ROLE grant_chain_second;

GRANT grant_chain_group TO grant_chain_first WITH ADMIN OPTION;

GRANT grant_chain_group TO grant_chain_first WITH ADMIN OPTION;

RESET ROLE;

REVOKE grant_chain_group FROM grant_chain_first GRANTED BY grant_chain_second;

REVOKE grant_chain_group FROM grant_chain_first GRANTED BY grant_chain_second;

SELECT count(*) = 1 FROM pg_auth_members
  WHERE roleid = 'grant_chain_group'::regrole
    AND member = 'grant_chain_first'::regrole;

REVOKE grant_chain_group FROM grant_chain_second GRANTED BY grant_chain_first;

REVOKE grant_chain_group FROM grant_chain_second;

REVOKE grant_chain_group FROM grant_chain_first;

DROP ROLE grant_chain_first, grant_chain_second, grant_chain_group;
