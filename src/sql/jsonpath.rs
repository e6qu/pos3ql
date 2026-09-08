//! PostgreSQL SQL/JSON path parsing and canonical text output.
//!
//! A `jsonpath` datum stores only this module's canonical, validated text.  The
//! parsed tree is statement-arena data and is rebuilt when a path is executed;
//! durable rows therefore stay compact without admitting an unvalidated path.

use core::cell::Cell;
use core::fmt::Write as _;

use crate::mem::arena::Arena;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::numeric::Numeric;
use crate::sql_err;

const MAX_STEPS: usize = 256;
const MAX_SUBSCRIPTS: usize = 256;
const MAX_DEPTH: u16 = 128;
const MAX_CANONICAL_BYTES: usize = 65_536;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Lax,
    Strict,
}

#[derive(Debug, Clone, Copy)]
pub struct Path<'a> {
    pub mode: Mode,
    pub expression: &'a Expr<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Plus,
    Minus,
    Not,
    IsUnknown,
    Exists,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Or,
    And,
    Equal,
    NotEqual,
    Less,
    Greater,
    LessOrEqual,
    GreaterOrEqual,
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
    StartsWith,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Abs,
    Size,
    Type,
    Floor,
    Double,
    Ceiling,
    KeyValue,
    Bigint,
    Boolean,
    Date,
    Integer,
    Number,
    String,
    Decimal,
    Datetime,
    Time,
    TimeTz,
    Timestamp,
    TimestampTz,
}

#[derive(Debug, Clone, Copy)]
pub enum Expr<'a> {
    Null,
    Bool(bool),
    Number(Numeric<'a>),
    String(&'a str),
    Root,
    Current,
    Last,
    Variable(&'a str),
    Unary {
        operator: UnaryOp,
        operand: &'a Expr<'a>,
    },
    Binary {
        operator: BinaryOp,
        left: &'a Expr<'a>,
        right: &'a Expr<'a>,
    },
    Chain {
        base: &'a Expr<'a>,
        steps: &'a [Step<'a>],
    },
    LikeRegex {
        operand: &'a Expr<'a>,
        pattern: &'a str,
        flags: &'a str,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct Subscript<'a> {
    pub from: &'a Expr<'a>,
    pub to: Option<&'a Expr<'a>>,
}

#[derive(Debug, Clone, Copy)]
pub enum Step<'a> {
    Key(&'a str),
    AnyKey,
    AnyArray,
    Index(&'a [Subscript<'a>]),
    Descendants {
        first: Option<u32>,
        last: Option<u32>,
        bounded: bool,
    },
    Filter(&'a Expr<'a>),
    Method {
        method: Method,
        first: Option<&'a Expr<'a>>,
        second: Option<&'a Expr<'a>>,
    },
}

fn syntax_error() -> SqlError {
    sql_err!(
        sqlstate::SYNTAX_ERROR,
        "invalid input syntax for type jsonpath"
    )
}

fn limit_error() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "JSON path expression is too complex"
    )
}

pub fn parse<'a>(input: &'a str, arena: &'a Arena) -> Result<Path<'a>, SqlError> {
    let mut parser = Parser {
        bytes: input.as_bytes(),
        at: 0,
        arena,
        depth: 0,
    };
    parser.whitespace();
    let mode = if parser.keyword("strict") {
        Mode::Strict
    } else {
        parser.keyword("lax");
        Mode::Lax
    };
    let expression = parser.expression(0)?;
    parser.whitespace();
    if parser.at != parser.bytes.len() {
        return Err(syntax_error());
    }
    validate_expression(expression)?;
    Ok(Path { mode, expression })
}

fn prepare_like_regex(
    pattern: &str,
    flags: &str,
    output: &mut crate::util::StackStr<4096>,
) -> Result<(), SqlError> {
    if flags.contains('q') {
        for character in pattern.chars() {
            if matches!(
                character,
                '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\'
            ) {
                output.write_char('\\').map_err(|_| limit_error())?;
            }
            output.write_char(character).map_err(|_| limit_error())?;
        }
    } else {
        let mut escaped = false;
        let mut in_class = false;
        let mut comment = false;
        for character in pattern.chars() {
            if comment {
                if character == '\n' {
                    comment = false;
                }
                continue;
            }
            if escaped {
                output.write_char(character).map_err(|_| limit_error())?;
                escaped = false;
                continue;
            }
            if character == '\\' {
                output.write_char(character).map_err(|_| limit_error())?;
                escaped = true;
            } else if character == '[' {
                in_class = true;
                output.write_char(character).map_err(|_| limit_error())?;
            } else if character == ']' && in_class {
                in_class = false;
                output.write_char(character).map_err(|_| limit_error())?;
            } else if flags.contains('x') && !in_class && character == '#' {
                comment = true;
            } else if flags.contains('x') && !in_class && character.is_whitespace() {
                continue;
            } else if !flags.contains('s') && !in_class && character == '.' {
                output.write_str("[^\n]").map_err(|_| limit_error())?;
            } else {
                output.write_char(character).map_err(|_| limit_error())?;
            }
        }
    }
    if output.is_truncated() {
        Err(limit_error())
    } else {
        Ok(())
    }
}

fn validate_expression(expression: &Expr<'_>) -> Result<(), SqlError> {
    fn step(step: &Step<'_>) -> Result<(), SqlError> {
        match step {
            Step::Index(subscripts) => {
                for subscript in *subscripts {
                    validate_expression(subscript.from)?;
                    if let Some(to) = subscript.to {
                        validate_expression(to)?;
                    }
                }
            }
            Step::Filter(expression) => validate_expression(expression)?,
            Step::Method { first, second, .. } => {
                if let Some(first) = first {
                    validate_expression(first)?;
                }
                if let Some(second) = second {
                    validate_expression(second)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    match expression {
        Expr::Unary { operand, .. } => validate_expression(operand),
        Expr::Binary { left, right, .. } => {
            validate_expression(left)?;
            validate_expression(right)
        }
        Expr::Chain { base, steps } => {
            validate_expression(base)?;
            for item in *steps {
                step(item)?;
            }
            Ok(())
        }
        Expr::LikeRegex {
            operand,
            pattern,
            flags,
        } => {
            validate_expression(operand)?;
            let mut prepared = crate::util::StackStr::<4096>::new();
            prepare_like_regex(pattern, flags, &mut prepared)?;
            crate::sql::regex::find(prepared.as_str(), "", 0, flags.contains('i')).map(|_| ())
        }
        _ => Ok(()),
    }
}

/// Validates and returns PostgreSQL's stable text output for a path.
pub fn canonicalize<'a>(input: &'a str, arena: &'a Arena) -> Result<&'a str, SqlError> {
    let path = parse(input, arena)?;
    let mut output = crate::util::StackStr::<MAX_CANONICAL_BYTES>::new();
    path.write(&mut output).map_err(|_| limit_error())?;
    if output.is_truncated() {
        return Err(limit_error());
    }
    arena.alloc_str(output.as_str()).map_err(|_| {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "JSON path exceeds the statement arena"
        )
    })
}

struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
    arena: &'a Arena,
    depth: u16,
}

impl<'a> Parser<'a> {
    fn expression(&mut self, minimum_precedence: u8) -> Result<&'a Expr<'a>, SqlError> {
        self.depth = self.depth.checked_add(1).ok_or_else(limit_error)?;
        if self.depth > MAX_DEPTH {
            return Err(limit_error());
        }
        let mut left = self.prefix()?;
        loop {
            self.whitespace();
            if self.starts_keyword("is") && minimum_precedence <= 2 {
                let saved = self.at;
                self.keyword("is");
                if self.keyword("unknown") {
                    left = self.node(Expr::Unary {
                        operator: UnaryOp::IsUnknown,
                        operand: left,
                    })?;
                    continue;
                }
                self.at = saved;
            }
            if self.starts_keyword("starts") && minimum_precedence <= 2 {
                self.keyword("starts");
                if !self.keyword("with") {
                    return Err(syntax_error());
                }
                let right = self.expression(3)?;
                left = self.node(Expr::Binary {
                    operator: BinaryOp::StartsWith,
                    left,
                    right,
                })?;
                continue;
            }
            if self.starts_keyword("like_regex") && minimum_precedence <= 2 {
                self.keyword("like_regex");
                let pattern = self.string()?;
                let flags = if self.keyword("flag") {
                    self.string()?
                } else {
                    ""
                };
                if flags
                    .bytes()
                    .any(|flag| !matches!(flag, b'i' | b's' | b'm' | b'x' | b'q'))
                {
                    return Err(syntax_error());
                }
                left = self.node(Expr::LikeRegex {
                    operand: left,
                    pattern,
                    flags,
                })?;
                continue;
            }
            let Some((operator, precedence, width)) = self.binary_operator() else {
                break;
            };
            if precedence < minimum_precedence {
                break;
            }
            self.at += width;
            let right = self.expression(precedence + 1)?;
            left = self.node(Expr::Binary {
                operator,
                left,
                right,
            })?;
        }
        self.depth -= 1;
        Ok(left)
    }

    fn prefix(&mut self) -> Result<&'a Expr<'a>, SqlError> {
        self.whitespace();
        if self.eat(b'+') {
            let operand = self.expression(6)?;
            return self.node(Expr::Unary {
                operator: UnaryOp::Plus,
                operand,
            });
        }
        if self.eat(b'-') {
            let operand = self.expression(6)?;
            return self.node(Expr::Unary {
                operator: UnaryOp::Minus,
                operand,
            });
        }
        if self.eat(b'!') {
            let operand = self.delimited()?;
            return self.node(Expr::Unary {
                operator: UnaryOp::Not,
                operand,
            });
        }
        if self.keyword("exists") {
            self.expect(b'(')?;
            let operand = self.expression(0)?;
            self.expect(b')')?;
            return self.node(Expr::Unary {
                operator: UnaryOp::Exists,
                operand,
            });
        }
        let base = if self.eat(b'(') {
            let expression = self.expression(0)?;
            self.expect(b')')?;
            expression
        } else if self.eat(b'$') {
            if self.peek_identifier_start() || self.peek() == Some(b'"') {
                let name = self.name()?;
                self.node(Expr::Variable(name))?
            } else {
                self.node(Expr::Root)?
            }
        } else if self.eat(b'@') {
            self.node(Expr::Current)?
        } else if self.keyword("null") {
            self.node(Expr::Null)?
        } else if self.keyword("true") {
            self.node(Expr::Bool(true))?
        } else if self.keyword("false") {
            self.node(Expr::Bool(false))?
        } else if self.keyword("last") {
            self.node(Expr::Last)?
        } else if self.peek() == Some(b'"') {
            let value = self.string()?;
            self.node(Expr::String(value))?
        } else if self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            let number = self.number()?;
            self.node(Expr::Number(number))?
        } else {
            return Err(syntax_error());
        };
        self.accessors(base)
    }

    fn delimited(&mut self) -> Result<&'a Expr<'a>, SqlError> {
        self.expect(b'(')?;
        let expression = self.expression(0)?;
        self.expect(b')')?;
        Ok(expression)
    }

    fn accessors(&mut self, base: &'a Expr<'a>) -> Result<&'a Expr<'a>, SqlError> {
        let mut steps = [Step::AnyKey; MAX_STEPS];
        let mut count = 0;
        loop {
            self.whitespace();
            let step = if self.eat(b'.') {
                if self.eat(b'*') {
                    if self.eat(b'*') {
                        self.descendants()?
                    } else {
                        Step::AnyKey
                    }
                } else {
                    let name = self.name()?;
                    self.whitespace();
                    if self.peek() == Some(b'(') {
                        let method = method(name).ok_or_else(syntax_error)?;
                        self.at += 1;
                        let (first, second) = self.method_arguments(method)?;
                        self.expect(b')')?;
                        Step::Method {
                            method,
                            first,
                            second,
                        }
                    } else {
                        Step::Key(name)
                    }
                }
            } else if self.eat(b'[') {
                if self.eat(b'*') {
                    self.expect(b']')?;
                    Step::AnyArray
                } else {
                    let mut values = [Subscript {
                        from: base,
                        to: None,
                    }; MAX_SUBSCRIPTS];
                    let mut length = 0;
                    loop {
                        if length == MAX_SUBSCRIPTS {
                            return Err(limit_error());
                        }
                        let from = self.expression(0)?;
                        let to = if self.keyword("to") {
                            Some(self.expression(0)?)
                        } else {
                            None
                        };
                        values[length] = Subscript { from, to };
                        length += 1;
                        self.whitespace();
                        if !self.eat(b',') {
                            break;
                        }
                    }
                    self.expect(b']')?;
                    Step::Index(self.slice(&values[..length])?)
                }
            } else if self.eat(b'?') {
                Step::Filter(self.delimited()?)
            } else {
                break;
            };
            if count == MAX_STEPS {
                return Err(limit_error());
            }
            steps[count] = step;
            count += 1;
        }
        if count == 0 {
            Ok(base)
        } else {
            self.node(Expr::Chain {
                base,
                steps: self.slice(&steps[..count])?,
            })
        }
    }

    fn descendants(&mut self) -> Result<Step<'a>, SqlError> {
        self.whitespace();
        if !self.eat(b'{') {
            return Ok(Step::Descendants {
                first: Some(0),
                last: None,
                bounded: false,
            });
        }
        let first = self.level()?;
        let last = if self.keyword("to") {
            self.level()?
        } else {
            first
        };
        self.expect(b'}')?;
        if let (Some(first), Some(last)) = (first, last)
            && first > last
        {
            return Err(syntax_error());
        }
        Ok(Step::Descendants {
            first,
            last,
            bounded: true,
        })
    }

    fn level(&mut self) -> Result<Option<u32>, SqlError> {
        if self.keyword("last") {
            return Ok(None);
        }
        self.whitespace();
        let start = self.at;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.at += 1;
        }
        if start == self.at {
            return Err(syntax_error());
        }
        core::str::from_utf8(&self.bytes[start..self.at])
            .ok()
            .and_then(|value| value.parse().ok())
            .map(Some)
            .ok_or_else(syntax_error)
    }

    fn method_arguments(
        &mut self,
        method: Method,
    ) -> Result<(Option<&'a Expr<'a>>, Option<&'a Expr<'a>>), SqlError> {
        self.whitespace();
        if self.peek() == Some(b')') {
            return Ok((None, None));
        }
        if !matches!(
            method,
            Method::Decimal
                | Method::Datetime
                | Method::Time
                | Method::TimeTz
                | Method::Timestamp
                | Method::TimestampTz
        ) {
            return Err(syntax_error());
        }
        let first = Some(self.expression(0)?);
        let second = if self.eat(b',') {
            if method != Method::Decimal {
                return Err(syntax_error());
            }
            Some(self.expression(0)?)
        } else {
            None
        };
        Ok((first, second))
    }

    fn number(&mut self) -> Result<Numeric<'a>, SqlError> {
        self.whitespace();
        let start = self.at;
        if self.eat(b'0') {
            if self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                return Err(syntax_error());
            }
        } else {
            if !self.peek().is_some_and(|byte| matches!(byte, b'1'..=b'9')) {
                return Err(syntax_error());
            }
            while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                self.at += 1;
            }
        }
        if self.eat(b'.') {
            let fraction = self.at;
            while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                self.at += 1;
            }
            if fraction == self.at {
                return Err(syntax_error());
            }
        }
        if self.peek().is_some_and(|byte| matches!(byte, b'e' | b'E')) {
            self.at += 1;
            if self.peek().is_some_and(|byte| matches!(byte, b'+' | b'-')) {
                self.at += 1;
            }
            let exponent = self.at;
            while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                self.at += 1;
            }
            if exponent == self.at {
                return Err(syntax_error());
            }
        }
        let raw = core::str::from_utf8(&self.bytes[start..self.at]).map_err(|_| syntax_error())?;
        Numeric::parse(raw, self.arena).map_err(|_| syntax_error())
    }

    fn name(&mut self) -> Result<&'a str, SqlError> {
        self.whitespace();
        if self.peek() == Some(b'"') {
            return self.string();
        }
        let start = self.at;
        while self.peek().is_some_and(identifier_continue) {
            self.at += 1;
        }
        if start == self.at {
            return Err(syntax_error());
        }
        core::str::from_utf8(&self.bytes[start..self.at]).map_err(|_| syntax_error())
    }

    fn string(&mut self) -> Result<&'a str, SqlError> {
        self.whitespace();
        if !self.eat(b'"') {
            return Err(syntax_error());
        }
        let start = self.at;
        loop {
            match self.peek() {
                None => return Err(syntax_error()),
                Some(b'"') => {
                    let raw = core::str::from_utf8(&self.bytes[start..self.at])
                        .map_err(|_| syntax_error())?;
                    self.at += 1;
                    return crate::sql::json::decode_string(raw, self.arena)
                        .map_err(|_| syntax_error());
                }
                Some(b'\\') => {
                    self.at += 1;
                    match self.peek() {
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => {
                            self.at += 1
                        }
                        Some(b'u') => {
                            self.at += 1;
                            for _ in 0..4 {
                                if !self.peek().is_some_and(|byte| byte.is_ascii_hexdigit()) {
                                    return Err(syntax_error());
                                }
                                self.at += 1;
                            }
                        }
                        _ => return Err(syntax_error()),
                    }
                }
                Some(byte) if byte < 0x20 => return Err(syntax_error()),
                Some(_) => self.at += 1,
            }
        }
    }

    fn binary_operator(&self) -> Option<(BinaryOp, u8, usize)> {
        let remaining = &self.bytes[self.at..];
        for (token, operator, precedence) in [
            (b"||".as_slice(), BinaryOp::Or, 0),
            (b"&&".as_slice(), BinaryOp::And, 1),
            (b"==".as_slice(), BinaryOp::Equal, 2),
            (b"!=".as_slice(), BinaryOp::NotEqual, 2),
            (b"<=".as_slice(), BinaryOp::LessOrEqual, 2),
            (b">=".as_slice(), BinaryOp::GreaterOrEqual, 2),
            (b"<".as_slice(), BinaryOp::Less, 2),
            (b">".as_slice(), BinaryOp::Greater, 2),
            (b"+".as_slice(), BinaryOp::Add, 3),
            (b"-".as_slice(), BinaryOp::Subtract, 3),
            (b"*".as_slice(), BinaryOp::Multiply, 4),
            (b"/".as_slice(), BinaryOp::Divide, 4),
            (b"%".as_slice(), BinaryOp::Modulo, 4),
        ] {
            if remaining.starts_with(token) {
                return Some((operator, precedence, token.len()));
            }
        }
        None
    }

    fn starts_keyword(&mut self, keyword: &str) -> bool {
        let saved = self.at;
        let result = self.keyword(keyword);
        self.at = saved;
        result
    }

    fn keyword(&mut self, keyword: &str) -> bool {
        self.whitespace();
        let remaining = &self.bytes[self.at..];
        if remaining.len() < keyword.len()
            || !remaining[..keyword.len()].eq_ignore_ascii_case(keyword.as_bytes())
            || remaining
                .get(keyword.len())
                .is_some_and(|byte| identifier_continue(*byte))
        {
            return false;
        }
        self.at += keyword.len();
        true
    }

    fn whitespace(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_whitespace()) {
            self.at += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), SqlError> {
        self.whitespace();
        if self.eat(byte) {
            Ok(())
        } else {
            Err(syntax_error())
        }
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn peek_identifier_start(&self) -> bool {
        self.peek()
            .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic() || byte >= 0x80)
    }

    fn node(&self, expression: Expr<'a>) -> Result<&'a Expr<'a>, SqlError> {
        self.arena
            .alloc(expression)
            .map(|expression| &*expression)
            .map_err(|_| limit_error())
    }

    fn slice<T: Copy>(&self, values: &[T]) -> Result<&'a [T], SqlError> {
        self.arena
            .alloc_slice_copy(values)
            .map(|values| &*values)
            .map_err(|_| limit_error())
    }
}

fn identifier_continue(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric() || byte >= 0x80
}

fn method(name: &str) -> Option<Method> {
    for (candidate, value) in [
        ("abs", Method::Abs),
        ("size", Method::Size),
        ("type", Method::Type),
        ("floor", Method::Floor),
        ("double", Method::Double),
        ("ceiling", Method::Ceiling),
        ("keyvalue", Method::KeyValue),
        ("bigint", Method::Bigint),
        ("boolean", Method::Boolean),
        ("date", Method::Date),
        ("integer", Method::Integer),
        ("number", Method::Number),
        ("string", Method::String),
        ("decimal", Method::Decimal),
        ("datetime", Method::Datetime),
        ("time", Method::Time),
        ("time_tz", Method::TimeTz),
        ("timestamp", Method::Timestamp),
        ("timestamp_tz", Method::TimestampTz),
    ] {
        if name.eq_ignore_ascii_case(candidate) {
            return Some(value);
        }
    }
    None
}

impl Path<'_> {
    fn write(&self, output: &mut dyn core::fmt::Write) -> core::fmt::Result {
        if self.mode == Mode::Strict {
            output.write_str("strict ")?;
        }
        let delimited = matches!(
            *self.expression,
            Expr::Binary { .. } | Expr::LikeRegex { .. }
        );
        if delimited {
            output.write_char('(')?;
        }
        self.expression.write(output, 0)?;
        if delimited {
            output.write_char(')')?;
        }
        Ok(())
    }
}

impl Expr<'_> {
    fn precedence(&self) -> u8 {
        match self {
            Expr::Binary {
                operator: BinaryOp::Or,
                ..
            } => 0,
            Expr::Binary {
                operator: BinaryOp::And,
                ..
            } => 1,
            Expr::Binary {
                operator:
                    BinaryOp::Equal
                    | BinaryOp::NotEqual
                    | BinaryOp::Less
                    | BinaryOp::Greater
                    | BinaryOp::LessOrEqual
                    | BinaryOp::GreaterOrEqual
                    | BinaryOp::StartsWith,
                ..
            }
            | Expr::LikeRegex { .. }
            | Expr::Unary {
                operator: UnaryOp::IsUnknown,
                ..
            } => 2,
            Expr::Binary {
                operator: BinaryOp::Add | BinaryOp::Subtract,
                ..
            } => 3,
            Expr::Binary {
                operator: BinaryOp::Multiply | BinaryOp::Divide | BinaryOp::Modulo,
                ..
            } => 4,
            Expr::Unary { .. } => 5,
            _ => 6,
        }
    }

    fn write(&self, output: &mut dyn core::fmt::Write, parent: u8) -> core::fmt::Result {
        let precedence = self.precedence();
        let parentheses = precedence < parent;
        if parentheses {
            output.write_char('(')?;
        }
        match self {
            Expr::Null => output.write_str("null")?,
            Expr::Bool(value) => output.write_str(if *value { "true" } else { "false" })?,
            Expr::Number(value) => write!(output, "{value}")?,
            Expr::String(value) => crate::sql::json::write_json_raw_string(value, output)?,
            Expr::Root => output.write_char('$')?,
            Expr::Current => output.write_char('@')?,
            Expr::Last => output.write_str("last")?,
            Expr::Variable(name) => {
                output.write_char('$')?;
                crate::sql::json::write_json_raw_string(name, output)?;
            }
            Expr::Unary { operator, operand } => match operator {
                UnaryOp::Plus => {
                    output.write_char('+')?;
                    operand.write(output, precedence)?;
                }
                UnaryOp::Minus => {
                    output.write_char('-')?;
                    operand.write(output, precedence)?;
                }
                UnaryOp::Not => {
                    output.write_str("!(")?;
                    operand.write(output, 0)?;
                    output.write_char(')')?;
                }
                UnaryOp::IsUnknown => {
                    output.write_char('(')?;
                    operand.write(output, 0)?;
                    output.write_str(") is unknown")?;
                }
                UnaryOp::Exists => {
                    output.write_str("exists (")?;
                    operand.write(output, 0)?;
                    output.write_char(')')?;
                }
            },
            Expr::Binary {
                operator,
                left,
                right,
            } => {
                left.write(output, precedence)?;
                write!(output, " {} ", operator.text())?;
                right.write(output, precedence + 1)?;
            }
            Expr::Chain { base, steps } => {
                if matches!(**base, Expr::Binary { .. } | Expr::LikeRegex { .. }) {
                    output.write_char('(')?;
                    base.write(output, 0)?;
                    output.write_char(')')?;
                } else {
                    base.write(output, precedence)?;
                }
                for step in *steps {
                    step.write(output)?;
                }
            }
            Expr::LikeRegex {
                operand,
                pattern,
                flags,
            } => {
                operand.write(output, precedence)?;
                output.write_str(" like_regex ")?;
                crate::sql::json::write_json_raw_string(pattern, output)?;
                if !flags.is_empty() {
                    output.write_str(" flag ")?;
                    crate::sql::json::write_json_raw_string(flags, output)?;
                }
            }
        }
        if parentheses {
            output.write_char(')')?;
        }
        Ok(())
    }
}

impl BinaryOp {
    fn text(self) -> &'static str {
        match self {
            Self::Or => "||",
            Self::And => "&&",
            Self::Equal => "==",
            Self::NotEqual => "!=",
            Self::Less => "<",
            Self::Greater => ">",
            Self::LessOrEqual => "<=",
            Self::GreaterOrEqual => ">=",
            Self::Add => "+",
            Self::Subtract => "-",
            Self::Multiply => "*",
            Self::Divide => "/",
            Self::Modulo => "%",
            Self::StartsWith => "starts with",
        }
    }
}

impl Step<'_> {
    fn write(&self, output: &mut dyn core::fmt::Write) -> core::fmt::Result {
        match self {
            Step::Key(key) => {
                output.write_char('.')?;
                crate::sql::json::write_json_raw_string(key, output)
            }
            Step::AnyKey => output.write_str(".*"),
            Step::AnyArray => output.write_str("[*]"),
            Step::Index(values) => {
                output.write_char('[')?;
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        output.write_char(',')?;
                    }
                    value.from.write(output, 0)?;
                    if let Some(to) = value.to {
                        output.write_str(" to ")?;
                        to.write(output, 0)?;
                    }
                }
                output.write_char(']')
            }
            Step::Descendants {
                first,
                last,
                bounded,
            } => {
                output.write_str(".**")?;
                if *bounded {
                    output.write_char('{')?;
                    write_level(output, *first)?;
                    if first != last {
                        output.write_str(" to ")?;
                        write_level(output, *last)?;
                    }
                    output.write_char('}')?;
                }
                Ok(())
            }
            Step::Filter(predicate) => {
                output.write_str("?(")?;
                predicate.write(output, 0)?;
                output.write_char(')')
            }
            Step::Method {
                method,
                first,
                second,
            } => {
                write!(output, ".{}(", method.text())?;
                if let Some(first) = first {
                    first.write(output, 0)?;
                }
                if let Some(second) = second {
                    output.write_char(',')?;
                    second.write(output, 0)?;
                }
                output.write_char(')')
            }
        }
    }
}

fn write_level(output: &mut dyn core::fmt::Write, level: Option<u32>) -> core::fmt::Result {
    match level {
        Some(level) => write!(output, "{level}"),
        None => output.write_str("last"),
    }
}

impl Method {
    fn text(self) -> &'static str {
        match self {
            Self::Abs => "abs",
            Self::Size => "size",
            Self::Type => "type",
            Self::Floor => "floor",
            Self::Double => "double",
            Self::Ceiling => "ceiling",
            Self::KeyValue => "keyvalue",
            Self::Bigint => "bigint",
            Self::Boolean => "boolean",
            Self::Date => "date",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::String => "string",
            Self::Decimal => "decimal",
            Self::Datetime => "datetime",
            Self::Time => "time",
            Self::TimeTz => "time_tz",
            Self::Timestamp => "timestamp",
            Self::TimestampTz => "timestamp_tz",
        }
    }
}

const MAX_RESULTS: usize = 1024;

/// Executes a validated path against a jsonb document.  Both the document and
/// variables are parsed through the canonical JSON boundary; every result is
/// an arena-backed JSON tree suitable for direct jsonb serialization.
pub fn query<'a>(
    target: &'a str,
    path: &'a str,
    variables: Option<&'a str>,
    silent: bool,
    arena: &'a Arena,
) -> Result<&'a [crate::sql::json::Json<'a>], SqlError> {
    query_with_timezone(target, path, variables, silent, false, arena)
}

pub fn query_with_timezone<'a>(
    target: &'a str,
    path: &'a str,
    variables: Option<&'a str>,
    silent: bool,
    use_timezone: bool,
    arena: &'a Arena,
) -> Result<&'a [crate::sql::json::Json<'a>], SqlError> {
    query_outcome(target, path, variables, silent, use_timezone, arena)
        .map(|outcome| outcome.values)
}

pub(crate) struct QueryOutcome<'a> {
    pub values: &'a [crate::sql::json::Json<'a>],
    pub suppressed_error: bool,
}

pub(crate) fn query_outcome<'a>(
    target: &'a str,
    path: &'a str,
    variables: Option<&'a str>,
    silent: bool,
    use_timezone: bool,
    arena: &'a Arena,
) -> Result<QueryOutcome<'a>, SqlError> {
    let root = crate::sql::json::parse(target, arena)?;
    let parsed = parse(path, arena)?;
    let variables = match variables {
        Some(text) => {
            let value = crate::sql::json::parse(text, arena)?;
            if !matches!(value, crate::sql::json::Json::Object(_)) {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "JSON path variables must be a JSON object"
                ));
            }
            Some(value)
        }
        None => None,
    };
    let evaluator = Evaluator {
        mode: parsed.mode,
        root,
        variables,
        use_timezone,
        next_generated_object_id: Cell::new(1 + variable_count(variables) as i64),
        arena,
    };
    match evaluator.expression(parsed.expression, root, None) {
        Ok(values) => Ok(QueryOutcome {
            values,
            suppressed_error: false,
        }),
        // PostgreSQL suppresses structural, numeric, and datetime failures,
        // but a missing PASSING variable is a catalog/name error and must
        // remain visible even when `silent` is true.
        Err(error) if silent && error.sqlstate != sqlstate::UNDEFINED_OBJECT => Ok(QueryOutcome {
            values: &[],
            suppressed_error: true,
        }),
        Err(error) => Err(error),
    }
}

fn variable_count(variables: Option<crate::sql::json::Json<'_>>) -> usize {
    match variables {
        Some(crate::sql::json::Json::Object(members)) => members.len(),
        _ => 0,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Truth {
    True,
    False,
    Unknown,
}

struct Evaluator<'a> {
    mode: Mode,
    root: crate::sql::json::Json<'a>,
    variables: Option<crate::sql::json::Json<'a>>,
    use_timezone: bool,
    next_generated_object_id: Cell<i64>,
    arena: &'a Arena,
}

impl<'a> Evaluator<'a> {
    fn expression(
        &self,
        expression: &'a Expr<'a>,
        current: crate::sql::json::Json<'a>,
        last: Option<usize>,
    ) -> Result<&'a [crate::sql::json::Json<'a>], SqlError> {
        use crate::sql::json::Json;
        match expression {
            Expr::Null => self.one(Json::Null),
            Expr::Bool(value) => self.one(Json::Bool(*value)),
            Expr::Number(value) => self.one(Json::Number(self.numeric_text(value)?)),
            Expr::String(value) => self.one(Json::Str(value)),
            Expr::Root => self.one(self.root),
            Expr::Current => self.one(current),
            Expr::Last => {
                let last = last.ok_or_else(runtime_error)?;
                self.one(Json::Number(self.integer_text(last as i64)?))
            }
            Expr::Variable(name) => {
                let Some(Json::Object(members)) = self.variables else {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "could not find jsonpath variable \"{}\"",
                        name
                    ));
                };
                match members.iter().find(|(key, _)| key == name) {
                    Some((_, value)) => self.one(*value),
                    None => Err(sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "could not find jsonpath variable \"{}\"",
                        name
                    )),
                }
            }
            Expr::Chain { base, steps } => {
                let mut values = self.expression(base, current, last)?;
                for step in *steps {
                    values = self.step(values, step)?;
                }
                Ok(values)
            }
            Expr::Unary { operator, operand } => match operator {
                UnaryOp::Exists => self.one(Json::Bool(
                    self.expression(operand, current, last)
                        .map(|values| !values.is_empty())
                        .unwrap_or(false),
                )),
                UnaryOp::Not => self.one(self.truth_json(
                    match self.predicate(operand, current, last)? {
                        Truth::True => Truth::False,
                        Truth::False => Truth::True,
                        Truth::Unknown => Truth::Unknown,
                    },
                )),
                UnaryOp::IsUnknown => self.one(Json::Bool(
                    self.predicate(operand, current, last)? == Truth::Unknown,
                )),
                UnaryOp::Plus | UnaryOp::Minus => {
                    let value = self.singleton(operand, current, last)?;
                    let number = self.number(value)?;
                    if *operator == UnaryOp::Plus {
                        self.one(Json::Number(self.numeric_text(&number)?))
                    } else {
                        let negated =
                            crate::sql::numeric::sub(&Numeric::ZERO, &number, self.arena)?;
                        self.one(Json::Number(self.numeric_text(&negated)?))
                    }
                }
            },
            Expr::Binary {
                operator,
                left,
                right,
            } => self.binary(*operator, left, right, current, last),
            Expr::LikeRegex {
                operand,
                pattern,
                flags,
            } => {
                let values = self.expression(operand, current, last)?;
                let mut answer = Truth::False;
                for value in values {
                    let Json::Str(text) = value else {
                        answer = Truth::Unknown;
                        continue;
                    };
                    let insensitive = flags.contains('i');
                    let mut prepared_pattern = crate::util::StackStr::<4096>::new();
                    prepare_like_regex(pattern, flags, &mut prepared_pattern)?;
                    let effective = prepared_pattern.as_str();
                    let matched = if flags.contains('m') {
                        let mut matched = false;
                        for line in text.split('\n') {
                            if crate::sql::regex::find(effective, line, 0, insensitive)?.is_some() {
                                matched = true;
                                break;
                            }
                        }
                        matched
                    } else {
                        crate::sql::regex::find(effective, text, 0, insensitive)?.is_some()
                    };
                    if matched {
                        answer = Truth::True;
                        break;
                    }
                }
                self.one(self.truth_json(answer))
            }
        }
    }

    fn binary(
        &self,
        operator: BinaryOp,
        left: &'a Expr<'a>,
        right: &'a Expr<'a>,
        current: crate::sql::json::Json<'a>,
        last: Option<usize>,
    ) -> Result<&'a [crate::sql::json::Json<'a>], SqlError> {
        use crate::sql::json::Json;
        if matches!(operator, BinaryOp::And | BinaryOp::Or) {
            let left = self.predicate(left, current, last)?;
            if (operator == BinaryOp::And && left == Truth::False)
                || (operator == BinaryOp::Or && left == Truth::True)
            {
                return self.one(Json::Bool(operator == BinaryOp::Or));
            }
            let right = self.predicate(right, current, last)?;
            let answer = match operator {
                BinaryOp::And => {
                    if right == Truth::False {
                        Truth::False
                    } else if left == Truth::True && right == Truth::True {
                        Truth::True
                    } else {
                        Truth::Unknown
                    }
                }
                BinaryOp::Or => {
                    if right == Truth::True {
                        Truth::True
                    } else if left == Truth::False && right == Truth::False {
                        Truth::False
                    } else {
                        Truth::Unknown
                    }
                }
                _ => unreachable!(),
            };
            return self.one(self.truth_json(answer));
        }
        if matches!(
            operator,
            BinaryOp::Equal
                | BinaryOp::NotEqual
                | BinaryOp::Less
                | BinaryOp::Greater
                | BinaryOp::LessOrEqual
                | BinaryOp::GreaterOrEqual
        ) {
            let left_values = self.expression(left, current, last)?;
            let right_values = self.expression(right, current, last)?;
            let mut answer = Truth::False;
            for left in left_values {
                for right in right_values {
                    match compare_json(left, right, self.use_timezone)? {
                        Some(ordering) => {
                            let matched = match operator {
                                BinaryOp::Equal => ordering.is_eq(),
                                BinaryOp::NotEqual => !ordering.is_eq(),
                                BinaryOp::Less => ordering.is_lt(),
                                BinaryOp::Greater => ordering.is_gt(),
                                BinaryOp::LessOrEqual => !ordering.is_gt(),
                                BinaryOp::GreaterOrEqual => !ordering.is_lt(),
                                _ => unreachable!(),
                            };
                            if matched {
                                return self.one(Json::Bool(true));
                            }
                        }
                        None => answer = Truth::Unknown,
                    }
                }
            }
            return self.one(self.truth_json(answer));
        }
        if operator == BinaryOp::StartsWith {
            let left_values = self.expression(left, current, last)?;
            let right = self.singleton(right, current, last)?;
            let Json::Str(prefix) = right else {
                return self.one(Json::Null);
            };
            let mut unknown = false;
            for left in left_values {
                match left {
                    Json::Str(text) if text.starts_with(prefix) => {
                        return self.one(Json::Bool(true));
                    }
                    Json::Str(_) => {}
                    _ => unknown = true,
                }
            }
            return self.one(if unknown {
                Json::Null
            } else {
                Json::Bool(false)
            });
        }

        let left = self.number(self.singleton(left, current, last)?)?;
        let right = self.number(self.singleton(right, current, last)?)?;
        let value = match operator {
            BinaryOp::Add => crate::sql::numeric::add(&left, &right, self.arena),
            BinaryOp::Subtract => crate::sql::numeric::sub(&left, &right, self.arena),
            BinaryOp::Multiply => crate::sql::numeric::mul(&left, &right, self.arena),
            BinaryOp::Divide => crate::sql::numeric::div(&left, &right, self.arena),
            BinaryOp::Modulo => crate::sql::numeric::rem(&left, &right, self.arena),
            _ => unreachable!(),
        }?;
        self.one(Json::Number(self.numeric_text(&value)?))
    }

    fn step(
        &self,
        input: &'a [crate::sql::json::Json<'a>],
        step: &'a Step<'a>,
    ) -> Result<&'a [crate::sql::json::Json<'a>], SqlError> {
        use crate::sql::json::Json;
        let mut output = [Json::Null; MAX_RESULTS];
        let mut count = 0;
        for value in input {
            match step {
                Step::Key(key) => self.member(*value, key, &mut output, &mut count)?,
                Step::AnyKey => self.any_member(*value, &mut output, &mut count)?,
                Step::AnyArray => match value {
                    Json::Array(items) => self.extend(&mut output, &mut count, items)?,
                    _ if self.mode == Mode::Lax => self.push(&mut output, &mut count, *value)?,
                    _ => return Err(array_not_found("jsonpath wildcard array accessor")),
                },
                Step::Index(subscripts) => {
                    let singleton = [*value];
                    let items = match value {
                        Json::Array(items) => *items,
                        _ if self.mode == Mode::Lax => &singleton,
                        _ => return Err(array_not_found("jsonpath array accessor")),
                    };
                    if items.is_empty() && self.mode == Mode::Lax {
                        continue;
                    }
                    let last = items.len().checked_sub(1);
                    for subscript in *subscripts {
                        let from = self.index(subscript.from, *value, last)?;
                        let to = match subscript.to {
                            Some(to) => self.index(to, *value, last)?,
                            None => from,
                        };
                        if from <= to {
                            for index in from..=to {
                                if let Some(item) = items.get(index) {
                                    self.push(&mut output, &mut count, *item)?;
                                } else if self.mode == Mode::Strict {
                                    return Err(invalid_subscript(
                                        "jsonpath array subscript is out of bounds",
                                    ));
                                }
                            }
                        }
                    }
                }
                Step::Descendants { first, last, .. } => {
                    self.descendants(*value, 0, *first, *last, &mut output, &mut count)?;
                }
                Step::Filter(predicate) => {
                    if self.predicate(predicate, *value, None)? == Truth::True {
                        self.push(&mut output, &mut count, *value)?;
                    }
                }
                Step::Method {
                    method,
                    first,
                    second,
                } => {
                    self.method(*value, *method, *first, *second, &mut output, &mut count)?;
                }
            }
        }
        self.results(&output[..count])
    }

    fn member(
        &self,
        value: crate::sql::json::Json<'a>,
        key: &str,
        output: &mut [crate::sql::json::Json<'a>; MAX_RESULTS],
        count: &mut usize,
    ) -> Result<(), SqlError> {
        use crate::sql::json::Json;
        match value {
            Json::Object(members) => match members.iter().find(|(name, _)| *name == key) {
                Some((_, value)) => self.push(output, count, *value),
                None if self.mode == Mode::Lax => Ok(()),
                None => Err(sql_err!(
                    sqlstate::SQL_JSON_MEMBER_NOT_FOUND,
                    "JSON object does not contain key \"{}\"",
                    key
                )),
            },
            Json::Array(items) if self.mode == Mode::Lax => {
                for item in items {
                    self.member(*item, key, output, count)?;
                }
                Ok(())
            }
            _ if self.mode == Mode::Lax => Ok(()),
            _ => Err(sql_err!(
                sqlstate::SQL_JSON_MEMBER_NOT_FOUND,
                "jsonpath member accessor can only be applied to an object"
            )),
        }
    }

    fn any_member(
        &self,
        value: crate::sql::json::Json<'a>,
        output: &mut [crate::sql::json::Json<'a>; MAX_RESULTS],
        count: &mut usize,
    ) -> Result<(), SqlError> {
        use crate::sql::json::Json;
        match value {
            Json::Object(members) => {
                for (_, value) in members {
                    self.push(output, count, *value)?;
                }
                Ok(())
            }
            Json::Array(items) if self.mode == Mode::Lax => {
                for item in items {
                    self.any_member(*item, output, count)?;
                }
                Ok(())
            }
            _ if self.mode == Mode::Lax => Ok(()),
            _ => Err(object_not_found("jsonpath wildcard member accessor")),
        }
    }

    fn descendants(
        &self,
        value: crate::sql::json::Json<'a>,
        depth: u32,
        first: Option<u32>,
        last: Option<u32>,
        output: &mut [crate::sql::json::Json<'a>; MAX_RESULTS],
        count: &mut usize,
    ) -> Result<(), SqlError> {
        use crate::sql::json::Json;
        let first = first.unwrap_or(u32::MAX);
        let last = last.unwrap_or(u32::MAX);
        if depth >= first && depth <= last {
            self.push(output, count, value)?;
        }
        if depth == last || depth >= MAX_DEPTH.into() {
            return Ok(());
        }
        match value {
            Json::Array(items) => {
                for item in items {
                    self.descendants(*item, depth + 1, Some(first), Some(last), output, count)?;
                }
            }
            Json::Object(members) => {
                for (_, item) in members {
                    self.descendants(*item, depth + 1, Some(first), Some(last), output, count)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn method(
        &self,
        value: crate::sql::json::Json<'a>,
        method: Method,
        first: Option<&'a Expr<'a>>,
        second: Option<&'a Expr<'a>>,
        output: &mut [crate::sql::json::Json<'a>; MAX_RESULTS],
        count: &mut usize,
    ) -> Result<(), SqlError> {
        use crate::sql::json::Json;
        match method {
            Method::Type => self.push(
                output,
                count,
                Json::Str(match value {
                    Json::Null => "null",
                    Json::Bool(_) => "boolean",
                    Json::Number(_) => "number",
                    Json::Str(_) => "string",
                    Json::Temporal { .. } => "datetime",
                    Json::Array(_) => "array",
                    Json::Object(_) => "object",
                }),
            ),
            Method::Size => {
                let size = match value {
                    Json::Array(items) => items.len(),
                    _ if self.mode == Mode::Lax => 1,
                    _ => return Err(array_not_found("jsonpath item method .size()")),
                };
                let text = self.integer_text(size as i64)?;
                self.push(output, count, Json::Number(text))
            }
            Method::Abs | Method::Floor | Method::Ceiling => {
                let number = self.number(value)?;
                let number = match method {
                    Method::Abs if number.sign == crate::sql::numeric::Sign::Neg => {
                        crate::sql::numeric::sub(&Numeric::ZERO, &number, self.arena)?
                    }
                    Method::Floor => {
                        number.round_scale(0, crate::sql::numeric::RoundMode::Floor, self.arena)?
                    }
                    Method::Ceiling => {
                        number.round_scale(0, crate::sql::numeric::RoundMode::Ceil, self.arena)?
                    }
                    _ => number,
                };
                let text = self.numeric_text(&number)?;
                self.push(output, count, Json::Number(text))
            }
            Method::Double
            | Method::Bigint
            | Method::Integer
            | Method::Number
            | Method::Decimal => {
                let number = match value {
                    Json::Number(text) | Json::Str(text) => {
                        Numeric::parse(text, self.arena).map_err(|_| method_argument_error())?
                    }
                    _ => return Err(method_argument_error()),
                };
                let precision = match first {
                    Some(precision) if method == Method::Decimal => {
                        Some(self.integer_argument(precision, value)?)
                    }
                    Some(_) => return Err(method_argument_error()),
                    None => None,
                };
                let scale = match second {
                    Some(scale) => Some(self.integer_argument(scale, value)?),
                    None => None,
                };
                let number = match method {
                    Method::Bigint | Method::Integer => {
                        let rounded = number.round_scale(
                            0,
                            crate::sql::numeric::RoundMode::HalfAwayZero,
                            self.arena,
                        )?;
                        let integer = rounded.to_i64().map_err(|_| method_argument_error())?;
                        if method == Method::Integer && i32::try_from(integer).is_err() {
                            return Err(method_argument_error());
                        }
                        Numeric::from_i64(integer, self.arena)?
                    }
                    Method::Decimal => {
                        let precision = precision.unwrap_or(0);
                        if precision == 0 && first.is_some() || !(0..=1000).contains(&precision) {
                            return Err(sql_err!(
                                sqlstate::INVALID_PARAMETER_VALUE,
                                "NUMERIC precision {} must be between 1 and 1000",
                                precision
                            ));
                        }
                        let scale = scale.unwrap_or(0);
                        if !(-1000..=1000).contains(&scale) {
                            return Err(sql_err!(
                                sqlstate::INVALID_PARAMETER_VALUE,
                                "NUMERIC scale {} must be between -1000 and 1000",
                                scale
                            ));
                        }
                        let rounded = self.round_numeric_scale(&number, scale)?;
                        if let Some(precision) = first.map(|_| precision)
                            && !numeric_fits_precision(&rounded, precision, scale)
                        {
                            return Err(method_argument_error());
                        }
                        rounded
                    }
                    Method::Double => {
                        let value = number.to_f64();
                        if !value.is_finite() {
                            return Err(method_argument_error());
                        }
                        let text = crate::stack_format!(64, "{value}");
                        Numeric::parse(text.as_str(), self.arena)
                            .map_err(|_| method_argument_error())?
                    }
                    Method::Number => number,
                    _ => unreachable!(),
                };
                let text = self.numeric_text(&number)?;
                self.push(output, count, Json::Number(text))
            }
            Method::Boolean => {
                let boolean = match value {
                    Json::Bool(value) => value,
                    Json::Number(value) => !Numeric::parse(value, self.arena)
                        .map_err(|_| method_argument_error())?
                        .is_zero(),
                    Json::Str(value)
                        if matches_ignore_ascii_case(value, &["true", "yes", "on", "1", "t"]) =>
                    {
                        true
                    }
                    Json::Str(value)
                        if matches_ignore_ascii_case(value, &["false", "no", "off", "0", "f"]) =>
                    {
                        false
                    }
                    _ => return Err(method_argument_error()),
                };
                self.push(output, count, Json::Bool(boolean))
            }
            Method::String => {
                let text = match value {
                    Json::Str(text) => text,
                    Json::Temporal { text, .. } => text,
                    Json::Bool(true) => "true",
                    Json::Bool(false) => "false",
                    Json::Number(text) => text,
                    Json::Null | Json::Array(_) | Json::Object(_) => {
                        return Err(method_argument_error());
                    }
                };
                self.push(output, count, Json::Str(text))
            }
            Method::KeyValue => {
                let Json::Object(members) = value else {
                    return Err(object_not_found("jsonpath item method .keyvalue()"));
                };
                let object_id = self.object_id(members)?;
                for (key, value) in members {
                    // Every member produced from one object carries the same
                    // object identity.
                    let id = Json::Number(self.integer_text(object_id)?);
                    let object = [("id", id), ("key", Json::Str(key)), ("value", *value)];
                    let object = self
                        .arena
                        .alloc_slice_copy(&object)
                        .map_err(|_| limit_error())?;
                    self.push(output, count, Json::Object(object))?;
                }
                Ok(())
            }
            Method::Date
            | Method::Datetime
            | Method::Time
            | Method::TimeTz
            | Method::Timestamp
            | Method::TimestampTz => {
                let Json::Str(text) = value else {
                    return Err(datetime_method_error());
                };
                if second.is_some() {
                    return Err(datetime_method_error());
                }
                let (template, precision) = match (method, first) {
                    (Method::Datetime, Some(expression)) => {
                        let Json::Str(template) = self.singleton(expression, value, None)? else {
                            return Err(datetime_method_error());
                        };
                        (Some(template), None)
                    }
                    (
                        Method::Time | Method::TimeTz | Method::Timestamp | Method::TimestampTz,
                        Some(expression),
                    ) => {
                        let precision = self.integer_argument(expression, value)?;
                        if !(0..=6).contains(&precision) {
                            return Err(sql_err!(
                                sqlstate::INVALID_PARAMETER_VALUE,
                                "time precision {} must be between 0 and 6",
                                precision
                            ));
                        }
                        (None, Some(precision as u32))
                    }
                    (Method::Date, Some(_)) => return Err(datetime_method_error()),
                    (_, None) => (None, None),
                    _ => return Err(datetime_method_error()),
                };
                let temporal = self.temporal(text, method, template, precision)?;
                self.push(output, count, temporal)
            }
        }
    }

    fn object_id(&self, target: &[(&str, crate::sql::json::Json<'a>)]) -> Result<i64, SqlError> {
        let target = target.as_ptr().cast::<()>();
        if let Some(offset) = jsonb_object_offset(self.root, target, self.arena)? {
            return i64::try_from(offset).map_err(|_| limit_error());
        }
        if let Some(crate::sql::json::Json::Object(variables)) = self.variables {
            for (index, (_, value)) in variables.iter().enumerate() {
                if let Some(offset) = jsonb_object_offset(*value, target, self.arena)? {
                    let base = i64::try_from(index + 1).map_err(|_| limit_error())?;
                    return base
                        .checked_mul(10_000_000_000)
                        .and_then(|base| base.checked_add(offset as i64))
                        .ok_or_else(limit_error);
                }
            }
        }
        // Objects synthesized by path methods become independent base objects
        // in PostgreSQL. They use monotonically assigned base IDs rather than
        // offsets into the original document.
        let base = self.next_generated_object_id.get();
        self.next_generated_object_id
            .set(base.checked_add(1).ok_or_else(limit_error)?);
        base.checked_mul(10_000_000_000).ok_or_else(limit_error)
    }

    fn temporal(
        &self,
        text: &'a str,
        method: Method,
        template: Option<&str>,
        precision: Option<u32>,
    ) -> Result<crate::sql::json::Json<'a>, SqlError> {
        use crate::sql::json::{Json, JsonTemporalKind};
        if template.is_some() && method != Method::Datetime {
            return Err(datetime_method_error());
        }
        if let Some(template) = template {
            let parsed = crate::sql::datetime::parse_formatted(text, template)
                .map_err(|_| datetime_method_error())?;
            let kind = template_datetime_kind(template, parsed.timezone_offset_seconds.is_some());
            let (value, offset) = match kind {
                JsonTemporalKind::Date => (
                    i64::from(
                        crate::sql::datetime::make_date(
                            parsed.year,
                            i64::from(parsed.month),
                            i64::from(parsed.day),
                        )
                        .map_err(|_| datetime_method_error())?,
                    ),
                    0,
                ),
                JsonTemporalKind::Time | JsonTemporalKind::TimeTz => (
                    ((parsed.hour * 60 + parsed.minute) * 60 + parsed.second) * 1_000_000
                        + parsed.microsecond,
                    parsed.timezone_offset_seconds.unwrap_or(0),
                ),
                JsonTemporalKind::Timestamp | JsonTemporalKind::TimestampTz => {
                    let local = crate::sql::datetime::make_timestamp(
                        parsed.year,
                        i64::from(parsed.month),
                        i64::from(parsed.day),
                        parsed.hour,
                        parsed.minute,
                        parsed.second as f64 + parsed.microsecond as f64 / 1_000_000.0,
                    )
                    .map_err(|_| datetime_method_error())?;
                    let offset = parsed.timezone_offset_seconds.unwrap_or(0);
                    (
                        if kind == JsonTemporalKind::TimestampTz {
                            local - i64::from(offset) * 1_000_000
                        } else {
                            local
                        },
                        offset,
                    )
                }
            };
            let canonical = render_temporal(kind, value, offset)?;
            return Ok(Json::Temporal {
                text: self
                    .arena
                    .alloc_str(canonical.as_str())
                    .map_err(|_| limit_error())?,
                kind,
                value,
                offset,
            });
        }
        let effective = if method == Method::Datetime {
            if text.contains(':') {
                if text.contains('T') || text.contains(' ') {
                    if jsonpath_has_timezone(text) {
                        Method::TimestampTz
                    } else {
                        Method::Timestamp
                    }
                } else if jsonpath_has_timezone(text) {
                    Method::TimeTz
                } else {
                    Method::Time
                }
            } else {
                Method::Date
            }
        } else {
            method
        };
        let (kind, mut value, offset) = match effective {
            Method::Date => (
                JsonTemporalKind::Date,
                i64::from(
                    crate::sql::datetime::parse_date(text).map_err(|_| datetime_method_error())?,
                ),
                0,
            ),
            Method::Time => {
                let (value, zone) = crate::sql::datetime::parse_timetz(text)
                    .map_err(|_| datetime_method_error())?;
                if zone.is_some() {
                    return Err(datetime_method_error());
                }
                (JsonTemporalKind::Time, value, 0)
            }
            Method::TimeTz => {
                let (value, zone) = crate::sql::datetime::parse_timetz(text)
                    .map_err(|_| datetime_method_error())?;
                let zone = zone.ok_or_else(datetime_method_error)?;
                (JsonTemporalKind::TimeTz, value, zone)
            }
            Method::Timestamp => {
                if jsonpath_has_timezone(text) {
                    return Err(datetime_method_error());
                }
                (
                    JsonTemporalKind::Timestamp,
                    crate::sql::datetime::parse_timestamp(text, false)
                        .map_err(|_| datetime_method_error())?,
                    0,
                )
            }
            Method::TimestampTz => {
                if !jsonpath_has_timezone(text) {
                    return Err(datetime_method_error());
                }
                let offset = timestamp_timezone_offset(text).ok_or_else(datetime_method_error)?;
                (
                    JsonTemporalKind::TimestampTz,
                    crate::sql::datetime::parse_timestamp(text, true)
                        .map_err(|_| datetime_method_error())?,
                    offset,
                )
            }
            _ => unreachable!(),
        };
        if let Some(precision) = precision {
            value = round_temporal_value(value, precision);
        }
        let rendered = render_temporal(kind, value, offset)?;
        let text = self
            .arena
            .alloc_str(rendered.as_str())
            .map_err(|_| limit_error())?;
        Ok(Json::Temporal {
            text,
            kind,
            value,
            offset,
        })
    }

    fn predicate(
        &self,
        expression: &'a Expr<'a>,
        current: crate::sql::json::Json<'a>,
        last: Option<usize>,
    ) -> Result<Truth, SqlError> {
        let values = self.expression(expression, current, last)?;
        if values.len() != 1 {
            return Ok(Truth::Unknown);
        }
        Ok(match values[0] {
            crate::sql::json::Json::Bool(true) => Truth::True,
            crate::sql::json::Json::Bool(false) => Truth::False,
            _ => Truth::Unknown,
        })
    }

    fn singleton(
        &self,
        expression: &'a Expr<'a>,
        current: crate::sql::json::Json<'a>,
        last: Option<usize>,
    ) -> Result<crate::sql::json::Json<'a>, SqlError> {
        let values = self.expression(expression, current, last)?;
        match values {
            [value] => Ok(*value),
            _ => Err(runtime_error()),
        }
    }

    fn index(
        &self,
        expression: &'a Expr<'a>,
        current: crate::sql::json::Json<'a>,
        last: Option<usize>,
    ) -> Result<usize, SqlError> {
        let value = self.singleton(expression, current, last).map_err(|_| {
            invalid_subscript("jsonpath array subscript is not a single numeric value")
        })?;
        let crate::sql::json::Json::Number(text) = value else {
            return Err(invalid_subscript(
                "jsonpath array subscript is not a single numeric value",
            ));
        };
        let numeric = Numeric::parse(text, self.arena).map_err(|_| {
            invalid_subscript("jsonpath array subscript is not a single numeric value")
        })?;
        let truncated = numeric
            .round_scale(0, crate::sql::numeric::RoundMode::Trunc, self.arena)
            .map_err(|_| invalid_subscript("jsonpath array subscript is out of bounds"))?;
        let integer = truncated
            .to_i64()
            .map_err(|_| invalid_subscript("jsonpath array subscript is out of bounds"))?;
        usize::try_from(integer)
            .map_err(|_| invalid_subscript("jsonpath array subscript is out of bounds"))
    }

    fn integer_argument(
        &self,
        expression: &'a Expr<'a>,
        current: crate::sql::json::Json<'a>,
    ) -> Result<i64, SqlError> {
        let value = self.singleton(expression, current, None)?;
        let crate::sql::json::Json::Number(text) = value else {
            return Err(runtime_error());
        };
        Numeric::parse(text, self.arena)
            .and_then(|number| number.to_i64())
            .map_err(|_| runtime_error())
    }

    fn round_numeric_scale(
        &self,
        number: &Numeric<'_>,
        scale: i64,
    ) -> Result<Numeric<'a>, SqlError> {
        if scale >= 0 {
            return number.round_scale(
                scale as usize,
                crate::sql::numeric::RoundMode::HalfAwayZero,
                self.arena,
            );
        }
        let places = usize::try_from(-scale).map_err(|_| runtime_error())?;
        let rendered = crate::stack_format!(2100, "{number}");
        let (negative, body) = rendered
            .as_str()
            .strip_prefix('-')
            .map_or((false, rendered.as_str()), |body| (true, body));
        let integer = body.split_once('.').map_or(body, |(integer, _)| integer);
        let cut = integer.len().saturating_sub(places);
        let round_up = integer
            .as_bytes()
            .get(cut)
            .is_some_and(|digit| *digit >= b'5');
        let mut digits = [b'0'; 2100];
        if cut > digits.len().saturating_sub(places + 2) {
            return Err(limit_error());
        }
        digits[..cut].copy_from_slice(&integer.as_bytes()[..cut]);
        let mut retained = cut;
        if round_up {
            let mut index = retained;
            while index > 0 && digits[index - 1] == b'9' {
                digits[index - 1] = b'0';
                index -= 1;
            }
            if index == 0 {
                digits.copy_within(0..retained, 1);
                digits[0] = b'1';
                retained += 1;
            } else {
                digits[index - 1] += 1;
            }
        }
        if retained == 0 {
            retained = 1;
        }
        let length = retained + places;
        for digit in &mut digits[retained..length] {
            *digit = b'0';
        }
        let mut output = [0u8; 2102];
        let mut at = 0usize;
        let nonzero = digits[..length].iter().any(|digit| *digit != b'0');
        if negative && nonzero {
            output[at] = b'-';
            at += 1;
        }
        output[at..at + length].copy_from_slice(&digits[..length]);
        at += length;
        let text = core::str::from_utf8(&output[..at]).map_err(|_| runtime_error())?;
        Numeric::parse(text, self.arena)
    }

    fn number(&self, value: crate::sql::json::Json<'a>) -> Result<Numeric<'a>, SqlError> {
        match value {
            crate::sql::json::Json::Number(text) => {
                Numeric::parse(text, self.arena).map_err(|_| method_argument_error())
            }
            _ => Err(method_argument_error()),
        }
    }

    fn numeric_text(&self, number: &Numeric) -> Result<&'a str, SqlError> {
        let text = crate::stack_format!(2100, "{number}");
        if text.is_truncated() {
            return Err(limit_error());
        }
        self.arena
            .alloc_str(text.as_str())
            .map_err(|_| limit_error())
    }

    fn integer_text(&self, value: i64) -> Result<&'a str, SqlError> {
        self.arena
            .alloc_str_display(value)
            .map_err(|_| limit_error())
    }

    fn truth_json(&self, truth: Truth) -> crate::sql::json::Json<'a> {
        match truth {
            Truth::True => crate::sql::json::Json::Bool(true),
            Truth::False => crate::sql::json::Json::Bool(false),
            Truth::Unknown => crate::sql::json::Json::Null,
        }
    }

    fn one(
        &self,
        value: crate::sql::json::Json<'a>,
    ) -> Result<&'a [crate::sql::json::Json<'a>], SqlError> {
        self.results(&[value])
    }

    fn results(
        &self,
        values: &[crate::sql::json::Json<'a>],
    ) -> Result<&'a [crate::sql::json::Json<'a>], SqlError> {
        self.arena
            .alloc_slice_copy(values)
            .map(|values| &*values)
            .map_err(|_| limit_error())
    }

    fn push(
        &self,
        output: &mut [crate::sql::json::Json<'a>; MAX_RESULTS],
        count: &mut usize,
        value: crate::sql::json::Json<'a>,
    ) -> Result<(), SqlError> {
        if *count == output.len() {
            return Err(limit_error());
        }
        output[*count] = value;
        *count += 1;
        Ok(())
    }

    fn extend(
        &self,
        output: &mut [crate::sql::json::Json<'a>; MAX_RESULTS],
        count: &mut usize,
        values: &[crate::sql::json::Json<'a>],
    ) -> Result<(), SqlError> {
        for value in values {
            self.push(output, count, *value)?;
        }
        Ok(())
    }
}

fn compare_json(
    left: &crate::sql::json::Json,
    right: &crate::sql::json::Json,
    use_timezone: bool,
) -> Result<Option<core::cmp::Ordering>, SqlError> {
    use crate::sql::json::Json;
    Ok(match (left, right) {
        (Json::Null, _) | (_, Json::Null) => None,
        (Json::Bool(left), Json::Bool(right)) => Some(left.cmp(right)),
        (Json::Number(left), Json::Number(right)) => {
            crate::sql::numeric::cmp_decimal_str(left, right)
        }
        (Json::Str(left), Json::Str(right)) => Some(left.cmp(right)),
        (
            Json::Temporal {
                kind: left_kind,
                value: left,
                offset: left_offset,
                ..
            },
            Json::Temporal {
                kind: right_kind,
                value: right,
                offset: right_offset,
                ..
            },
        ) if left_kind == right_kind => {
            let normalize = |kind: crate::sql::json::JsonTemporalKind, value: i64, offset: i32| {
                if kind == crate::sql::json::JsonTemporalKind::TimeTz {
                    value - i64::from(offset) * 1_000_000
                } else {
                    value
                }
            };
            let ordering = normalize(*left_kind, *left, *left_offset).cmp(&normalize(
                *right_kind,
                *right,
                *right_offset,
            ));
            Some(
                if ordering.is_eq() && *left_kind == crate::sql::json::JsonTemporalKind::TimeTz {
                    right_offset.cmp(left_offset)
                } else {
                    ordering
                },
            )
        }
        (
            Json::Temporal {
                kind: left_kind,
                value: left,
                offset: left_offset,
                ..
            },
            Json::Temporal {
                kind: right_kind,
                value: right,
                offset: right_offset,
                ..
            },
        ) => compare_temporal_cross(
            *left_kind,
            *left,
            *left_offset,
            *right_kind,
            *right,
            *right_offset,
            use_timezone,
        )?,
        _ => None,
    })
}

fn compare_temporal_cross(
    left_kind: crate::sql::json::JsonTemporalKind,
    left: i64,
    left_offset: i32,
    right_kind: crate::sql::json::JsonTemporalKind,
    right: i64,
    right_offset: i32,
    use_timezone: bool,
) -> Result<Option<core::cmp::Ordering>, SqlError> {
    use crate::sql::json::JsonTemporalKind;
    const DAY_MICROSECONDS: i64 = 86_400_000_000;
    let date_family = |kind| {
        matches!(
            kind,
            JsonTemporalKind::Date | JsonTemporalKind::Timestamp | JsonTemporalKind::TimestampTz
        )
    };
    if date_family(left_kind) && date_family(right_kind) {
        let needs_timezone = left_kind == JsonTemporalKind::TimestampTz
            || right_kind == JsonTemporalKind::TimestampTz;
        if needs_timezone && !use_timezone {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "cannot compare timestamp values without time zone usage"
            ));
        }
        let normalize = |kind, value: i64| {
            let local = if kind == JsonTemporalKind::Date {
                value * DAY_MICROSECONDS
            } else {
                value
            };
            if needs_timezone && kind != JsonTemporalKind::TimestampTz {
                let offset = crate::sql::timezone::session().resolve(local).0;
                local - i64::from(offset) * 1_000_000
            } else {
                local
            }
        };
        return Ok(Some(
            normalize(left_kind, left).cmp(&normalize(right_kind, right)),
        ));
    }
    let time_family = |kind| matches!(kind, JsonTemporalKind::Time | JsonTemporalKind::TimeTz);
    if time_family(left_kind) && time_family(right_kind) {
        if !use_timezone {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "cannot compare time values without time zone usage"
            ));
        }
        let session_offset = crate::sql::timezone::session()
            .resolve(crate::sql::datetime::now_micros())
            .0;
        let normalize = |kind, value: i64, offset: i32| {
            let offset = if kind == JsonTemporalKind::TimeTz {
                offset
            } else {
                session_offset
            };
            (value - i64::from(offset) * 1_000_000, offset)
        };
        let left = normalize(left_kind, left, left_offset);
        let right = normalize(right_kind, right, right_offset);
        return Ok(Some(
            left.0.cmp(&right.0).then_with(|| right.1.cmp(&left.1)),
        ));
    }
    Ok(None)
}

fn round_temporal_value(value: i64, precision: u32) -> i64 {
    let unit = 10_i64.pow(6 - precision);
    if unit == 1 {
        return value;
    }
    let half = unit / 2;
    if value >= 0 {
        value.saturating_add(half) / unit * unit
    } else {
        value.saturating_sub(half) / unit * unit
    }
}

fn jsonb_object_offset(
    root: crate::sql::json::Json<'_>,
    target: *const (),
    arena: &Arena,
) -> Result<Option<usize>, SqlError> {
    fn align4(value: usize) -> Result<usize, SqlError> {
        value
            .checked_add(3)
            .map(|value| value & !3)
            .ok_or_else(limit_error)
    }
    fn scalar_size(
        value: crate::sql::json::Json<'_>,
        length: &mut usize,
        arena: &Arena,
    ) -> Result<(), SqlError> {
        use crate::sql::json::Json;
        match value {
            Json::Null | Json::Bool(_) => {}
            Json::Str(text) | Json::Temporal { text, .. } => {
                *length = length.checked_add(text.len()).ok_or_else(limit_error)?;
            }
            Json::Number(text) => {
                *length = align4(*length)?;
                let number = Numeric::parse(text, arena)?;
                let short = number.dscale <= 63 && (-64..=63).contains(&number.weight);
                let header = if short { 6 } else { 8 };
                *length = length
                    .checked_add(header + number.ndigits() * 2)
                    .ok_or_else(limit_error)?;
            }
            Json::Array(_) | Json::Object(_) => unreachable!("container is not a scalar"),
        }
        Ok(())
    }
    fn serialize(
        value: crate::sql::json::Json<'_>,
        target: *const (),
        root_start: usize,
        length: &mut usize,
        found: &mut Option<usize>,
        arena: &Arena,
    ) -> Result<(), SqlError> {
        use crate::sql::json::Json;
        match value {
            Json::Array(items) => {
                *length = align4(*length)?;
                *length = length
                    .checked_add(4 + items.len() * 4)
                    .ok_or_else(limit_error)?;
                for item in items {
                    match item {
                        Json::Array(_) | Json::Object(_) => {
                            serialize(*item, target, root_start, length, found, arena)?;
                        }
                        _ => scalar_size(*item, length, arena)?,
                    }
                }
            }
            Json::Object(members) => {
                *length = align4(*length)?;
                let object_start = *length;
                if members.as_ptr().cast::<()>() == target {
                    *found = Some(object_start - root_start);
                }
                *length = length
                    .checked_add(4 + members.len() * 8)
                    .ok_or_else(limit_error)?;
                for (key, _) in members {
                    *length = length.checked_add(key.len()).ok_or_else(limit_error)?;
                }
                for (_, item) in members {
                    match item {
                        Json::Array(_) | Json::Object(_) => {
                            serialize(*item, target, root_start, length, found, arena)?;
                        }
                        _ => scalar_size(*item, length, arena)?,
                    }
                }
            }
            scalar => scalar_size(scalar, length, arena)?,
        }
        Ok(())
    }

    let mut length = 4usize; // varlena header precedes the root container
    let root_start = align4(length)?;
    let mut found = None;
    serialize(root, target, root_start, &mut length, &mut found, arena)?;
    Ok(found)
}

fn render_temporal(
    kind: crate::sql::json::JsonTemporalKind,
    value: i64,
    offset: i32,
) -> Result<crate::util::StackStr<64>, SqlError> {
    use crate::sql::json::JsonTemporalKind;
    let mut output = crate::util::StackStr::<64>::new();
    match kind {
        JsonTemporalKind::Date => output
            .write_str(crate::sql::datetime::format_date(value as i32).as_str())
            .map_err(|_| limit_error())?,
        JsonTemporalKind::Time | JsonTemporalKind::TimeTz => output
            .write_str(crate::sql::datetime::format_time(value).as_str())
            .map_err(|_| limit_error())?,
        JsonTemporalKind::Timestamp => output
            .write_str(crate::sql::datetime::format_timestamp_json(value, false).as_str())
            .map_err(|_| limit_error())?,
        JsonTemporalKind::TimestampTz => output
            .write_str(
                crate::sql::datetime::format_timestamp_json(
                    value + i64::from(offset) * 1_000_000,
                    false,
                )
                .as_str(),
            )
            .map_err(|_| limit_error())?,
    }
    if matches!(
        kind,
        JsonTemporalKind::TimeTz | JsonTemporalKind::TimestampTz
    ) {
        let absolute = offset.unsigned_abs();
        write!(
            output,
            "{}{:02}:{:02}",
            if offset < 0 { '-' } else { '+' },
            absolute / 3600,
            absolute / 60 % 60
        )
        .map_err(|_| limit_error())?;
    }
    if output.is_truncated() {
        Err(limit_error())
    } else {
        Ok(output)
    }
}

fn contains_ascii_case_insensitive(haystack: &str, needle: &str) -> bool {
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn template_datetime_kind(
    template: &str,
    has_timezone: bool,
) -> crate::sql::json::JsonTemporalKind {
    use crate::sql::json::JsonTemporalKind;
    let has_date = [
        "YYYY", "IYYY", "YYY", "YY", "MONTH", "MON", "MM", "DDD", "DD", "J",
    ]
    .iter()
    .any(|token| contains_ascii_case_insensitive(template, token));
    let has_time = ["HH", "MI", "SS", "MS", "US"]
        .iter()
        .any(|token| contains_ascii_case_insensitive(template, token));
    match (has_date, has_time, has_timezone) {
        (true, true, true) => JsonTemporalKind::TimestampTz,
        (true, true, false) => JsonTemporalKind::Timestamp,
        (true, false, _) => JsonTemporalKind::Date,
        (false, _, true) => JsonTemporalKind::TimeTz,
        (false, _, false) => JsonTemporalKind::Time,
    }
}

fn timestamp_timezone_offset(text: &str) -> Option<i32> {
    let (_, time) = text.split_once('T').or_else(|| text.split_once(' '))?;
    crate::sql::datetime::parse_timetz(time).ok()?.1
}

fn jsonpath_has_timezone(text: &str) -> bool {
    let time = text
        .split_once('T')
        .or_else(|| text.split_once(' '))
        .map_or(text, |(_, time)| time);
    time.ends_with('Z')
        || time.ends_with('z')
        || time
            .char_indices()
            .skip(1)
            .any(|(_, character)| matches!(character, '+' | '-'))
}

fn matches_ignore_ascii_case(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

fn numeric_fits_precision(number: &Numeric<'_>, precision: i64, scale: i64) -> bool {
    let rendered = crate::stack_format!(2100, "{number}");
    let body = rendered
        .as_str()
        .strip_prefix('-')
        .unwrap_or(rendered.as_str());
    let (integer, fraction) = body.split_once('.').unwrap_or((body, ""));
    let integer = integer.trim_start_matches('0');
    let leading_position = if !integer.is_empty() {
        integer.len() as i64
    } else if let Some(first) = fraction.bytes().position(|digit| digit != b'0') {
        -(first as i64)
    } else {
        return true;
    };
    leading_position <= precision - scale
}

fn method_argument_error() -> SqlError {
    sql_err!(
        sqlstate::NON_NUMERIC_SQL_JSON_ITEM,
        "argument of jsonpath item method is invalid for target type"
    )
}

fn datetime_method_error() -> SqlError {
    sql_err!(
        sqlstate::INVALID_ARGUMENT_FOR_SQL_JSON_DATETIME_FUNCTION,
        "argument of jsonpath datetime method is invalid"
    )
}

fn invalid_subscript(message: &str) -> SqlError {
    sql_err!(sqlstate::INVALID_SQL_JSON_SUBSCRIPT, "{}", message)
}

fn array_not_found(accessor: &str) -> SqlError {
    sql_err!(
        sqlstate::SQL_JSON_ARRAY_NOT_FOUND,
        "{} can only be applied to an array",
        accessor
    )
}

fn object_not_found(accessor: &str) -> SqlError {
    sql_err!(
        sqlstate::SQL_JSON_OBJECT_NOT_FOUND,
        "{} can only be applied to an object",
        accessor
    )
}

fn runtime_error() -> SqlError {
    sql_err!(sqlstate::DATA_EXCEPTION, "JSON path evaluation failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_postgresql_examples() {
        let mut budget = crate::mem::Budget::new(256 * 1024);
        let mut arena = Arena::new(&mut budget, "jsonpath test", 256 * 1024).unwrap();
        for (input, expected) in [
            ("$", "$"),
            ("strict $", "strict $"),
            ("lax $.a[0]", "$.\"a\"[0]"),
            ("$[0, 2 to last]", "$[0,2 to last]"),
            ("$.**{1 to 3}", "$.**{1 to 3}"),
            ("$.a ? (@ > 1)", "$.\"a\"?(@ > 1)"),
            ("$ ? (exists (@.a))", "$?(exists (@.\"a\"))"),
            ("$var", "$\"var\""),
            ("1 + 2 * 3", "(1 + 2 * 3)"),
        ] {
            arena.reset();
            assert_eq!(canonicalize(input, &arena).unwrap(), expected, "{input}");
        }
    }

    #[test]
    fn rejects_invalid_paths() {
        let mut budget = crate::mem::Budget::new(64 * 1024);
        let mut arena = Arena::new(&mut budget, "jsonpath test", 64 * 1024).unwrap();
        for input in ["", "$.", "$[", "$ ? (@ = 1)", "$.unknown()", "$.**{3 to 1}"] {
            arena.reset();
            assert!(canonicalize(input, &arena).is_err(), "{input}");
        }
    }

    #[test]
    fn datetime_methods_canonicalize_and_round() {
        let mut budget = crate::mem::Budget::new(512 * 1024);
        let arena = Arena::new(&mut budget, "jsonpath datetime test", 512 * 1024).unwrap();
        for (target, path, expected) in [
            ("\"12:34:56.789\"", "$.time(2)", "\"12:34:56.79\""),
            (
                "\"12:34:56.789 +05:30\"",
                "$.time_tz(2)",
                "\"12:34:56.79+05:30\"",
            ),
            (
                "\"2023-08-15 12:34:56.789\"",
                "$.timestamp(2)",
                "\"2023-08-15T12:34:56.79\"",
            ),
            (
                "\"2023-08-15 12:34:56.789 +05:30\"",
                "$.timestamp_tz(2)",
                "\"2023-08-15T12:34:56.79+05:30\"",
            ),
        ] {
            let values = query(target, path, None, false, &arena)
                .unwrap_or_else(|error| panic!("{path}: {} {}", error.sqlstate, error.message));
            assert_eq!(
                crate::stack_format!(128, "{}", crate::sql::json::JsonWrite(&values[0])).as_str(),
                expected
            );
        }
    }
}
