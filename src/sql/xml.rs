//! Allocation-free XML well-formedness and SQL/XML escaping.
//!
//! PostgreSQL's `xml` type stores the original UTF-8 spelling. This parser is
//! therefore a validation boundary, not a normalizer: it checks nested names,
//! attributes, references, comments, CDATA, processing instructions and a
//! bounded document type declaration without rewriting the value.

use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::types::Datum;
use crate::sql_err;
use core::fmt::Write as _;

const MAX_DEPTH: usize = 64;
const MAX_ATTRIBUTES: usize = 128;
const MAX_ENTITIES: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Content,
    Document,
}

pub fn validate(input: &str, mode: Mode) -> Result<(), SqlError> {
    Parser::new(input, mode).parse()
}

pub fn is_well_formed(input: &str, mode: Mode) -> bool {
    validate(input, mode).is_ok()
}

fn invalid(mode: Mode) -> SqlError {
    sql_err!(
        match mode {
            Mode::Document => sqlstate::INVALID_XML_DOCUMENT,
            Mode::Content => sqlstate::INVALID_XML_CONTENT,
        },
        "invalid XML {}",
        match mode {
            Mode::Document => "document",
            Mode::Content => "content",
        }
    )
}

struct Parser<'a> {
    source: &'a str,
    bytes: &'a [u8],
    at: usize,
    mode: Mode,
    roots: usize,
    saw_doctype: bool,
    entities: [&'a str; MAX_ENTITIES],
    entity_count: usize,
}

impl<'a> Parser<'a> {
    fn new(source: &'a str, mode: Mode) -> Self {
        Self {
            source,
            bytes: source.as_bytes(),
            at: 0,
            mode,
            roots: 0,
            saw_doctype: false,
            entities: [""; MAX_ENTITIES],
            entity_count: 0,
        }
    }

    fn parse(mut self) -> Result<(), SqlError> {
        if self.bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
            self.at = 3;
        }
        if self.starts(b"<?xml") && self.boundary(self.at + 5) {
            self.declaration()?;
        }
        while self.at < self.bytes.len() {
            if self.starts(b"<!--") {
                self.comment()?;
            } else if self.starts(b"<?") {
                self.processing_instruction()?;
            } else if self.starts_ci(b"<!DOCTYPE") {
                if self.saw_doctype || self.roots != 0 || self.mode != Mode::Document {
                    return Err(invalid(self.mode));
                }
                self.doctype()?;
                self.saw_doctype = true;
            } else if self.starts(b"<") {
                self.element(0)?;
                self.roots += 1;
            } else {
                let start = self.at;
                self.text()?;
                if self.mode == Mode::Document
                    && !self.source[start..self.at].chars().all(char::is_whitespace)
                {
                    return Err(invalid(self.mode));
                }
            }
        }
        if self.mode == Mode::Document && self.roots != 1 {
            return Err(invalid(self.mode));
        }
        Ok(())
    }

    fn declaration(&mut self) -> Result<(), SqlError> {
        self.at += 5;
        if !self.peek().is_some_and(|byte| byte.is_ascii_whitespace()) {
            return Err(invalid(self.mode));
        }
        let end = self.find(b"?>").ok_or_else(|| invalid(self.mode))?;
        let body = self.source[self.at..end].trim();
        let mut at = 0usize;
        let mut attribute = 0usize;
        while at < body.len() {
            let attribute_name_start = at;
            while body.as_bytes().get(at).is_some_and(u8::is_ascii_alphabetic) {
                at += 1;
            }
            let name = &body[attribute_name_start..at];
            while body.as_bytes().get(at).is_some_and(u8::is_ascii_whitespace) {
                at += 1;
            }
            if body.as_bytes().get(at) != Some(&b'=') {
                return Err(invalid(self.mode));
            }
            at += 1;
            while body.as_bytes().get(at).is_some_and(u8::is_ascii_whitespace) {
                at += 1;
            }
            let quote = *body.as_bytes().get(at).ok_or_else(|| invalid(self.mode))?;
            if !matches!(quote, b'\'' | b'"') {
                return Err(invalid(self.mode));
            }
            at += 1;
            let value_start = at;
            while body.as_bytes().get(at).is_some_and(|byte| *byte != quote) {
                at += 1;
            }
            if body.as_bytes().get(at) != Some(&quote) {
                return Err(invalid(self.mode));
            }
            let value = &body[value_start..at];
            at += 1;
            let valid = match (attribute, name) {
                (0, "version") => matches!(value, "1.0" | "1.1"),
                (1, "encoding") => valid_encoding_name(value),
                (1 | 2, "standalone") => matches!(value, "yes" | "no"),
                _ => false,
            };
            if !valid {
                return Err(invalid(self.mode));
            }
            attribute += 1;
            if at < body.len() {
                if !body.as_bytes()[at].is_ascii_whitespace() {
                    return Err(invalid(self.mode));
                }
                while body.as_bytes().get(at).is_some_and(u8::is_ascii_whitespace) {
                    at += 1;
                }
            }
        }
        if attribute == 0 {
            return Err(invalid(self.mode));
        }
        self.at = end + 2;
        Ok(())
    }

    fn element(&mut self, depth: usize) -> Result<(), SqlError> {
        if depth == MAX_DEPTH || !self.consume(b"<") || self.starts(b"/") {
            return Err(invalid(self.mode));
        }
        let name = self.name()?;
        let mut attributes = [""; MAX_ATTRIBUTES];
        let mut count = 0;
        loop {
            self.space();
            if self.consume(b"/>") {
                return Ok(());
            }
            if self.consume(b">") {
                break;
            }
            if count == MAX_ATTRIBUTES {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "XML element has more than {} attributes",
                    MAX_ATTRIBUTES
                ));
            }
            let attribute = self.name()?;
            if attributes[..count].contains(&attribute) {
                return Err(invalid(self.mode));
            }
            attributes[count] = attribute;
            count += 1;
            self.space();
            if !self.consume(b"=") {
                return Err(invalid(self.mode));
            }
            self.space();
            self.attribute_value()?;
        }
        loop {
            if self.starts(b"</") {
                self.at += 2;
                let close = self.name()?;
                self.space();
                if close != name || !self.consume(b">") {
                    return Err(invalid(self.mode));
                }
                return Ok(());
            }
            if self.starts(b"<!--") {
                self.comment()?;
            } else if self.starts(b"<![CDATA[") {
                self.cdata()?;
            } else if self.starts(b"<?") {
                self.processing_instruction()?;
            } else if self.starts(b"<") {
                self.element(depth + 1)?;
            } else if self.at == self.bytes.len() {
                return Err(invalid(self.mode));
            } else {
                self.text()?;
            }
        }
    }

    fn text(&mut self) -> Result<(), SqlError> {
        while self.at < self.bytes.len() && self.bytes[self.at] != b'<' {
            if self.bytes[self.at] == b'&' {
                self.reference()?;
            } else {
                let character = self.source[self.at..]
                    .chars()
                    .next()
                    .ok_or_else(|| invalid(self.mode))?;
                if !xml_character(character) || self.starts(b"]]>") {
                    return Err(invalid(self.mode));
                }
                self.at += character.len_utf8();
            }
        }
        Ok(())
    }

    fn attribute_value(&mut self) -> Result<(), SqlError> {
        let quote = *self.bytes.get(self.at).ok_or_else(|| invalid(self.mode))?;
        if quote != b'\'' && quote != b'"' {
            return Err(invalid(self.mode));
        }
        self.at += 1;
        while self.at < self.bytes.len() && self.bytes[self.at] != quote {
            if self.bytes[self.at] == b'<' {
                return Err(invalid(self.mode));
            }
            if self.bytes[self.at] == b'&' {
                self.reference()?;
            } else {
                let character = self.source[self.at..].chars().next().unwrap();
                if !xml_character(character) {
                    return Err(invalid(self.mode));
                }
                self.at += character.len_utf8();
            }
        }
        if self.bytes.get(self.at) != Some(&quote) {
            return Err(invalid(self.mode));
        }
        self.at += 1;
        Ok(())
    }

    fn reference(&mut self) -> Result<(), SqlError> {
        self.at += 1;
        let end = self.bytes[self.at..]
            .iter()
            .position(|byte| *byte == b';')
            .map(|offset| self.at + offset)
            .ok_or_else(|| invalid(self.mode))?;
        let reference = &self.source[self.at..end];
        let valid = match reference.strip_prefix('#') {
            Some(hex) if hex.starts_with(['x', 'X']) => u32::from_str_radix(&hex[1..], 16)
                .ok()
                .and_then(char::from_u32)
                .is_some_and(xml_character),
            Some(decimal) => decimal
                .parse::<u32>()
                .ok()
                .and_then(char::from_u32)
                .is_some_and(xml_character),
            None => {
                matches!(reference, "amp" | "lt" | "gt" | "apos" | "quot")
                    || self.entities[..self.entity_count].contains(&reference)
            }
        };
        if !valid {
            return Err(invalid(self.mode));
        }
        self.at = end + 1;
        Ok(())
    }

    fn comment(&mut self) -> Result<(), SqlError> {
        self.at += 4;
        let end = self.find(b"-->").ok_or_else(|| invalid(self.mode))?;
        if self.bytes[self.at..end]
            .windows(2)
            .any(|window| window == b"--")
        {
            return Err(invalid(self.mode));
        }
        self.at = end + 3;
        Ok(())
    }

    fn cdata(&mut self) -> Result<(), SqlError> {
        self.at += 9;
        let end = self.find(b"]]>").ok_or_else(|| invalid(self.mode))?;
        if !self.source[self.at..end].chars().all(xml_character) {
            return Err(invalid(self.mode));
        }
        self.at = end + 3;
        Ok(())
    }

    fn processing_instruction(&mut self) -> Result<(), SqlError> {
        self.at += 2;
        let target = self.name()?;
        if target.eq_ignore_ascii_case("xml") {
            return Err(invalid(self.mode));
        }
        let end = self.find(b"?>").ok_or_else(|| invalid(self.mode))?;
        if !self.source[self.at..end].chars().all(xml_character) {
            return Err(invalid(self.mode));
        }
        self.at = end + 2;
        Ok(())
    }

    fn doctype(&mut self) -> Result<(), SqlError> {
        self.at += 9;
        self.space();
        self.name()?;
        let declarations_start = self.at;
        let mut bracket_depth = 0usize;
        let mut quote = None;
        while let Some(byte) = self.peek() {
            self.at += 1;
            if let Some(delimiter) = quote {
                if byte == delimiter {
                    quote = None;
                }
                continue;
            }
            match byte {
                b'\'' | b'"' => quote = Some(byte),
                b'[' => bracket_depth += 1,
                b']' if bracket_depth != 0 => bracket_depth -= 1,
                b'>' if bracket_depth == 0 => {
                    self.collect_entities(declarations_start, self.at - 1)?;
                    return Ok(());
                }
                _ => {}
            }
        }
        Err(invalid(self.mode))
    }

    fn collect_entities(&mut self, start: usize, end: usize) -> Result<(), SqlError> {
        let declarations = &self.source[start..end];
        let mut at = 0usize;
        while let Some(relative) = declarations[at..].find("<!ENTITY") {
            at += relative + 8;
            while declarations
                .as_bytes()
                .get(at)
                .is_some_and(u8::is_ascii_whitespace)
            {
                at += 1;
            }
            if declarations.as_bytes().get(at) == Some(&b'%') {
                at += 1;
                continue;
            }
            let entity_name_start = at;
            let Some(first) = declarations[at..].chars().next() else {
                return Err(invalid(self.mode));
            };
            if !name_start(first) {
                return Err(invalid(self.mode));
            }
            at += first.len_utf8();
            while let Some(character) = declarations[at..].chars().next() {
                if !name_continue(character) {
                    break;
                }
                at += character.len_utf8();
            }
            if !declarations
                .as_bytes()
                .get(at)
                .is_some_and(u8::is_ascii_whitespace)
            {
                return Err(invalid(self.mode));
            }
            if self.entity_count == self.entities.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "XML document declares more than {} entities",
                    MAX_ENTITIES
                ));
            }
            let name = &declarations[entity_name_start..at];
            if self.entities[..self.entity_count].contains(&name) {
                return Err(invalid(self.mode));
            }
            self.entities[self.entity_count] = name;
            self.entity_count += 1;
        }
        Ok(())
    }

    fn name(&mut self) -> Result<&'a str, SqlError> {
        let start = self.at;
        let first = self.source[self.at..]
            .chars()
            .next()
            .ok_or_else(|| invalid(self.mode))?;
        if !name_start(first) {
            return Err(invalid(self.mode));
        }
        self.at += first.len_utf8();
        while let Some(character) = self.source[self.at..].chars().next() {
            if !name_continue(character) {
                break;
            }
            self.at += character.len_utf8();
        }
        Ok(&self.source[start..self.at])
    }

    fn space(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_whitespace()) {
            self.at += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn starts(&self, value: &[u8]) -> bool {
        self.bytes[self.at..].starts_with(value)
    }

    fn starts_ci(&self, value: &[u8]) -> bool {
        self.bytes[self.at..]
            .get(..value.len())
            .is_some_and(|found| found.eq_ignore_ascii_case(value))
    }

    fn consume(&mut self, value: &[u8]) -> bool {
        if self.starts(value) {
            self.at += value.len();
            true
        } else {
            false
        }
    }

    fn find(&self, needle: &[u8]) -> Option<usize> {
        self.bytes[self.at..]
            .windows(needle.len())
            .position(|window| window == needle)
            .map(|offset| self.at + offset)
    }

    fn boundary(&self, at: usize) -> bool {
        self.bytes.get(at).is_none_or(u8::is_ascii_whitespace)
    }
}

fn valid_encoding_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn name_start(character: char) -> bool {
    character == ':' || character == '_' || character.is_alphabetic() || character as u32 >= 0x80
}

fn name_continue(character: char) -> bool {
    name_start(character) || character.is_ascii_digit() || matches!(character, '-' | '.')
}

fn xml_character(character: char) -> bool {
    matches!(character, '\u{9}' | '\u{a}' | '\u{d}')
        || ('\u{20}'..='\u{d7ff}').contains(&character)
        || ('\u{e000}'..='\u{fffd}').contains(&character)
        || ('\u{10000}'..='\u{10ffff}').contains(&character)
}

/// XML-escapes character data. Attribute callers additionally escape quotes.
pub fn escape_text(input: &str, mut write: impl FnMut(&str)) {
    let mut start = 0;
    for (index, character) in input.char_indices() {
        let replacement = match character {
            '&' => "&amp;",
            '<' => "&lt;",
            '>' => "&gt;",
            _ => continue,
        };
        write(&input[start..index]);
        write(replacement);
        start = index + character.len_utf8();
    }
    write(&input[start..]);
}

pub fn without_declaration(input: &str) -> &str {
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    if input.starts_with("<?xml")
        && let Some(end) = input.find("?>")
    {
        return &input[end + 2..];
    }
    input
}

/// Emits PostgreSQL's `xml_out`/`xml_send` spelling. The stored value keeps
/// its declaration verbatim for casts to text, while XML output removes the
/// client-encoding declaration and the redundant XML 1.0 declaration.
pub fn write_output(input: &str, mut write: impl FnMut(&str)) {
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    let Some(body_start) = input.strip_prefix("<?xml").map(|_| 5) else {
        write(input);
        return;
    };
    let Some(relative_end) = input[body_start..].find("?>") else {
        write(input);
        return;
    };
    let end = body_start + relative_end;
    let body = &input[body_start..end];
    let version = declaration_attribute(body, "version").unwrap_or("1.0");
    let standalone = declaration_attribute(body, "standalone");
    if version != "1.0" || standalone.is_some() {
        write("<?xml version=\"");
        write(version);
        write("\"");
        if let Some(standalone) = standalone {
            write(" standalone=\"");
            write(standalone);
            write("\"");
        }
        write("?>");
    }
    write(&input[end + 2..]);
}

fn declaration_attribute<'a>(body: &'a str, wanted: &str) -> Option<&'a str> {
    let mut at = 0usize;
    while at < body.len() {
        while body.as_bytes().get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        let start = at;
        while body.as_bytes().get(at).is_some_and(u8::is_ascii_alphabetic) {
            at += 1;
        }
        let name = &body[start..at];
        while body.as_bytes().get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        at += usize::from(body.as_bytes().get(at) == Some(&b'='));
        while body.as_bytes().get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        let quote = *body.as_bytes().get(at)?;
        at += 1;
        let value_start = at;
        while body.as_bytes().get(at).is_some_and(|byte| *byte != quote) {
            at += 1;
        }
        let value = &body[value_start..at];
        at += 1;
        if name == wanted {
            return Some(value);
        }
    }
    None
}

pub fn rewrite_namespaces<'a>(
    path: &str,
    document: &str,
    mappings: &[(&str, &str)],
    arena: &'a crate::mem::arena::Arena,
) -> Result<&'a str, SqlError> {
    if mappings.is_empty() {
        return arena.alloc_str(path).map_err(|_| xpath_full());
    }
    let mut source_prefixes = [""; 128];
    if mappings.len() > source_prefixes.len() {
        return Err(xpath_full());
    }
    for (index, (_, uri)) in mappings.iter().enumerate() {
        let mut search = 0usize;
        while let Some(relative) = document[search..].find("xmlns") {
            let start = search + relative;
            let suffix = &document[start + 5..];
            let (prefix, after_name) = if let Some(rest) = suffix.strip_prefix(':') {
                let length = rest.bytes().take_while(|byte| is_name_byte(*byte)).count();
                (&rest[..length], &rest[length..])
            } else {
                ("", suffix)
            };
            let after_name = after_name.trim_start();
            let Some(after_equal) = after_name.strip_prefix('=').map(str::trim_start) else {
                search = start + 5;
                continue;
            };
            let Some(quote) = after_equal
                .chars()
                .next()
                .filter(|c| matches!(c, '\'' | '"'))
            else {
                search = start + 5;
                continue;
            };
            let value = &after_equal[quote.len_utf8()..];
            if let Some(end) = value.find(quote)
                && &value[..end] == *uri
            {
                source_prefixes[index] = prefix;
                break;
            }
            search = start + 5;
        }
        if source_prefixes[index].is_empty() && !document_namespace_is_default(document, uri) {
            source_prefixes[index] = "__pos3ql_namespace_uri_not_present";
        }
    }
    let mut output = crate::util::StackStr::<65_536>::new();
    let mut at = 0usize;
    while at < path.len() {
        let mut replaced = false;
        for (index, (query_prefix, _)) in mappings.iter().enumerate() {
            if query_prefix.is_empty() {
                continue;
            }
            let needle_len = query_prefix.len() + 1;
            if path[at..].starts_with(query_prefix)
                && path.as_bytes().get(at + query_prefix.len()) == Some(&b':')
                && (at == 0 || !is_name_byte(path.as_bytes()[at - 1]))
            {
                if !source_prefixes[index].is_empty() {
                    output
                        .write_str(source_prefixes[index])
                        .map_err(|_| xpath_full())?;
                    output.write_char(':').map_err(|_| xpath_full())?;
                }
                at += needle_len;
                replaced = true;
                break;
            }
        }
        if !replaced {
            let character = path[at..].chars().next().ok_or_else(xpath_error)?;
            output.write_char(character).map_err(|_| xpath_full())?;
            at += character.len_utf8();
        }
    }
    if output.is_truncated() {
        return Err(xpath_full());
    }
    arena.alloc_str(output.as_str()).map_err(|_| xpath_full())
}

fn document_namespace_is_default(document: &str, uri: &str) -> bool {
    let mut search = 0usize;
    while let Some(relative) = document[search..].find("xmlns") {
        let start = search + relative;
        let suffix = &document[start + 5..];
        if suffix.starts_with(':') {
            search = start + 5;
            continue;
        }
        let after_name = suffix.trim_start();
        let Some(after_equal) = after_name.strip_prefix('=').map(str::trim_start) else {
            search = start + 5;
            continue;
        };
        let Some(quote) = after_equal
            .chars()
            .next()
            .filter(|c| matches!(c, '\'' | '"'))
        else {
            search = start + 5;
            continue;
        };
        let value = &after_equal[quote.len_utf8()..];
        if value.find(quote).is_some_and(|end| &value[..end] == uri) {
            return true;
        }
        search = start + 5;
    }
    false
}

const MAX_XPATH_NODES: usize = 1024;
const MAX_XPATH_ATTRIBUTES: usize = 4096;
const MAX_XPATH_STEPS: usize = 64;

#[derive(Clone, Copy)]
struct IndexedElement<'a> {
    name: &'a str,
    parent: i16,
    start: usize,
    inner_start: usize,
    inner_end: usize,
    end: usize,
    attributes_start: u16,
    attributes_len: u16,
}

#[derive(Clone, Copy)]
struct IndexedAttribute<'a> {
    name: &'a str,
    value: &'a str,
}

struct IndexedDocument<'a> {
    source: &'a str,
    elements: &'a [IndexedElement<'a>],
    attributes: &'a [IndexedAttribute<'a>],
}

#[derive(Clone, Copy)]
enum StepKind<'a> {
    Element(&'a str),
    AnyElement,
    Attribute(&'a str),
    Text,
}

#[derive(Clone, Copy)]
struct XPathStep<'a> {
    descendant: bool,
    kind: StepKind<'a>,
    position: Option<usize>,
    attribute_filter: Option<(&'a str, &'a str)>,
}

pub fn xpath_exists(
    path: &str,
    document: &str,
    arena: &crate::mem::arena::Arena,
) -> Result<bool, SqlError> {
    xpath(path, document, arena).map(|values| !values.is_empty())
}

pub fn xpath<'a>(
    path: &str,
    document: &'a str,
    arena: &'a crate::mem::arena::Arena,
) -> Result<&'a [Datum<'a>], SqlError> {
    validate(document, Mode::Document)?;
    let indexed = index_document(document, arena)?;
    let trimmed = path.trim();
    if let Some(inner) = wrapped(trimmed, "count") {
        let count = select(inner, &indexed, arena, false)?.len();
        let value = arena.alloc_str_display(count).map_err(|_| xpath_full())?;
        return arena
            .alloc_slice_copy(&[Datum::Xml(value)])
            .map(|values| &*values)
            .map_err(|_| xpath_full());
    }
    if let Some(inner) = wrapped(trimmed, "boolean") {
        let value = if select(inner, &indexed, arena, false)?.is_empty() {
            "false"
        } else {
            "true"
        };
        return arena
            .alloc_slice_copy(&[Datum::Xml(value)])
            .map(|values| &*values)
            .map_err(|_| xpath_full());
    }
    if let Some(inner) = wrapped(trimmed, "string") {
        let selected = select(inner, &indexed, arena, false)?;
        let text = match selected.first() {
            Some(Datum::Xml(fragment)) => string_value(fragment, arena)?,
            _ => "",
        };
        let mut escaped = crate::util::StackStr::<65_536>::new();
        escape_text(text, |part| {
            let _ = escaped.write_str(part);
        });
        if escaped.is_truncated() {
            return Err(xpath_full());
        }
        let value = arena
            .alloc_str(escaped.as_str())
            .map_err(|_| xpath_full())?;
        return arena
            .alloc_slice_copy(&[Datum::Xml(value)])
            .map(|values| &*values)
            .map_err(|_| xpath_full());
    }
    if let Some(inner) = wrapped(trimmed, "number") {
        let number = if inner.trim().starts_with('/') {
            let selected = select(inner, &indexed, arena, false)?;
            match selected.first() {
                Some(Datum::Xml(fragment)) => string_value(fragment, arena)?
                    .trim()
                    .parse::<f64>()
                    .unwrap_or(f64::NAN),
                _ => f64::NAN,
            }
        } else {
            inner.trim().parse::<f64>().map_err(|_| xpath_error())?
        };
        let value = arena
            .alloc_str_display(crate::sql::types::PgFloat8(number))
            .map_err(|_| xpath_full())?;
        return arena
            .alloc_slice_copy(&[Datum::Xml(value)])
            .map(|values| &*values)
            .map_err(|_| xpath_full());
    }
    if trimmed == "true()" || trimmed == "false()" {
        return arena
            .alloc_slice_copy(&[Datum::Xml(if trimmed.starts_with('t') {
                "true"
            } else {
                "false"
            })])
            .map(|values| &*values)
            .map_err(|_| xpath_full());
    }
    select(trimmed, &indexed, arena, false)
}

pub fn xpath_relative<'a>(
    path: &str,
    context: &'a str,
    arena: &'a crate::mem::arena::Arena,
) -> Result<&'a [Datum<'a>], SqlError> {
    validate(context, Mode::Document)?;
    if path.trim() == "." {
        return arena
            .alloc_slice_copy(&[Datum::Xml(context)])
            .map(|values| &*values)
            .map_err(|_| xpath_full());
    }
    let indexed = index_document(context, arena)?;
    select(path.trim(), &indexed, arena, true)
}

fn xpath_full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "XPath result is too large"
    )
}

fn xpath_error() -> SqlError {
    sql_err!(sqlstate::DATA_EXCEPTION, "invalid XPath expression")
}

fn xpath_unsupported() -> SqlError {
    sql_err!(
        sqlstate::FEATURE_NOT_SUPPORTED,
        "XPath expression is outside the bounded SQL/XML subset"
    )
}

fn wrapped<'a>(value: &'a str, name: &str) -> Option<&'a str> {
    value
        .strip_prefix(name)?
        .strip_prefix('(')?
        .strip_suffix(')')
}

fn index_document<'a>(
    source: &'a str,
    arena: &'a crate::mem::arena::Arena,
) -> Result<IndexedDocument<'a>, SqlError> {
    let bytes = source.as_bytes();
    let mut elements = [IndexedElement {
        name: "",
        parent: -1,
        start: 0,
        inner_start: 0,
        inner_end: 0,
        end: 0,
        attributes_start: 0,
        attributes_len: 0,
    }; MAX_XPATH_NODES];
    let mut attributes = [IndexedAttribute {
        name: "",
        value: "",
    }; MAX_XPATH_ATTRIBUTES];
    let mut stack = [0usize; MAX_DEPTH];
    let mut element_count = 0usize;
    let mut attribute_count = 0usize;
    let mut depth = 0usize;
    let mut at = 0usize;
    while let Some(relative) = source[at..].find('<') {
        at += relative;
        if bytes[at..].starts_with(b"<!--") {
            at = source[at + 4..]
                .find("-->")
                .map(|v| at + 7 + v)
                .ok_or_else(xpath_error)?;
            continue;
        }
        if bytes[at..].starts_with(b"<![CDATA[") {
            at = source[at + 9..]
                .find("]]>")
                .map(|v| at + 12 + v)
                .ok_or_else(xpath_error)?;
            continue;
        }
        if bytes[at..].starts_with(b"<?") {
            at = source[at + 2..]
                .find("?>")
                .map(|v| at + 4 + v)
                .ok_or_else(xpath_error)?;
            continue;
        }
        if bytes[at..].starts_with(b"<!") {
            let mut brackets = 0usize;
            let mut quote = None;
            at += 2;
            while at < bytes.len() {
                let byte = bytes[at];
                at += 1;
                if let Some(q) = quote {
                    if byte == q {
                        quote = None;
                    }
                } else {
                    match byte {
                        b'\'' | b'"' => quote = Some(byte),
                        b'[' => brackets += 1,
                        b']' => brackets = brackets.saturating_sub(1),
                        b'>' if brackets == 0 => break,
                        _ => {}
                    }
                }
            }
            continue;
        }
        if bytes[at..].starts_with(b"</") {
            let close_start = at;
            let close_end = source[at..]
                .find('>')
                .map(|v| at + v + 1)
                .ok_or_else(xpath_error)?;
            if depth == 0 {
                return Err(xpath_error());
            }
            depth -= 1;
            let index = stack[depth];
            elements[index].inner_end = close_start;
            elements[index].end = close_end;
            at = close_end;
            continue;
        }
        if element_count == MAX_XPATH_NODES || depth == MAX_DEPTH {
            return Err(xpath_full());
        }
        let start = at;
        at += 1;
        let name_start = at;
        while at < bytes.len() && is_name_byte(bytes[at]) {
            at += 1;
        }
        let name = &source[name_start..at];
        let attributes_start = attribute_count;
        let mut self_closing = false;
        loop {
            while bytes.get(at).is_some_and(|b| b.is_ascii_whitespace()) {
                at += 1;
            }
            if bytes.get(at) == Some(&b'>') {
                at += 1;
                break;
            }
            if bytes.get(at..at + 2) == Some(b"/>") {
                at += 2;
                self_closing = true;
                break;
            }
            if attribute_count == MAX_XPATH_ATTRIBUTES {
                return Err(xpath_full());
            }
            let attr_start = at;
            while at < bytes.len() && is_name_byte(bytes[at]) {
                at += 1;
            }
            let attr_name = &source[attr_start..at];
            while bytes.get(at).is_some_and(|b| b.is_ascii_whitespace()) {
                at += 1;
            }
            if bytes.get(at) != Some(&b'=') {
                return Err(xpath_error());
            }
            at += 1;
            while bytes.get(at).is_some_and(|b| b.is_ascii_whitespace()) {
                at += 1;
            }
            let quote = *bytes.get(at).ok_or_else(xpath_error)?;
            at += 1;
            let value_start = at;
            while bytes.get(at) != Some(&quote) {
                at += 1;
            }
            let value = &source[value_start..at];
            at += 1;
            attributes[attribute_count] = IndexedAttribute {
                name: attr_name,
                value,
            };
            attribute_count += 1;
        }
        let index = element_count;
        elements[index] = IndexedElement {
            name,
            parent: if depth == 0 {
                -1
            } else {
                stack[depth - 1] as i16
            },
            start,
            inner_start: at,
            inner_end: at,
            end: at,
            attributes_start: attributes_start as u16,
            attributes_len: (attribute_count - attributes_start) as u16,
        };
        element_count += 1;
        if !self_closing {
            stack[depth] = index;
            depth += 1;
        }
    }
    let elements = arena
        .alloc_slice_copy(&elements[..element_count])
        .map_err(|_| xpath_full())?;
    let attributes = arena
        .alloc_slice_copy(&attributes[..attribute_count])
        .map_err(|_| xpath_full())?;
    Ok(IndexedDocument {
        source,
        elements,
        attributes,
    })
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b':' | b'-' | b'.') || byte >= 0x80
}

fn select<'a>(
    path: &str,
    document: &IndexedDocument<'a>,
    arena: &'a crate::mem::arena::Arena,
    relative: bool,
) -> Result<&'a [Datum<'a>], SqlError> {
    let mut steps = [XPathStep {
        descendant: false,
        kind: StepKind::AnyElement,
        position: None,
        attribute_filter: None,
    }; MAX_XPATH_STEPS];
    let count = parse_xpath(path, &mut steps)?;
    let mut current = [-1i16; MAX_XPATH_NODES];
    let mut current_count = 1usize;
    if relative {
        if document.elements.is_empty() {
            return Ok(&[]);
        }
        current[0] = 0;
    }
    for (step_index, step) in steps[..count].iter().enumerate() {
        match step.kind {
            StepKind::Element(_) | StepKind::AnyElement => {
                let mut next = [-1i16; MAX_XPATH_NODES];
                let mut next_count = 0usize;
                for parent in &current[..current_count] {
                    let mut position = 0usize;
                    for (index, element) in document.elements.iter().enumerate() {
                        let relation = if step.descendant {
                            *parent == -1
                                || is_descendant(document.elements, index, *parent as usize)
                        } else {
                            element.parent == *parent
                        };
                        let name_matches = match step.kind {
                            StepKind::Element(name) => element.name == name,
                            StepKind::AnyElement => true,
                            _ => false,
                        };
                        if !relation
                            || !name_matches
                            || !filter_matches(document, element, step.attribute_filter)
                        {
                            continue;
                        }
                        position += 1;
                        if step.position.is_some_and(|wanted| wanted != position) {
                            continue;
                        }
                        if next_count == next.len() {
                            return Err(xpath_full());
                        }
                        next[next_count] = index as i16;
                        next_count += 1;
                    }
                }
                current[..next_count].copy_from_slice(&next[..next_count]);
                current_count = next_count;
            }
            StepKind::Attribute(name) => {
                if step_index + 1 != count {
                    return Err(xpath_error());
                }
                let mut result = [Datum::Null; MAX_XPATH_NODES];
                let mut n = 0;
                for index in &current[..current_count] {
                    let element = &document.elements[*index as usize];
                    for attribute in attributes(document, element) {
                        if attribute.name == name {
                            result[n] = Datum::Xml(attribute.value);
                            n += 1;
                        }
                    }
                }
                return arena
                    .alloc_slice_copy(&result[..n])
                    .map(|values| &*values)
                    .map_err(|_| xpath_full());
            }
            StepKind::Text => {
                if step_index + 1 != count {
                    return Err(xpath_error());
                }
                let mut result = [Datum::Null; MAX_XPATH_NODES];
                let mut n = 0;
                for index in &current[..current_count] {
                    let element = &document.elements[*index as usize];
                    let text = direct_text(document, element, arena)?;
                    if !text.is_empty() {
                        result[n] = Datum::Xml(text);
                        n += 1;
                    }
                }
                return arena
                    .alloc_slice_copy(&result[..n])
                    .map(|values| &*values)
                    .map_err(|_| xpath_full());
            }
        }
    }
    let mut result = [Datum::Null; MAX_XPATH_NODES];
    for (slot, index) in result.iter_mut().zip(&current[..current_count]) {
        let element = &document.elements[*index as usize];
        *slot = Datum::Xml(&document.source[element.start..element.end]);
    }
    arena
        .alloc_slice_copy(&result[..current_count])
        .map(|values| &*values)
        .map_err(|_| xpath_full())
}

pub fn string_value<'a>(
    fragment: &str,
    arena: &'a crate::mem::arena::Arena,
) -> Result<&'a str, SqlError> {
    let mut output = crate::util::StackStr::<65_536>::new();
    let bytes = fragment.as_bytes();
    let mut at = 0usize;
    while at < bytes.len() {
        if bytes[at] == b'<' {
            if bytes[at..].starts_with(b"<![CDATA[") {
                let start = at + 9;
                let end = fragment[start..]
                    .find("]]>")
                    .map(|offset| start + offset)
                    .ok_or_else(xpath_error)?;
                output
                    .write_str(&fragment[start..end])
                    .map_err(|_| xpath_full())?;
                at = end + 3;
            } else if bytes[at..].starts_with(b"<!--") {
                at = fragment[at + 4..]
                    .find("-->")
                    .map(|offset| at + 7 + offset)
                    .ok_or_else(xpath_error)?;
            } else if bytes[at..].starts_with(b"<?") {
                at = fragment[at + 2..]
                    .find("?>")
                    .map(|offset| at + 4 + offset)
                    .ok_or_else(xpath_error)?;
            } else {
                at = fragment[at..]
                    .find('>')
                    .map(|offset| at + offset + 1)
                    .ok_or_else(xpath_error)?;
            }
        } else if bytes[at] == b'&' {
            let end = fragment[at..]
                .find(';')
                .map(|offset| at + offset)
                .ok_or_else(xpath_error)?;
            let entity = &fragment[at + 1..end];
            let character = match entity {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "apos" => '\'',
                "quot" => '"',
                numeric if numeric.starts_with("#x") || numeric.starts_with("#X") => {
                    u32::from_str_radix(&numeric[2..], 16)
                        .ok()
                        .and_then(char::from_u32)
                        .ok_or_else(xpath_error)?
                }
                numeric if numeric.starts_with('#') => numeric[1..]
                    .parse::<u32>()
                    .ok()
                    .and_then(char::from_u32)
                    .ok_or_else(xpath_error)?,
                _ => return Err(xpath_error()),
            };
            output.write_char(character).map_err(|_| xpath_full())?;
            at = end + 1;
        } else {
            let character = fragment[at..].chars().next().ok_or_else(xpath_error)?;
            output.write_char(character).map_err(|_| xpath_full())?;
            at += character.len_utf8();
        }
    }
    if output.is_truncated() {
        return Err(xpath_full());
    }
    arena.alloc_str(output.as_str()).map_err(|_| xpath_full())
}

fn parse_xpath<'a>(path: &'a str, steps: &mut [XPathStep<'a>]) -> Result<usize, SqlError> {
    let bytes = path.as_bytes();
    let mut at = 0usize;
    let mut count = 0usize;
    while at < bytes.len() {
        if bytes[at].is_ascii_whitespace() {
            at += 1;
            continue;
        }
        let descendant = if bytes.get(at..at + 2) == Some(b"//") {
            at += 2;
            true
        } else if bytes.get(at) == Some(&b'/') {
            at += 1;
            false
        } else if count == 0 {
            false
        } else {
            return Err(xpath_unsupported());
        };
        if count == steps.len() {
            return Err(xpath_full());
        }
        let kind = if bytes.get(at) == Some(&b'@') {
            at += 1;
            let start = at;
            while bytes.get(at).is_some_and(|b| is_name_byte(*b)) {
                at += 1;
            }
            StepKind::Attribute(&path[start..at])
        } else if path[at..].starts_with("text()") {
            at += 6;
            StepKind::Text
        } else if bytes.get(at) == Some(&b'*') {
            at += 1;
            StepKind::AnyElement
        } else {
            let start = at;
            while bytes.get(at).is_some_and(|b| is_name_byte(*b)) {
                at += 1;
            }
            if start == at {
                return Err(xpath_error());
            }
            StepKind::Element(&path[start..at])
        };
        let mut position = None;
        let mut attribute_filter = None;
        if bytes.get(at) == Some(&b'[') {
            at += 1;
            if bytes.get(at) == Some(&b'@') {
                at += 1;
                let start = at;
                while bytes.get(at).is_some_and(|b| is_name_byte(*b)) {
                    at += 1;
                }
                let name = &path[start..at];
                if bytes.get(at) != Some(&b'=') {
                    return Err(xpath_error());
                }
                at += 1;
                let quote = *bytes.get(at).ok_or_else(xpath_error)?;
                if !matches!(quote, b'\'' | b'"') {
                    return Err(xpath_error());
                }
                at += 1;
                let value_start = at;
                while bytes.get(at).is_some_and(|byte| *byte != quote) {
                    at += 1;
                }
                if bytes.get(at) != Some(&quote) {
                    return Err(xpath_error());
                }
                attribute_filter = Some((name, &path[value_start..at]));
                at += 1;
            } else {
                let start = at;
                while bytes.get(at).is_some_and(|byte| byte.is_ascii_digit()) {
                    at += 1;
                }
                position = path[start..at].parse().ok();
                if position.is_none() {
                    return Err(xpath_error());
                }
            }
            if bytes.get(at) != Some(&b']') {
                return Err(xpath_unsupported());
            }
            at += 1;
        }
        steps[count] = XPathStep {
            descendant,
            kind,
            position,
            attribute_filter,
        };
        count += 1;
    }
    if count == 0 {
        return Err(xpath_error());
    }
    Ok(count)
}

fn is_descendant(elements: &[IndexedElement<'_>], mut index: usize, ancestor: usize) -> bool {
    while elements[index].parent >= 0 {
        index = elements[index].parent as usize;
        if index == ancestor {
            return true;
        }
    }
    false
}

fn attributes<'a>(
    document: &IndexedDocument<'a>,
    element: &IndexedElement<'_>,
) -> &'a [IndexedAttribute<'a>] {
    let start = element.attributes_start as usize;
    let all = document.attributes;
    &all[start..start + element.attributes_len as usize]
}

fn filter_matches(
    document: &IndexedDocument<'_>,
    element: &IndexedElement<'_>,
    filter: Option<(&str, &str)>,
) -> bool {
    filter.is_none_or(|(name, value)| {
        attributes(document, element)
            .iter()
            .any(|a| a.name == name && a.value == value)
    })
}

fn direct_text<'a>(
    document: &IndexedDocument<'a>,
    element: &IndexedElement<'a>,
    arena: &'a crate::mem::arena::Arena,
) -> Result<&'a str, SqlError> {
    // The common text() case has no child markup. For mixed content, strip
    // markup into an arena buffer while preserving entity spelling.
    let inner = &document.source[element.inner_start..element.inner_end];
    if !inner.contains('<') {
        return Ok(inner);
    }
    let mut out = crate::util::StackStr::<65_536>::new();
    let mut at = 0;
    while at < inner.len() {
        if let Some(open) = inner[at..].find('<') {
            let open = at + open;
            let _ = out.write_str(&inner[at..open]);
            let close = inner[open..]
                .find('>')
                .map(|v| open + v + 1)
                .ok_or_else(xpath_error)?;
            at = close;
        } else {
            let _ = out.write_str(&inner[at..]);
            break;
        }
    }
    if out.is_truncated() {
        return Err(xpath_full());
    }
    arena.alloc_str(out.as_str()).map_err(|_| xpath_full())
}
