-- PostgreSQL 18 retains this obsolete spelling only to reject it precisely.
CREATE ROLE role_credentials_error UNENCRYPTED PASSWORD 'not-supported';
CREATE ROLE role_credentials_duplicate LOGIN NOLOGIN;
CREATE ROLE role_credentials_duplicate IN ROLE parent IN GROUP parent;
