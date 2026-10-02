//! A minimal JSON value, parser and writer — just enough for JSON-RPC/LSP
//! traffic (objects, arrays, strings with the standard escapes including
//! `\uXXXX` surrogate pairs, numbers, `true`/`false`/`null`).
//!
//! The workspace takes no external crates, so this small hand-written module
//! stands in for `serde_json`.

/// A JSON value.
///
/// Objects keep insertion order in a `Vec<(String, Json)>` rather than a
/// map — JSON-RPC objects are small and read positionally (`get`), so a map
/// buys nothing but a `Hash` bound we don't need.
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    #[must_use]
    pub fn str(s: impl Into<String>) -> Self {
        Json::String(s.into())
    }

    #[must_use]
    pub fn num(n: impl Into<f64>) -> Self {
        Json::Number(n.into())
    }

    #[must_use]
    pub fn object(pairs: Vec<(String, Json)>) -> Self {
        Json::Object(pairs)
    }

    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(s) => Some(s),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Number(n) => Some(*n),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(a) => Some(a),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_object(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Object(o) => Some(o),
            _ => None,
        }
    }

    /// Look up a key in an object. `None` if this isn't an object or the
    /// key is absent.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Json> {
        self.as_object()?
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonError(pub String);

impl std::fmt::Display for JsonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "json: {}", self.0)
    }
}

/// Parse a complete JSON document. Trailing whitespace after the value is
/// allowed; trailing garbage is not.
pub fn parse(input: &str) -> Result<Json, JsonError> {
    let mut p = Parser {
        chars: input.chars().peekable(),
    };
    p.skip_ws();
    let v = p.value()?;
    p.skip_ws();
    if p.chars.peek().is_some() {
        return Err(JsonError("trailing data after JSON value".into()));
    }
    Ok(v)
}

struct Parser<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while matches!(self.chars.peek(), Some(' ' | '\t' | '\n' | '\r')) {
            self.chars.next();
        }
    }

    fn bump(&mut self) -> Result<char, JsonError> {
        self.chars
            .next()
            .ok_or_else(|| JsonError("unexpected end of input".into()))
    }

    fn expect(&mut self, c: char) -> Result<(), JsonError> {
        match self.bump()? {
            x if x == c => Ok(()),
            x => Err(JsonError(format!("expected {c:?}, found {x:?}"))),
        }
    }

    fn expect_literal(&mut self, lit: &str, value: Json) -> Result<Json, JsonError> {
        for want in lit.chars() {
            self.expect(want)?;
        }
        Ok(value)
    }

    fn value(&mut self) -> Result<Json, JsonError> {
        self.skip_ws();
        match self.chars.peek() {
            Some('"') => self.string().map(Json::String),
            Some('{') => self.object(),
            Some('[') => self.array(),
            Some('t') => self.expect_literal("true", Json::Bool(true)),
            Some('f') => self.expect_literal("false", Json::Bool(false)),
            Some('n') => self.expect_literal("null", Json::Null),
            Some(c) if c.is_ascii_digit() || *c == '-' => self.number(),
            Some(c) => Err(JsonError(format!("unexpected character {c:?}"))),
            None => Err(JsonError("unexpected end of input".into())),
        }
    }

    fn object(&mut self) -> Result<Json, JsonError> {
        self.expect('{')?;
        let mut pairs = Vec::new();
        self.skip_ws();
        if self.chars.peek() == Some(&'}') {
            self.chars.next();
            return Ok(Json::Object(pairs));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            self.expect(':')?;
            let val = self.value()?;
            pairs.push((key, val));
            self.skip_ws();
            match self.bump()? {
                ',' => continue,
                '}' => break,
                c => return Err(JsonError(format!("expected ',' or '}}', found {c:?}"))),
            }
        }
        Ok(Json::Object(pairs))
    }

    fn array(&mut self) -> Result<Json, JsonError> {
        self.expect('[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.chars.peek() == Some(&']') {
            self.chars.next();
            return Ok(Json::Array(items));
        }
        loop {
            let v = self.value()?;
            items.push(v);
            self.skip_ws();
            match self.bump()? {
                ',' => continue,
                ']' => break,
                c => return Err(JsonError(format!("expected ',' or ']', found {c:?}"))),
            }
        }
        Ok(Json::Array(items))
    }

    fn string(&mut self) -> Result<String, JsonError> {
        self.expect('"')?;
        let mut out = String::new();
        loop {
            let c = self.bump()?;
            match c {
                '"' => break,
                '\\' => {
                    let esc = self.bump()?;
                    match esc {
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        '/' => out.push('/'),
                        'b' => out.push('\u{08}'),
                        'f' => out.push('\u{0C}'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'u' => {
                            let hi = self.hex4()?;
                            if (0xD800..=0xDBFF).contains(&hi) {
                                // High surrogate: must be followed by a low
                                // surrogate to form one astral code point.
                                if self.bump()? != '\\' || self.bump()? != 'u' {
                                    return Err(JsonError("unpaired UTF-16 surrogate".into()));
                                }
                                let lo = self.hex4()?;
                                if !(0xDC00..=0xDFFF).contains(&lo) {
                                    return Err(JsonError("invalid low surrogate".into()));
                                }
                                let cp = 0x10000
                                    + (u32::from(hi) - 0xD800) * 0x400
                                    + (u32::from(lo) - 0xDC00);
                                out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                            } else {
                                out.push(char::from_u32(u32::from(hi)).unwrap_or('\u{FFFD}'));
                            }
                        }
                        other => {
                            return Err(JsonError(format!("bad escape \\{other}")));
                        }
                    }
                }
                c => out.push(c),
            }
        }
        Ok(out)
    }

    fn hex4(&mut self) -> Result<u16, JsonError> {
        let mut v: u16 = 0;
        for _ in 0..4 {
            let c = self.bump()?;
            let d = c
                .to_digit(16)
                .ok_or_else(|| JsonError(format!("bad hex digit {c:?}")))?;
            v = v * 16 + d as u16;
        }
        Ok(v)
    }

    fn number(&mut self) -> Result<Json, JsonError> {
        let mut s = String::new();
        if self.chars.peek() == Some(&'-') {
            s.push(self.bump()?);
        }
        while matches!(self.chars.peek(), Some(c) if c.is_ascii_digit()) {
            s.push(self.bump()?);
        }
        if self.chars.peek() == Some(&'.') {
            s.push(self.bump()?);
            while matches!(self.chars.peek(), Some(c) if c.is_ascii_digit()) {
                s.push(self.bump()?);
            }
        }
        if matches!(self.chars.peek(), Some('e' | 'E')) {
            s.push(self.bump()?);
            if matches!(self.chars.peek(), Some('+' | '-')) {
                s.push(self.bump()?);
            }
            while matches!(self.chars.peek(), Some(c) if c.is_ascii_digit()) {
                s.push(self.bump()?);
            }
        }
        s.parse::<f64>()
            .map(Json::Number)
            .map_err(|e| JsonError(format!("bad number {s:?}: {e}")))
    }
}

/// Serialise a JSON value. Strings are escaped on the way out: `"`, `\`,
/// and control characters (`\n`, `\r`, `\t`, `\u{08}`, `\u{0C}`, and any
/// other byte below `0x20` as `\u00XX`) — enough for a SQL snippet in a
/// diagnostic message to round-trip safely.
#[must_use]
pub fn to_string(v: &Json) -> String {
    let mut out = String::new();
    write_value(v, &mut out);
    out
}

fn write_value(v: &Json, out: &mut String) {
    match v {
        Json::Null => out.push_str("null"),
        Json::Bool(true) => out.push_str("true"),
        Json::Bool(false) => out.push_str("false"),
        Json::Number(n) => write_number(*n, out),
        Json::String(s) => write_string(s, out),
        Json::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Json::Object(pairs) => {
            out.push('{');
            for (i, (k, val)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(k, out);
                out.push(':');
                write_value(val, out);
            }
            out.push('}');
        }
    }
}

fn write_number(n: f64, out: &mut String) {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 1e15 {
        out.push_str(&(n as i64).to_string());
    } else {
        out.push_str(&n.to_string());
    }
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_scalars() {
        assert_eq!(parse("null").unwrap(), Json::Null);
        assert_eq!(parse("true").unwrap(), Json::Bool(true));
        assert_eq!(parse("false").unwrap(), Json::Bool(false));
        assert_eq!(parse("42").unwrap(), Json::Number(42.0));
        assert_eq!(parse("-3.5").unwrap(), Json::Number(-3.5));
        assert_eq!(parse("1e3").unwrap(), Json::Number(1000.0));
    }

    #[test]
    fn round_trips_nested_object_and_array() {
        let src = r#"{"a":1,"b":[1,2,3],"c":{"d":null,"e":false}}"#;
        let v = parse(src).unwrap();
        assert_eq!(v.get("a").unwrap().as_f64(), Some(1.0));
        let arr = v.get("b").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(v.get("c").unwrap().get("d").unwrap(), &Json::Null);
        assert_eq!(v.get("c").unwrap().get("e").unwrap(), &Json::Bool(false));
        // Round-trip through the writer and re-parse to the same value.
        let out = to_string(&v);
        assert_eq!(parse(&out).unwrap(), v);
    }

    #[test]
    fn parses_all_standard_escapes() {
        let src = r#""\"\\\/\b\f\n\r\t""#;
        let s = parse(src).unwrap();
        assert_eq!(s, Json::String("\"\\/\u{08}\u{0C}\n\r\t".to_string()));
    }

    #[test]
    fn parses_unicode_escape() {
        assert_eq!(parse(r#""é""#).unwrap(), Json::String("é".to_string()));
    }

    #[test]
    fn parses_surrogate_pair() {
        // U+1D11E MUSICAL SYMBOL G CLEF, encoded as a UTF-16 surrogate pair.
        let s = parse(r#""𝄞""#).unwrap();
        assert_eq!(s, Json::String("\u{1D11E}".to_string()));
    }

    #[test]
    fn writer_escapes_quotes_and_backslashes() {
        let v = Json::String("she said \"go\\here\"".to_string());
        let out = to_string(&v);
        assert_eq!(out, r#""she said \"go\\here\"""#);
        assert_eq!(parse(&out).unwrap(), v);
    }

    #[test]
    fn writer_escapes_control_characters() {
        let v = Json::String("a\nb\tc\rd".to_string());
        let out = to_string(&v);
        assert_eq!(parse(&out).unwrap(), v);
        assert!(out.contains("\\n"));
        assert!(out.contains("\\t"));
        assert!(out.contains("\\r"));
    }

    #[test]
    fn writer_renders_integers_without_decimal_point() {
        assert_eq!(to_string(&Json::Number(3.0)), "3");
        assert_eq!(to_string(&Json::Number(-1.0)), "-1");
        assert_eq!(to_string(&Json::Number(0.0)), "0");
    }

    #[test]
    fn rejects_trailing_garbage() {
        assert!(parse("42 43").is_err());
    }

    #[test]
    fn rejects_truncated_input() {
        assert!(parse(r#"{"a":"#).is_err());
    }

    #[test]
    fn object_get_is_positional_lookup() {
        let v = parse(r#"{"x":1,"y":2}"#).unwrap();
        assert_eq!(v.get("y").unwrap().as_f64(), Some(2.0));
        assert!(v.get("z").is_none());
    }
}
