-- Every Snowball dictionary and configuration PostgreSQL 18.6 ships:
-- stemmed sample words, stopwords, catalog rows and psql's describe output.
select w, ts_lexize('arabic_stem', w) from unnest(array['الكتاب', 'والمكتبات', 'يكتبون', 'مدرسة', 'المعلمين', 'بالقلم', 'فسيكتبونها', 'كتابان', 'المدارس', 'استعمالات']) with ordinality as t(w, n) order by n;
select w, ts_lexize('armenian_stem', w) from unnest(array['դրոշակակիր', 'գալակտոզի', 'սպիտակեցում', 'ձայնասկավառակներով', 'կենսաբանությունը', 'թմբկահարի', 'երկուսս', 'մեռուկը', 'ռեզինը', 'միամտորեն']) with ordinality as t(w, n) order by n;
select w, ts_lexize('basque_stem', w) from unnest(array['endemikoa', 'bonbardatu', 'lirika', 'hutsaren', 'eztainua', 'erlojuak', 'muntatu', 'zalduna', 'nagusienak', 'entrenatzaile']) with ordinality as t(w, n) order by n;
select w, ts_lexize('catalan_stem', w) from unnest(array['criticat', 'cedeix', 'sorollós', 'marxes', 'gaita', 'eclipsant', 'demandant', 'nomenclàtor', 'rostres', 'obertures']) with ordinality as t(w, n) order by n;
select w, ts_lexize('danish_stem', w) from unnest(array['frelse', 'egetræet', 'udryd', 'oplevet', 'kongehuset', 'havne', 'gabbatha', 'reddet', 'tilsynsmand', 'ressædet']) with ordinality as t(w, n) order by n;
select w, ts_lexize('dutch_stem', w) from unnest(array['flexo', 'daartegen', 'wegingsfactoren', 'pinkegat', 'loopjongen', 'handling', 'gegeten', 'rivierslib', 'vingen', 'rozekleurtjes']) with ordinality as t(w, n) order by n;
select w, ts_lexize('english_stem', w) from unnest(array['duped', 'contenteth', 'prosperously', 'likest', 'fumed', 'epochs', 'rowed', 'wagoner', 'sall', 'eases']) with ordinality as t(w, n) order by n;
select w, ts_lexize('estonian_stem', w) from unnest(array['jalamil', 'fragmendi', 'summasid', 'naissugu', 'lahutamine', 'keskaegset', 'jäähokimeeskonnas', 'ostmiseks', 'seletav', 'pagenduses']) with ordinality as t(w, n) order by n;
select w, ts_lexize('finnish_stem', w) from unnest(array['elämästä', 'bronson', 'laatia', 'hullua', 'galke', 'erikoishammaslääkäri', 'myllymäki', 'virkamiehet', 'news', 'emulsio']) with ordinality as t(w, n) order by n;
select w, ts_lexize('french_stem', w) from unnest(array['dominaient', 'comptons', 'plaisantât', 'exposé', 'dénaturée', 'raisonnables', 'vivandière', 'rassurée', 'dormeurs', 'puisait']) with ordinality as t(w, n) order by n;
select w, ts_lexize('german_stem', w) from unnest(array['gerte', 'entlocken', 'verachteten', 'platzen', 'hülflos', 'gomorrah', 'weichet', 'werterziehung', 'geschiedener', 'vorteil']) with ordinality as t(w, n) order by n;
select w, ts_lexize('greek_stem', w) from unnest(array['διαφορίσιμες', 'βίους', 'φρούτου', 'οργάνωσή', 'καταψύκτες', 'εξαρτηθεί', 'δυσμενών', 'πετρώδεις', 'τυπωμένη', 'πλανήτη']) with ordinality as t(w, n) order by n;
select w, ts_lexize('hindi_stem', w) from unnest(array['किराया', 'एन्ड', 'माण्डवी', 'निर्जला', 'टमाटर', 'ग़रीब', 'कैमरा', 'हारमोनियम', 'पापनाशक', 'भारोपीय']) with ordinality as t(w, n) order by n;
select w, ts_lexize('hungarian_stem', w) from unnest(array['bolyongott', 'beleesnék', 'lépték', 'kulcsszavak', 'kiegyeznek', 'jólétét', 'bután', 'késztetés', 'leültettek', 'későmodern-posztmodern']) with ordinality as t(w, n) order by n;
select w, ts_lexize('indonesian_stem', w) from unnest(array['diantarkan', 'budak', 'pasifnya', 'konsepnya', 'hidayah', 'ditonton', 'diindonesiakan', 'ultras', 'loyalis', 'noguerai']) with ordinality as t(w, n) order by n;
select w, ts_lexize('irish_stem', w) from unnest(array['cuiteogaí', 'chlódóireachta', 'thallann', 'mhiabhán', 'ghréithre', 'éagais', 'dhearbhcheart', 'ngeamhair', 'soladaithe', 'niamhga']) with ordinality as t(w, n) order by n;
select w, ts_lexize('italian_stem', w) from unnest(array['edilberto', 'consolle', 'rozzi', 'nedo', 'igea', 'famosa', 'sollievo', 'sott', 'elencato', 'sgomentata']) with ordinality as t(w, n) order by n;
select w, ts_lexize('lithuanian_stem', w) from unnest(array['išleidusi', 'evangelikų', 'redaktoriai', 'nuosavu', 'korupcija', 'jūrininku', 'skiedromis', 'įsteigto', 'skyriui', 'išnaikintos']) with ordinality as t(w, n) order by n;
select w, ts_lexize('nepali_stem', w) from unnest(array['अँकाइ', 'अँकाइनु', 'अँकाउनु', 'अँकाउछन्', 'अँकाउनेछ', 'अँगालिएका', 'अँगालिएकी', 'अँगालिएको', 'अँगालिछन्', 'अँगालिछस्']) with ordinality as t(w, n) order by n;
select w, ts_lexize('norwegian_stem', w) from unnest(array['gjallarhorn', 'fell', 'rullering', 'medlemsstyret', 'innbyggere', 'gulne', 'skulle', 'vòk', 'slukøret', 'gjennomgått']) with ordinality as t(w, n) order by n;
select w, ts_lexize('portuguese_stem', w) from unnest(array['científica', 'bistrô', 'numerosa', 'funda', 'elege', 'coronária', 'combinador', 'veng', 'horst', 'modelar']) with ordinality as t(w, n) order by n;
select w, ts_lexize('romanian_stem', w) from unnest(array['dezordonat', 'complicități', 'şoim', 'performanţelor', 'juraţii', 'feminizez', 'drăgălaș', 'purtase', 'voință', 'păşim']) with ordinality as t(w, n) order by n;
select w, ts_lexize('russian_stem', w) from unnest(array['запретишь', 'девушек', 'танец', 'познания', 'неряхой', 'костюмах', 'золотистые', 'предполагались', 'соседей', 'прерывающее']) with ordinality as t(w, n) order by n;
select w, ts_lexize('serbian_stem', w) from unnest(array['eksplicitniji', 'detekcijama', 'pasivni', 'kišovitim', 'hroničnih', 'finansijsku', 'emancipaciju', 'ljubavničke', 'obilni', 'ljušturom']) with ordinality as t(w, n) order by n;
select w, ts_lexize('spanish_stem', w) from unnest(array['común', 'calculadoras', 'propinó', 'inherentes', 'espiritualidad', 'defensor', 'consultoriobursátil', 'lesionar', 'plateadas', 'ligne']) with ordinality as t(w, n) order by n;
select w, ts_lexize('swedish_stem', w) from unnest(array['fn', 'dingla', 'sk', 'ljufligt', 'hårstrå', 'förströ', 'fruntimmerskjortel', 'miss', 'räkning', 'modehandlerskor']) with ordinality as t(w, n) order by n;
select w, ts_lexize('tamil_stem', w) from unnest(array['ஏவூர்தியை', 'இவ்விடயங்கள்', 'மண்கொண்டார்', 'திருமணத்துக்குப்', 'சாந்தப்படுத்தும்', 'காட்சியளிக்கின்றன', 'ஓஸ்லோவைக்', 'நடிப்பையும்', 'பெரியின்', 'நம்பினோர்']) with ordinality as t(w, n) order by n;
select w, ts_lexize('turkish_stem', w) from unnest(array['ekipmanlar', 'chiapppero', 'yönlü', 'olayında', 'karþý', 'gitmemeleri', 'reklamcılık', 'yanıtsız', 'rsc', 'ekümeniklikle']) with ordinality as t(w, n) order by n;
select w, ts_lexize('yiddish_stem', w) from unnest(array['אינטעלעקטואַל', 'אױסװעגן', 'צעל', 'לירישן', 'דורכצושיסן', 'בנעימות', 'אײַנשליסנדיקער', 'נטעלע', 'צאָלרײַכע', 'נפֿיחה']) with ordinality as t(w, n) order by n;
-- Stopwords, upper case (only ASCII letters are lowered under the C ctype)
-- and words too long to stem.
select ts_lexize('english_stem', 'The'), ts_lexize('russian_stem', 'и'), ts_lexize('german_stem', 'UND'),
       ts_lexize('french_stem', 'nous'), ts_lexize('german_stem', 'HÄUSER'), ts_lexize('arabic_stem', 'في'),
       ts_lexize('english_stem', ''), ts_lexize('simple', 'The');
select length((ts_lexize('english_stem', repeat('Running', 143)))[1]), (ts_lexize('english_stem', repeat('Running', 143)))[1] = repeat('running', 143);
select length((ts_lexize('english_stem', repeat('Running', 142) || 'ab'))[1]);
-- Configurations map token types to their dictionaries.
select to_tsvector('english', 'Fast. word: "quoted" it''s (paren) [x] {y} a,b c;d e!f g*h e.g. example.com. 1.2. a-b. end http://example.com/a?b=1');
select to_tsvector('german', 'Die Häuser standen leer, aber die Kinder spielten draußen.');
select to_tsvector('french', 'Les enfants jouaient dans les jardins publics.');
select to_tsvector('russian', 'Красивые дома стояли на берегу реки, running fast.');
select to_tsvector('hindi', 'running दौड़ते हुए लड़के');
select to_tsvector('spanish', 'Los niños corrían rápidamente por las calles.');
select to_tsvector('greek', 'Τα παιδιά έπαιζαν στους δρόμους.');
select to_tsvector('turkish', 'Çocuklar sokaklarda oynuyorlardı.');
select to_tsvector('finnish', 'Lapset leikkivät kaduilla.');
select to_tsvector('italian', 'I bambini giocavano nelle strade.');
select to_tsquery('swedish', 'barnen & lekte'), plainto_tsquery('dutch', 'de kinderen speelden'), websearch_to_tsquery('portuguese', 'as crianças brincavam');
select to_tsvector('danish', 'Børnene legede på gaderne') @@ to_tsquery('danish', 'barn & lege');
select alias, token, dictionaries, dictionary, lexemes from ts_debug('russian', 'Running красивые дома 42');
select alias, token, dictionaries, dictionary, lexemes from ts_debug('hindi', 'word शब्दों');
-- Catalog rows initdb creates.
select cfgname, cfgnamespace::regnamespace, cfgowner::regrole, cfgparser::regproc, obj_description(oid, 'pg_ts_config') from pg_ts_config order by cfgname;
select dictname, dictnamespace::regnamespace, dictowner::regrole, t.tmplname, dictinitoption, obj_description(d.oid, 'pg_ts_dict') from pg_ts_dict d join pg_ts_template t on t.oid = d.dicttemplate order by dictname;
select tmplname, tmplinit, tmpllexize, obj_description(oid, 'pg_ts_template') from pg_ts_template order by tmplname;
select c.cfgname, m.maptokentype, m.mapseqno, d.dictname from pg_ts_config_map m join pg_ts_config c on c.oid = m.mapcfg join pg_ts_dict d on d.oid = m.mapdict where c.cfgname in ('hindi', 'russian', 'german', 'simple') order by 1, 2, 3;
select count(*) from pg_ts_config_map;
\dF
\dFd
\dFt
\dF+ russian
\dF+ hindi
\dFd+ german_stem
\dFt+ snowball
-- Dictionaries created from the snowball and simple templates.
create text search dictionary porter_390 (template = snowball, language = porter);
create text search dictionary dutch_390 (Template = snowball, Language = Dutch, StopWords = dutch);
create text search dictionary simple_390 (template = simple, stopwords = english, accept = false);
select dictname, dictinitoption from pg_ts_dict where dictname like '%390' order by 1;
select ts_lexize('porter_390', 'generously'), ts_lexize('dutch_390', 'Kinderen'), ts_lexize('dutch_390', 'de'),
       ts_lexize('simple_390', 'The'), ts_lexize('simple_390', 'Cats');
alter text search dictionary dutch_390 (language = 'german', stopwords = '');
select dictinitoption from pg_ts_dict where dictname = 'dutch_390';
select ts_lexize('dutch_390', 'Häuser'), ts_lexize('dutch_390', 'und');
alter text search dictionary simple_390 (accept = on);
select dictinitoption from pg_ts_dict where dictname = 'simple_390';
select ts_lexize('simple_390', 'Cats');
create text search configuration german_390 (copy = german);
alter text search configuration german_390 alter mapping for asciiword with simple_390, porter_390;
select to_tsvector('german_390', 'The Running Häuser');
create text search dictionary bad_390 (template = snowball);
create text search dictionary bad_390 (template = snowball, language = klingon);
create text search dictionary bad_390 (template = snowball, language = english, language = german);
create text search dictionary bad_390 (template = snowball, language = english, stopwords = english, stopwords = english);
create text search dictionary bad_390 (template = snowball, language = english, color = red);
create text search dictionary bad_390 (template = snowball, language = english, stopwords = 'English');
create text search dictionary bad_390 (template = simple, accept = maybe);
create text search dictionary bad_390 (template = simple, accept = true, accept = false);
create text search dictionary bad_390 (template = simple, colour = blue);
drop text search configuration german_390;
drop text search dictionary porter_390;
drop text search dictionary dutch_390;
drop text search dictionary simple_390;
-- default_text_search_config resolves the configuration it names.
set default_text_search_config = German;
show default_text_search_config;
select get_current_ts_config(), to_tsvector('Die Häuser'), 'Die Häuser'::text @@ to_tsquery('haus');
set default_text_search_config = 'pg_catalog."russian"';
show default_text_search_config;
set default_text_search_config = 'klingon';
set default_text_search_config = 'public.german';
show default_text_search_config;
reset default_text_search_config;
show default_text_search_config;
