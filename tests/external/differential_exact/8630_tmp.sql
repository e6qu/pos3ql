CREATE DOMAIN dt AS text CHECK (length(VALUE) < 5);
CREATE DOMAIN dq AS text;
CREATE DOMAIN dn AS int CHECK (VALUE > 0);
DROP TYPE public.dt CASCADE;
DROP TYPE public.dq CASCADE;
DROP TYPE public.dn CASCADE;
SELECT typname, typtype FROM pg_type WHERE typnamespace = 'public'::regnamespace AND typtype = 'd';
