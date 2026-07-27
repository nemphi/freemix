use std::collections::BTreeMap;

use super::DecodeError;

pub(super) const MAX_JSON_DEPTH: usize = 64;

#[derive(Debug)]
pub(super) enum Value {
    Null,
    Bool(bool),
    Number(u128),
    NegativeNumber(u128),
    String(String),
    Array(Vec<Self>),
    Object(BTreeMap<String, Self>),
}

pub(super) struct Reader<'a> {
    source: &'a str,
    offset: usize,
}

impl<'a> Reader<'a> {
    pub(super) const fn new(source: &'a str) -> Self {
        Self { source, offset: 0 }
    }

    pub(super) fn document(mut self) -> Result<Value, DecodeError> {
        self.whitespace();
        let value = self.value(0)?;
        self.whitespace();
        if self.offset != self.source.len() {
            return Err(self.syntax("trailing content"));
        }
        Ok(value)
    }

    fn value(&mut self, depth: usize) -> Result<Value, DecodeError> {
        if depth > MAX_JSON_DEPTH {
            return Err(self.syntax("JSON nesting limit exceeded"));
        }
        match self.source.as_bytes().get(self.offset) {
            Some(b'{') => self.object(depth + 1),
            Some(b'[') => self.array(depth + 1),
            Some(b'"') => self.string().map(Value::String),
            Some(b't') if self.consume_literal("true") => Ok(Value::Bool(true)),
            Some(b'f') if self.consume_literal("false") => Ok(Value::Bool(false)),
            Some(b'n') if self.consume_literal("null") => Ok(Value::Null),
            Some(byte) if byte.is_ascii_digit() => self.number().map(Value::Number),
            Some(b'-') => {
                self.offset += 1;
                self.number().map(Value::NegativeNumber)
            }
            _ => Err(self.syntax("expected JSON value")),
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value, DecodeError> {
        self.expect_byte(b'{')?;
        let mut values = BTreeMap::new();
        self.whitespace();
        if self.consume_byte(b'}') {
            return Ok(Value::Object(values));
        }
        loop {
            let key = self.string()?;
            self.whitespace();
            self.expect_byte(b':')?;
            self.whitespace();
            let value = self.value(depth)?;
            if values.insert(key.clone(), value).is_some() {
                return Err(self.syntax(format!("duplicate field `{key}`")));
            }
            self.whitespace();
            if self.consume_byte(b'}') {
                break;
            }
            self.expect_byte(b',')?;
            self.whitespace();
            if self.next_is(b'}') {
                return Err(self.syntax("trailing comma in object"));
            }
        }
        Ok(Value::Object(values))
    }

    fn array(&mut self, depth: usize) -> Result<Value, DecodeError> {
        self.expect_byte(b'[')?;
        let mut values = Vec::new();
        self.whitespace();
        if self.consume_byte(b']') {
            return Ok(Value::Array(values));
        }
        loop {
            values.push(self.value(depth)?);
            self.whitespace();
            if self.consume_byte(b']') {
                break;
            }
            self.expect_byte(b',')?;
            self.whitespace();
            if self.next_is(b']') {
                return Err(self.syntax("trailing comma in array"));
            }
        }
        Ok(Value::Array(values))
    }

    fn number(&mut self) -> Result<u128, DecodeError> {
        let bytes = self.source.as_bytes();
        let start = self.offset;
        if bytes.get(self.offset) == Some(&b'0') {
            self.offset += 1;
            if bytes.get(self.offset).is_some_and(u8::is_ascii_digit) {
                return Err(self.syntax("leading zero in number"));
            }
            return Ok(0);
        }
        while bytes.get(self.offset).is_some_and(u8::is_ascii_digit) {
            self.offset += 1;
        }
        if self.offset == start {
            return Err(self.syntax("expected unsigned integer"));
        }
        self.source[start..self.offset]
            .parse()
            .map_err(|_| self.syntax("unsigned integer overflow"))
    }

    fn string(&mut self) -> Result<String, DecodeError> {
        self.expect_byte(b'"')?;
        let mut output = String::new();
        loop {
            let Some(byte) = self.source.as_bytes().get(self.offset).copied() else {
                return Err(self.syntax("unterminated string"));
            };
            match byte {
                b'"' => {
                    self.offset += 1;
                    return Ok(output);
                }
                b'\\' => {
                    self.offset += 1;
                    self.escape(&mut output)?;
                }
                0..=0x1f => return Err(self.syntax("unescaped control character")),
                _ => {
                    let character = self
                        .remaining()
                        .chars()
                        .next()
                        .ok_or_else(|| self.syntax("unterminated string"))?;
                    output.push(character);
                    self.offset += character.len_utf8();
                }
            }
        }
    }

    fn escape(&mut self, output: &mut String) -> Result<(), DecodeError> {
        let Some(escaped) = self.source.as_bytes().get(self.offset).copied() else {
            return Err(self.syntax("unterminated escape"));
        };
        self.offset += 1;
        match escaped {
            b'"' => output.push('"'),
            b'\\' => output.push('\\'),
            b'/' => output.push('/'),
            b'b' => output.push('\u{08}'),
            b'f' => output.push('\u{0c}'),
            b'n' => output.push('\n'),
            b'r' => output.push('\r'),
            b't' => output.push('\t'),
            b'u' => self.unicode_escape(output)?,
            _ => return Err(self.syntax("invalid string escape")),
        }
        Ok(())
    }

    fn unicode_escape(&mut self, output: &mut String) -> Result<(), DecodeError> {
        let first = self.hex_quad()?;
        let scalar = if (0xd800..=0xdbff).contains(&first) {
            if !self.remaining().starts_with("\\u") {
                return Err(self.syntax("high surrogate without low surrogate"));
            }
            self.offset += 2;
            let second = self.hex_quad()?;
            if !(0xdc00..=0xdfff).contains(&second) {
                return Err(self.syntax("invalid low surrogate"));
            }
            0x1_0000 + ((u32::from(first) - 0xd800) << 10) + (u32::from(second) - 0xdc00)
        } else if (0xdc00..=0xdfff).contains(&first) {
            return Err(self.syntax("unexpected low surrogate"));
        } else {
            u32::from(first)
        };
        output.push(char::from_u32(scalar).ok_or_else(|| self.syntax("invalid Unicode scalar"))?);
        Ok(())
    }

    fn hex_quad(&mut self) -> Result<u16, DecodeError> {
        let end = self.offset.saturating_add(4);
        let Some(value) = self.source.get(self.offset..end) else {
            return Err(self.syntax("truncated Unicode escape"));
        };
        if !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(self.syntax("invalid Unicode escape"));
        }
        self.offset = end;
        u16::from_str_radix(value, 16).map_err(|_| self.syntax("invalid Unicode escape"))
    }

    fn whitespace(&mut self) {
        while self
            .source
            .as_bytes()
            .get(self.offset)
            .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        {
            self.offset += 1;
        }
    }

    fn expect_byte(&mut self, expected: u8) -> Result<(), DecodeError> {
        if self.consume_byte(expected) {
            Ok(())
        } else {
            Err(self.syntax(format!("expected `{}`", char::from(expected))))
        }
    }

    fn consume_byte(&mut self, expected: u8) -> bool {
        if self.next_is(expected) {
            self.offset += 1;
            true
        } else {
            false
        }
    }

    fn next_is(&self, expected: u8) -> bool {
        self.source.as_bytes().get(self.offset) == Some(&expected)
    }

    fn consume_literal(&mut self, literal: &str) -> bool {
        if self.remaining().starts_with(literal) {
            self.offset += literal.len();
            true
        } else {
            false
        }
    }

    fn remaining(&self) -> &str {
        &self.source[self.offset..]
    }

    fn syntax(&self, message: impl Into<String>) -> DecodeError {
        DecodeError::Syntax {
            offset: self.offset,
            message: message.into(),
        }
    }
}
