//! The Snowball stemmers and stopword lists PostgreSQL 18.6 ships.
//!
//! Each language module is translated from PostgreSQL's
//! `src/backend/snowball/libstemmer/stem_UTF_8_*.c` by
//! `tools/generate-snowball-stemmers.py`, keeping Snowball's control flow.
//! The runtime below ports Snowball's `utilities.c` and `api.c` from the same
//! tree to a fixed buffer: a word whose stemmed form would outgrow it fails
//! with SQLSTATE 54000 instead of growing the buffer. PostgreSQL never stems a
//! word longer than 1,000 bytes (`dict_snowball.c`), so the buffer leaves room
//! for lower-casing growth and every suffix replacement the stemmers make.
//!
//! Snowball (<https://snowballstem.org>, commit
//! d19326ac6c1b9a417fc872f7c2f845265a5e9ece as synced by PostgreSQL 18.6) and
//! its stopword lists are distributed under this license:
//!
//! Copyright (c) 2001, Dr Martin Porter
//! Copyright (c) 2004,2005, Richard Boulton
//! Copyright (c) 2013, Yoshiki Shibukawa
//! Copyright (c) 2006,2007,2009,2010,2011,2014-2019, Olly Betts
//! All rights reserved.
//!
//! Redistribution and use in source and binary forms, with or without
//! modification, are permitted provided that the following conditions
//! are met:
//!
//!   1. Redistributions of source code must retain the above copyright notice,
//!      this list of conditions and the following disclaimer.
//!   2. Redistributions in binary form must reproduce the above copyright
//!      notice, this list of conditions and the following disclaimer in the
//!      documentation and/or other materials provided with the distribution.
//!   3. Neither the name of the Snowball project nor the names of its
//!      contributors may be used to endorse or promote products derived from
//!      this software without specific prior written permission.
//!
//! THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
//! AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
//! IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
//! ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER OR CONTRIBUTORS BE
//! LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR
//! CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF
//! SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS
//! INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN
//! CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE)
//! ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE
//! POSSIBILITY OF SUCH DAMAGE.

mod arabic;
mod armenian;
mod basque;
mod catalan;
mod danish;
mod dutch;
mod english;
mod estonian;
mod finnish;
mod french;
mod german;
mod greek;
mod hindi;
mod hungarian;
mod indonesian;
mod irish;
mod italian;
mod lithuanian;
mod nepali;
mod norwegian;
mod porter;
mod portuguese;
mod romanian;
mod russian;
mod serbian;
mod spanish;
mod stopwords;
mod swedish;
mod tamil;
mod turkish;
mod yiddish;

/// PostgreSQL passes a word to the stemmer only when it has at most this many
/// bytes; a longer word is returned lower-cased (`dsnowball_lexize`).
pub(crate) const MAX_STEMMED_INPUT_BYTES: usize = 1_000;
/// Bytes one word may occupy while it is stemmed.
pub(crate) const STEM_BUFFER_BYTES: usize = 4_096;
const INTEGER_VARIABLES: usize = 4;

/// A stemmer that failed because its fixed buffer is full.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StemBufferFull;

pub(super) struct Among {
    pub(super) s: &'static [u8],
    pub(super) substring_i: i32,
    pub(super) result: i32,
    pub(super) function: Option<fn(&mut Env) -> i32>,
}

/// Snowball's `SN_env`: the word, the cursor and limits, and the stemmer's
/// integer and string variables, all in fixed storage.
pub(super) struct Env {
    p: [u8; STEM_BUFFER_BYTES],
    size: i32,
    pub(super) c: i32,
    pub(super) l: i32,
    pub(super) lb: i32,
    pub(super) bra: i32,
    pub(super) ket: i32,
    pub(super) i: [i32; INTEGER_VARIABLES],
    s0: [u8; STEM_BUFFER_BYTES],
    s0_size: i32,
    full: bool,
}

impl Env {
    fn new(word: &[u8]) -> Result<Self, StemBufferFull> {
        if word.len() > STEM_BUFFER_BYTES {
            return Err(StemBufferFull);
        }
        let mut env = Self {
            p: [0; STEM_BUFFER_BYTES],
            size: word.len() as i32,
            c: 0,
            l: word.len() as i32,
            lb: 0,
            bra: 0,
            ket: 0,
            i: [0; INTEGER_VARIABLES],
            s0: [0; STEM_BUFFER_BYTES],
            s0_size: 0,
            full: false,
        };
        env.p[..word.len()].copy_from_slice(word);
        Ok(env)
    }

    pub(super) fn byte(&self, at: i32) -> i32 {
        i32::from(self.p[at as usize])
    }

    pub(super) fn skip_utf8(&self, mut c: i32, limit: i32, n: i32) -> i32 {
        if n < 0 {
            return -1;
        }
        for _ in 0..n {
            if c >= limit {
                return -1;
            }
            let b = self.byte(c);
            c += 1;
            if b >= 0xC0 {
                while c < limit {
                    let b = self.byte(c);
                    if b >= 0xC0 || b < 0x80 {
                        break;
                    }
                    c += 1;
                }
            }
        }
        c
    }

    pub(super) fn skip_b_utf8(&self, mut c: i32, limit: i32, n: i32) -> i32 {
        if n < 0 {
            return -1;
        }
        for _ in 0..n {
            if c <= limit {
                return -1;
            }
            c -= 1;
            let b = self.byte(c);
            if b >= 0x80 {
                while c > limit {
                    if self.byte(c) >= 0xC0 {
                        break;
                    }
                    c -= 1;
                }
            }
        }
        c
    }

    /// Decodes the character at `c`, returning its width and value.
    fn get_utf8(&self, mut c: i32, l: i32) -> (i32, i32) {
        if c >= l {
            return (0, 0);
        }
        let b0 = self.byte(c);
        c += 1;
        if b0 < 0xC0 || c == l {
            return (1, b0);
        }
        let b1 = self.byte(c) & 0x3F;
        c += 1;
        if b0 < 0xE0 || c == l {
            return (2, (b0 & 0x1F) << 6 | b1);
        }
        let b2 = self.byte(c) & 0x3F;
        c += 1;
        if b0 < 0xF0 || c == l {
            return (3, (b0 & 0xF) << 12 | b1 << 6 | b2);
        }
        (4, (b0 & 0x7) << 18 | b1 << 12 | b2 << 6 | (self.byte(c) & 0x3F))
    }

    /// Decodes the character before `c`, returning its width and value.
    fn get_b_utf8(&self, mut c: i32, lb: i32) -> (i32, i32) {
        if c <= lb {
            return (0, 0);
        }
        c -= 1;
        let mut b = self.byte(c);
        if b < 0x80 || c == lb {
            return (1, b);
        }
        let mut a = b & 0x3F;
        c -= 1;
        b = self.byte(c);
        if b >= 0xC0 || c == lb {
            return (2, (b & 0x1F) << 6 | a);
        }
        a |= (b & 0x3F) << 6;
        c -= 1;
        b = self.byte(c);
        if b >= 0xE0 || c == lb {
            return (3, (b & 0xF) << 12 | a);
        }
        c -= 1;
        (4, (self.byte(c) & 0x7) << 18 | (b & 0x3F) << 12 | a)
    }

    fn outside(s: &[u8], ch: i32, min: i32, max: i32) -> bool {
        let ch = ch - min;
        ch + min > max || ch < 0 || (s[(ch >> 3) as usize] & (1 << (ch & 0x7))) == 0
    }

    pub(super) fn in_grouping_utf8(&mut self, s: &[u8], min: i32, max: i32, repeat: bool) -> i32 {
        loop {
            let (w, ch) = self.get_utf8(self.c, self.l);
            if w == 0 {
                return -1;
            }
            if Self::outside(s, ch, min, max) {
                return w;
            }
            self.c += w;
            if !repeat {
                return 0;
            }
        }
    }

    pub(super) fn in_grouping_b_utf8(&mut self, s: &[u8], min: i32, max: i32, repeat: bool) -> i32 {
        loop {
            let (w, ch) = self.get_b_utf8(self.c, self.lb);
            if w == 0 {
                return -1;
            }
            if Self::outside(s, ch, min, max) {
                return w;
            }
            self.c -= w;
            if !repeat {
                return 0;
            }
        }
    }

    pub(super) fn out_grouping_utf8(&mut self, s: &[u8], min: i32, max: i32, repeat: bool) -> i32 {
        loop {
            let (w, ch) = self.get_utf8(self.c, self.l);
            if w == 0 {
                return -1;
            }
            if !Self::outside(s, ch, min, max) {
                return w;
            }
            self.c += w;
            if !repeat {
                return 0;
            }
        }
    }

    pub(super) fn out_grouping_b_utf8(
        &mut self,
        s: &[u8],
        min: i32,
        max: i32,
        repeat: bool,
    ) -> i32 {
        loop {
            let (w, ch) = self.get_b_utf8(self.c, self.lb);
            if w == 0 {
                return -1;
            }
            if !Self::outside(s, ch, min, max) {
                return w;
            }
            self.c -= w;
            if !repeat {
                return 0;
            }
        }
    }

    pub(super) fn eq_s(&mut self, s: &[u8]) -> i32 {
        let size = s.len() as i32;
        if self.l - self.c < size || &self.p[self.c as usize..(self.c + size) as usize] != s {
            return 0;
        }
        self.c += size;
        1
    }

    pub(super) fn eq_s_b(&mut self, s: &[u8]) -> i32 {
        let size = s.len() as i32;
        if self.c - self.lb < size || &self.p[(self.c - size) as usize..self.c as usize] != s {
            return 0;
        }
        self.c -= size;
        1
    }

    /// Snowball's `replace_s`: replaces `[c_bra, c_ket)` with `s`, returning
    /// the length adjustment or `None` when the buffer would overflow.
    fn replace(&mut self, c_bra: i32, c_ket: i32, s: &[u8]) -> Option<i32> {
        let adjustment = s.len() as i32 - (c_ket - c_bra);
        let len = self.size;
        if adjustment != 0 {
            if adjustment + len > STEM_BUFFER_BYTES as i32 {
                self.full = true;
                return None;
            }
            self.p
                .copy_within(c_ket as usize..len as usize, (c_ket + adjustment) as usize);
            self.size = adjustment + len;
            self.l += adjustment;
            if self.c >= c_ket {
                self.c += adjustment;
            } else if self.c > c_bra {
                self.c = c_bra;
            }
        }
        self.p[c_bra as usize..c_bra as usize + s.len()].copy_from_slice(s);
        Some(adjustment)
    }

    fn slice_check(&self) -> bool {
        self.bra < 0 || self.bra > self.ket || self.ket > self.l || self.l > self.size
    }

    pub(super) fn slice_from(&mut self, s: &[u8]) -> i32 {
        if self.slice_check() {
            return -1;
        }
        match self.replace(self.bra, self.ket, s) {
            Some(_) => 0,
            None => -1,
        }
    }

    pub(super) fn slice_del(&mut self) -> i32 {
        self.slice_from(b"")
    }

    pub(super) fn insert(&mut self, bra: i32, ket: i32, s: &[u8]) -> i32 {
        let Some(adjustment) = self.replace(bra, ket, s) else {
            return -1;
        };
        if bra <= self.bra {
            self.bra += adjustment;
        }
        if bra <= self.ket {
            self.ket += adjustment;
        }
        0
    }

    /// `slice_to` into string variable `index`; false when the slice is invalid.
    pub(super) fn slice_to_s(&mut self, index: i32) -> i32 {
        debug_assert_eq!(index, 0, "the runtime holds one string variable");
        if self.slice_check() {
            return 0;
        }
        let (bra, ket) = (self.bra as usize, self.ket as usize);
        self.s0[..ket - bra].copy_from_slice(&self.p[bra..ket]);
        self.s0_size = (ket - bra) as i32;
        1
    }

    pub(super) fn eq_v_b_s(&mut self, index: i32) -> i32 {
        debug_assert_eq!(index, 0, "the runtime holds one string variable");
        let size = self.s0_size;
        if self.c - self.lb < size
            || self.p[(self.c - size) as usize..self.c as usize] != self.s0[..size as usize]
        {
            return 0;
        }
        self.c -= size;
        1
    }

    pub(super) fn len_utf8(&self) -> i32 {
        self.p[..self.size as usize]
            .iter()
            .filter(|byte| **byte >= 0xC0 || **byte < 0x80)
            .count() as i32
    }
}

pub(super) fn find_among(z: &mut Env, v: &[Among]) -> i32 {
    let (mut i, mut j) = (0i32, v.len() as i32);
    let (c, l) = (z.c, z.l);
    let (mut common_i, mut common_j) = (0i32, 0i32);
    let mut first_key_inspected = false;
    loop {
        let k = i + ((j - i) >> 1);
        let mut diff = 0;
        let mut common = common_i.min(common_j);
        let w = &v[k as usize];
        for i2 in common..w.s.len() as i32 {
            if c + common == l {
                diff = -1;
                break;
            }
            diff = z.byte(c + common) - i32::from(w.s[i2 as usize]);
            if diff != 0 {
                break;
            }
            common += 1;
        }
        if diff < 0 {
            j = k;
            common_j = common;
        } else {
            i = k;
            common_i = common;
        }
        if j - i <= 1 {
            if i > 0 || j == i || first_key_inspected {
                break;
            }
            first_key_inspected = true;
        }
    }
    loop {
        let w = &v[i as usize];
        let size = w.s.len() as i32;
        if common_i >= size {
            z.c = c + size;
            let Some(function) = w.function else {
                return w.result;
            };
            let res = function(z);
            z.c = c + size;
            if res != 0 {
                return w.result;
            }
        }
        i = w.substring_i;
        if i < 0 {
            return 0;
        }
    }
}

pub(super) fn find_among_b(z: &mut Env, v: &[Among]) -> i32 {
    let (mut i, mut j) = (0i32, v.len() as i32);
    let (c, lb) = (z.c, z.lb);
    let (mut common_i, mut common_j) = (0i32, 0i32);
    let mut first_key_inspected = false;
    loop {
        let k = i + ((j - i) >> 1);
        let mut diff = 0;
        let mut common = common_i.min(common_j);
        let w = &v[k as usize];
        let mut i2 = w.s.len() as i32 - 1 - common;
        while i2 >= 0 {
            if c - common == lb {
                diff = -1;
                break;
            }
            diff = z.byte(c - 1 - common) - i32::from(w.s[i2 as usize]);
            if diff != 0 {
                break;
            }
            common += 1;
            i2 -= 1;
        }
        if diff < 0 {
            j = k;
            common_j = common;
        } else {
            i = k;
            common_i = common;
        }
        if j - i <= 1 {
            if i > 0 || j == i || first_key_inspected {
                break;
            }
            first_key_inspected = true;
        }
    }
    loop {
        let w = &v[i as usize];
        let size = w.s.len() as i32;
        if common_i >= size {
            z.c = c - size;
            let Some(function) = w.function else {
                return w.result;
            };
            let res = function(z);
            z.c = c - size;
            if res != 0 {
                return w.result;
            }
        }
        i = w.substring_i;
        if i < 0 {
            return 0;
        }
    }
}

macro_rules! languages {
    ($($variant:ident => $module:ident),* $(,)?) => {
        /// A Snowball stemmer module PostgreSQL ships for UTF-8 databases.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub(crate) enum SnowballLanguage {
            $($variant),*
        }

        impl SnowballLanguage {
            pub(crate) const ALL: &'static [Self] = &[$(Self::$variant),*];

            pub(crate) const fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => stringify!($module)),*
                }
            }

            fn stemmer(self) -> fn(&mut Env) -> i32 {
                match self {
                    $(Self::$variant => $module::stem),*
                }
            }
        }
    };
}

languages! {
    Arabic => arabic,
    Armenian => armenian,
    Basque => basque,
    Catalan => catalan,
    Danish => danish,
    Dutch => dutch,
    English => english,
    Estonian => estonian,
    Finnish => finnish,
    French => french,
    German => german,
    Greek => greek,
    Hindi => hindi,
    Hungarian => hungarian,
    Indonesian => indonesian,
    Irish => irish,
    Italian => italian,
    Lithuanian => lithuanian,
    Nepali => nepali,
    Norwegian => norwegian,
    Porter => porter,
    Portuguese => portuguese,
    Romanian => romanian,
    Russian => russian,
    Serbian => serbian,
    Spanish => spanish,
    Swedish => swedish,
    Tamil => tamil,
    Turkish => turkish,
    Yiddish => yiddish,
}

impl SnowballLanguage {
    /// `locate_stem_module`: languages match ignoring ASCII case.
    pub(crate) fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|language| language.name().eq_ignore_ascii_case(name))
    }

    /// Stems a lower-cased word into `out`, returning the stem's length.
    /// Like `dsnowball_lexize`, an emptied word keeps its input form.
    pub(crate) fn stem(self, word: &str, out: &mut [u8]) -> Result<usize, StemBufferFull> {
        let mut env = Env::new(word.as_bytes())?;
        let status = (self.stemmer())(&mut env);
        if env.full {
            return Err(StemBufferFull);
        }
        debug_assert!(status >= 0, "only a full buffer fails a Snowball stemmer");
        let result: &[u8] = if env.l > 0 {
            &env.p[..env.l as usize]
        } else {
            word.as_bytes()
        };
        let target = out.get_mut(..result.len()).ok_or(StemBufferFull)?;
        target.copy_from_slice(result);
        Ok(result.len())
    }
}

macro_rules! stopword_lists {
    ($($variant:ident => $file:ident / $words:ident),* $(,)?) => {
        /// A stopword file in PostgreSQL's `tsearch_data` directory.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub(crate) enum StopwordList {
            $($variant),*
        }

        impl StopwordList {
            pub(crate) const ALL: &'static [Self] = &[$(Self::$variant),*];

            pub(crate) const fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => stringify!($file)),*
                }
            }

            const fn words(self) -> &'static [&'static str] {
                match self {
                    $(Self::$variant => stopwords::$words),*
                }
            }
        }
    };
}

stopword_lists! {
    Danish => danish / DANISH,
    Dutch => dutch / DUTCH,
    English => english / ENGLISH,
    Finnish => finnish / FINNISH,
    French => french / FRENCH,
    German => german / GERMAN,
    Hungarian => hungarian / HUNGARIAN,
    Italian => italian / ITALIAN,
    Nepali => nepali / NEPALI,
    Norwegian => norwegian / NORWEGIAN,
    Portuguese => portuguese / PORTUGUESE,
    Russian => russian / RUSSIAN,
    Spanish => spanish / SPANISH,
    Swedish => swedish / SWEDISH,
    Turkish => turkish / TURKISH,
}

impl StopwordList {
    /// The list named by a `StopWords` option; file names match exactly.
    pub(crate) fn parse(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|list| list.name() == name)
    }

    /// `searchstoplist`: a bytewise binary search of the sorted list.
    pub(crate) fn contains(self, word: &str) -> bool {
        self.words().binary_search(&word).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stem(language: SnowballLanguage, word: &str) -> String {
        let mut out = [0u8; STEM_BUFFER_BYTES];
        let len = language.stem(word, &mut out).unwrap();
        String::from_utf8(out[..len].to_vec()).unwrap()
    }

    #[test]
    fn snowball_stemmers_match_postgresql_samples() {
        // ts_lexize results from PostgreSQL 18.6.
        for (language, word, expected) in [
            (SnowballLanguage::English, "running", "run"),
            (SnowballLanguage::English, "generously", "generous"),
            (SnowballLanguage::Porter, "generously", "gener"),
            (SnowballLanguage::German, "häuser", "haus"),
            (SnowballLanguage::French, "continuellement", "continuel"),
            (SnowballLanguage::Russian, "красивые", "красив"),
            (SnowballLanguage::Finnish, "taloissa", "talo"),
            (SnowballLanguage::Danish, "kalkerede", "kalk"),
            (SnowballLanguage::Turkish, "kitaplar", "kitap"),
        ] {
            assert_eq!(stem(language, word), expected, "{language:?} {word}");
        }
    }

    #[test]
    fn snowball_stemmer_reports_a_full_buffer() {
        let word = "a".repeat(STEM_BUFFER_BYTES + 1);
        let mut out = [0u8; STEM_BUFFER_BYTES];
        assert_eq!(
            SnowballLanguage::English.stem(&word, &mut out),
            Err(StemBufferFull)
        );
    }

    #[test]
    fn stopword_lists_are_sorted_and_searchable() {
        for list in StopwordList::ALL {
            assert!(list.words().windows(2).all(|pair| pair[0] < pair[1]));
        }
        assert!(StopwordList::English.contains("the"));
        assert!(StopwordList::Russian.contains("и"));
        assert!(!StopwordList::English.contains("database"));
        assert_eq!(StopwordList::parse("English"), None);
    }
}
