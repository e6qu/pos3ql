-- PostgreSQL's default text search parser: ts_parse token types over URLs,
-- e-mail addresses, hosts, paths, numbers, versions, XML, hyphenated words,
-- mixed scripts and long tokens, with to_tsvector and ts_debug of the same.
select tokid, token from ts_parse('default', $q$http://www.example.com/path/to/file.html?x=1&y=2#frag$q$);
select tokid, token from ts_parse('default', $q$https://user@host.example.org:8080/a/b$q$);
select tokid, token from ts_parse('default', $q$ftp://ftp.x.org$q$);
select tokid, token from ts_parse('default', $q$www.google.com/search?q=hello$q$);
select tokid, token from ts_parse('default', $q$mailto:someone@example.com$q$);
select tokid, token from ts_parse('default', $q$john.doe@example.co.uk, jane_doe@mail-server.example.com$q$);
select tokid, token from ts_parse('default', $q$a@b@c@d.e$q$);
select tokid, token from ts_parse('default', $q$foo.bar.baz qux.example123.com 1.2.3.4 127.0.0.1:5432/db$q$);
select tokid, token from ts_parse('default', $q$/usr/local/bin/psql ./relative/path ../up/one ~/home/file.txt ~user/x C:\Windows\path$q$);
select tokid, token from ts_parse('default', $q$file.tar.gz name.with.dots.txt index.html$q$);
select tokid, token from ts_parse('default', $q$1.2.3 1.2.3.4.5 10.0 0.5 .5 5. -1.5 +2.25 -17 +42 0 007 12345678901234567890$q$);
select tokid, token from ts_parse('default', $q$1e10 1.5e-3 -2.5E+7 3e 4e+ 1.e5 6.02e23$q$);
select tokid, token from ts_parse('default', $q$<html><body class="x" id='y'>Hello &amp; world &#169; &#xA9; &bogus</body></html>$q$);
select tokid, token from ts_parse('default', $q$<!-- a comment --> <!DOCTYPE html> <?xml version="1.0"?> <br/> <a href="http://x.com/?a=b">link</a>$q$);
select tokid, token from ts_parse('default', $q$<script>var x = 1 < 2;</script> after <style>p { color: red }</style> done$q$);
select tokid, token from ts_parse('default', $q$self-esteem well-known state-of-the-art e-mail x-1 1-x 2-3-4 abc-123-def foo--bar -dash trailing-$q$);
select tokid, token from ts_parse('default', $q$naïve café résumé Ünïcödé-wörd über-cool 日本語 русский-текст ελληνικά mixedРусский abc123 123abc a1b2c3$q$);
select tokid, token from ts_parse('default', $q$under_score _lead trail_ CamelCase ALLCAPS$q$);
select tokid, token from ts_parse('default', $q$plus+sign hash#tag dollar$ percent% caret^ star* (paren) [bracket] {brace} "dq" 'sq' `bt` back\slash pipe|bar semi;colon colon:word excl! q?$q$);
select tokid, token from ts_parse('default', $q$version 2.0.1-beta host-name.example.com x.y z.$q$);
select tokid, token from ts_parse('default', $q$tab	separated$q$);
select tokid, token from ts_parse('default', $q$newline$q$);
select tokid, token from ts_parse('default', $q$line2   multiple   spaces$q$);
select tokid, token from ts_parse('default', $q$devanagari हिन्दी: कि क्षि and combining é é$q$);
select to_tsvector('english', $q$http://www.example.com/path/to/file.html?x=1&y=2#frag$q$);
select to_tsvector('simple', $q$http://www.example.com/path/to/file.html?x=1&y=2#frag$q$);
select to_tsvector('english', $q$https://user@host.example.org:8080/a/b$q$);
select to_tsvector('simple', $q$https://user@host.example.org:8080/a/b$q$);
select to_tsvector('english', $q$ftp://ftp.x.org$q$);
select to_tsvector('simple', $q$ftp://ftp.x.org$q$);
select to_tsvector('english', $q$www.google.com/search?q=hello$q$);
select to_tsvector('simple', $q$www.google.com/search?q=hello$q$);
select to_tsvector('english', $q$mailto:someone@example.com$q$);
select to_tsvector('simple', $q$mailto:someone@example.com$q$);
select to_tsvector('english', $q$john.doe@example.co.uk, jane_doe@mail-server.example.com$q$);
select to_tsvector('simple', $q$john.doe@example.co.uk, jane_doe@mail-server.example.com$q$);
select to_tsvector('english', $q$a@b@c@d.e$q$);
select to_tsvector('simple', $q$a@b@c@d.e$q$);
select to_tsvector('english', $q$foo.bar.baz qux.example123.com 1.2.3.4 127.0.0.1:5432/db$q$);
select to_tsvector('simple', $q$foo.bar.baz qux.example123.com 1.2.3.4 127.0.0.1:5432/db$q$);
select to_tsvector('english', $q$/usr/local/bin/psql ./relative/path ../up/one ~/home/file.txt ~user/x C:\Windows\path$q$);
select to_tsvector('simple', $q$/usr/local/bin/psql ./relative/path ../up/one ~/home/file.txt ~user/x C:\Windows\path$q$);
select to_tsvector('english', $q$file.tar.gz name.with.dots.txt index.html$q$);
select to_tsvector('simple', $q$file.tar.gz name.with.dots.txt index.html$q$);
select to_tsvector('english', $q$1.2.3 1.2.3.4.5 10.0 0.5 .5 5. -1.5 +2.25 -17 +42 0 007 12345678901234567890$q$);
select to_tsvector('simple', $q$1.2.3 1.2.3.4.5 10.0 0.5 .5 5. -1.5 +2.25 -17 +42 0 007 12345678901234567890$q$);
select to_tsvector('english', $q$1e10 1.5e-3 -2.5E+7 3e 4e+ 1.e5 6.02e23$q$);
select to_tsvector('simple', $q$1e10 1.5e-3 -2.5E+7 3e 4e+ 1.e5 6.02e23$q$);
select to_tsvector('english', $q$<html><body class="x" id='y'>Hello &amp; world &#169; &#xA9; &bogus</body></html>$q$);
select to_tsvector('simple', $q$<html><body class="x" id='y'>Hello &amp; world &#169; &#xA9; &bogus</body></html>$q$);
select to_tsvector('english', $q$<!-- a comment --> <!DOCTYPE html> <?xml version="1.0"?> <br/> <a href="http://x.com/?a=b">link</a>$q$);
select to_tsvector('simple', $q$<!-- a comment --> <!DOCTYPE html> <?xml version="1.0"?> <br/> <a href="http://x.com/?a=b">link</a>$q$);
select to_tsvector('english', $q$<script>var x = 1 < 2;</script> after <style>p { color: red }</style> done$q$);
select to_tsvector('simple', $q$<script>var x = 1 < 2;</script> after <style>p { color: red }</style> done$q$);
select to_tsvector('english', $q$self-esteem well-known state-of-the-art e-mail x-1 1-x 2-3-4 abc-123-def foo--bar -dash trailing-$q$);
select to_tsvector('simple', $q$self-esteem well-known state-of-the-art e-mail x-1 1-x 2-3-4 abc-123-def foo--bar -dash trailing-$q$);
select to_tsvector('english', $q$naïve café résumé Ünïcödé-wörd über-cool 日本語 русский-текст ελληνικά mixedРусский abc123 123abc a1b2c3$q$);
select to_tsvector('simple', $q$naïve café résumé Ünïcödé-wörd über-cool 日本語 русский-текст ελληνικά mixedРусский abc123 123abc a1b2c3$q$);
select to_tsvector('english', $q$under_score _lead trail_ CamelCase ALLCAPS$q$);
select to_tsvector('simple', $q$under_score _lead trail_ CamelCase ALLCAPS$q$);
select to_tsvector('english', $q$plus+sign hash#tag dollar$ percent% caret^ star* (paren) [bracket] {brace} "dq" 'sq' `bt` back\slash pipe|bar semi;colon colon:word excl! q?$q$);
select to_tsvector('simple', $q$plus+sign hash#tag dollar$ percent% caret^ star* (paren) [bracket] {brace} "dq" 'sq' `bt` back\slash pipe|bar semi;colon colon:word excl! q?$q$);
select to_tsvector('english', $q$version 2.0.1-beta host-name.example.com x.y z.$q$);
select to_tsvector('simple', $q$version 2.0.1-beta host-name.example.com x.y z.$q$);
select to_tsvector('english', $q$tab	separated$q$);
select to_tsvector('simple', $q$tab	separated$q$);
select to_tsvector('english', $q$newline$q$);
select to_tsvector('simple', $q$newline$q$);
select to_tsvector('english', $q$line2   multiple   spaces$q$);
select to_tsvector('simple', $q$line2   multiple   spaces$q$);
select to_tsvector('english', $q$devanagari हिन्दी: कि क्षि and combining é é$q$);
select to_tsvector('simple', $q$devanagari हिन्दी: कि क्षि and combining é é$q$);
select alias, token, dictionaries, lexemes from ts_debug('english', $q$http://www.example.com/path/to/file.html?x=1&y=2#frag$q$);
select alias, token, dictionaries, lexemes from ts_debug('english', $q$https://user@host.example.org:8080/a/b$q$);
select alias, token, dictionaries, lexemes from ts_debug('english', $q$ftp://ftp.x.org$q$);
select alias, token, dictionaries, lexemes from ts_debug('english', $q$www.google.com/search?q=hello$q$);
select alias, token, dictionaries, lexemes from ts_debug('english', $q$mailto:someone@example.com$q$);
select alias, token, dictionaries, lexemes from ts_debug('english', $q$john.doe@example.co.uk, jane_doe@mail-server.example.com$q$);
select alias, token, dictionaries, lexemes from ts_debug('english', $q$a@b@c@d.e$q$);
select alias, token, dictionaries, lexemes from ts_debug('english', $q$foo.bar.baz qux.example123.com 1.2.3.4 127.0.0.1:5432/db$q$);
select count(*), string_agg(tokid::text, ',') from ts_parse('default', $q$http://www.example.com/path/to/file.html?x=1&y=2#frag
https://user@host.example.org:8080/a/b
ftp://ftp.x.org
www.google.com/search?q=hello
mailto:someone@example.com
john.doe@example.co.uk, jane_doe@mail-server.example.com
a@b@c@d.e
foo.bar.baz qux.example123.com 1.2.3.4 127.0.0.1:5432/db
/usr/local/bin/psql ./relative/path ../up/one ~/home/file.txt ~user/x C:\Windows\path
file.tar.gz name.with.dots.txt index.html
1.2.3 1.2.3.4.5 10.0 0.5 .5 5. -1.5 +2.25 -17 +42 0 007 12345678901234567890
1e10 1.5e-3 -2.5E+7 3e 4e+ 1.e5 6.02e23
<html><body class="x" id='y'>Hello &amp; world &#169; &#xA9; &bogus</body></html>
<!-- a comment --> <!DOCTYPE html> <?xml version="1.0"?> <br/> <a href="http://x.com/?a=b">link</a>
<script>var x = 1 < 2;</script> after <style>p { color: red }</style> done
self-esteem well-known state-of-the-art e-mail x-1 1-x 2-3-4 abc-123-def foo--bar -dash trailing-
naïve café résumé Ünïcödé-wörd über-cool 日本語 русский-текст ελληνικά mixedРусский abc123 123abc a1b2c3
under_score _lead trail_ CamelCase ALLCAPS
plus+sign hash#tag dollar$ percent% caret^ star* (paren) [bracket] {brace} "dq" 'sq' `bt` back\slash pipe|bar semi;colon colon:word excl! q?
version 2.0.1-beta host-name.example.com x.y z.
tab	separated
newline
line2   multiple   spaces
devanagari हिन्दी: कि क्षि and combining é é$q$);
select tokid, token from ts_parse('default', $q$1.2.3$q$);
select tokid, token from ts_parse('default', $q$1.2.3.4.5$q$);
select tokid, token from ts_parse('default', $q$x.y.z.$q$);
select tokid, token from ts_parse('default', $q$a.b@c.d$q$);
select tokid, token from ts_parse('default', $q$foo@bar$q$);
select tokid, token from ts_parse('default', $q$foo@bar.$q$);
select tokid, token from ts_parse('default', $q$@x.com$q$);
select tokid, token from ts_parse('default', $q$a-b-c-1$q$);
select tokid, token from ts_parse('default', $q$a-1-b$q$);
select tokid, token from ts_parse('default', $q$1-a$q$);
select tokid, token from ts_parse('default', $q$--x$q$);
select tokid, token from ts_parse('default', $q$x--$q$);
select tokid, token from ts_parse('default', $q$a-$q$);
select tokid, token from ts_parse('default', $q$-a$q$);
select tokid, token from ts_parse('default', $q$+1$q$);
select tokid, token from ts_parse('default', $q$+a$q$);
select tokid, token from ts_parse('default', $q$-1.e$q$);
select tokid, token from ts_parse('default', $q$1e+$q$);
select tokid, token from ts_parse('default', $q$<a$q$);
select tokid, token from ts_parse('default', $q$<a b='c$q$);
select tokid, token from ts_parse('default', $q$<a b="c\"d">$q$);
select tokid, token from ts_parse('default', $q$<!--x$q$);
select tokid, token from ts_parse('default', $q$<!-x-->$q$);
select tokid, token from ts_parse('default', $q$<?x$q$);
select tokid, token from ts_parse('default', $q$&#x;$q$);
select tokid, token from ts_parse('default', $q$&#12$q$);
select tokid, token from ts_parse('default', $q$&a.b-c;$q$);
select tokid, token from ts_parse('default', $q$&;$q$);
select tokid, token from ts_parse('default', $q$http://$q$);
select tokid, token from ts_parse('default', $q$http://x$q$);
select tokid, token from ts_parse('default', $q$http:/x$q$);
select tokid, token from ts_parse('default', $q$http:x$q$);
select tokid, token from ts_parse('default', $q$x:/y$q$);
select tokid, token from ts_parse('default', $q$https://a.b/c d$q$);
select tokid, token from ts_parse('default', $q$a.b.c/d/e$q$);
select tokid, token from ts_parse('default', $q$a.b:80$q$);
select tokid, token from ts_parse('default', $q$a.b:80/x$q$);
select tokid, token from ts_parse('default', $q$a.b:x$q$);
select tokid, token from ts_parse('default', $q$~$q$);
select tokid, token from ts_parse('default', $q$~/$q$);
select tokid, token from ts_parse('default', $q$~x/y$q$);
select tokid, token from ts_parse('default', $q$./a$q$);
select tokid, token from ts_parse('default', $q$../a/b$q$);
select tokid, token from ts_parse('default', $q$/$q$);
select tokid, token from ts_parse('default', $q$//$q$);
select tokid, token from ts_parse('default', $q$/a/$q$);
select tokid, token from ts_parse('default', $q$a/b/c$q$);
select tokid, token from ts_parse('default', $q$1/2$q$);
select tokid, token from ts_parse('default', $q$a_b.c$q$);
select tokid, token from ts_parse('default', $q$_a.b$q$);
select tokid, token from ts_parse('default', $q$x.1$q$);
select tokid, token from ts_parse('default', $q$1.x$q$);
select tokid, token from ts_parse('default', $q$1x.y$q$);
select tokid, token from ts_parse('default', $q$x1.y2$q$);
select tokid, token from ts_parse('default', $q$x.y1$q$);
select tokid, token from ts_parse('default', $q$xn--80ak6aa92e.com$q$);
select tokid, token from ts_parse('default', $q$été$q$);
select tokid, token from ts_parse('default', $q$कि$q$);
select tokid, token from ts_parse('default', $q$xःy$q$);
select tokid, token from ts_parse('default', $q$1́$q$);
select tokid, token from ts_parse('default', $q$é-è$q$);
select tokid, token from ts_parse('default', $q$a-é$q$);
select tokid, token from ts_parse('default', $q$1-é$q$);
select tokid, token from ts_parse('default', $q$é-1$q$);
select tokid, token from ts_parse('default', $q$a-1b$q$);
select tokid, token from ts_parse('default', $q$a-b1-c$q$);
select tokid, token from ts_parse('default', $q$1-1$q$);
select tokid, token from ts_parse('default', $q$1-1-a$q$);
select tokid, token from ts_parse('default', $q$<script>x</script>y$q$);
select tokid, token from ts_parse('default', $q$<SCRIPT>a<b>c</SCRIPT>d$q$);
select tokid, token from ts_parse('default', $q$<style>q</style>r$q$);
select tokid, length(token) from ts_parse('default', repeat('a', 2046) || ' b');
select length(to_tsvector('simple', repeat('a', 2046) || ' b')::text);
select tokid, length(token) from ts_parse('default', repeat('a', 2047) || ' b');
select length(to_tsvector('simple', repeat('a', 2047) || ' b')::text);
select tokid, length(token) from ts_parse('default', repeat('a', 2048) || ' b');
select length(to_tsvector('simple', repeat('a', 2048) || ' b')::text);
select tokid, length(token) from ts_parse('default', repeat('a', 3000) || ' b');
select length(to_tsvector('simple', repeat('a', 3000) || ' b')::text);
select tokid, length(token) from ts_parse('default', repeat('ab.', 1500) || 'com');
select tokid, length(token) from ts_parse('default', 'x' || repeat('-ab', 1000));
