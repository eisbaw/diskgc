//! A minimal JSON writer for the report: values in, text out. Hand-written
//! rather than a dependency (no reviewed production crate writes JSON, and
//! the report needs only this): objects keep their field order, strings
//! are escaped as RFC 8259 requires, numbers are integers. (No raw string
//! literals here, tests included: the source scan cannot read them.)

use std::fmt::Write as _;

/// A JSON value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Json {
    Null,
    Bool(bool),
    U64(u64),
    I64(i64),
    Str(String),
    Arr(Vec<Json>),
    /// Fields in the order written.
    Obj(Vec<(&'static str, Json)>),
}

impl Json {
    /// `Some(v)` as `f(v)`, `None` as `null`.
    pub fn opt<T>(value: Option<T>, f: impl FnOnce(T) -> Json) -> Json {
        value.map_or(Json::Null, f)
    }

    pub fn str(text: impl Into<String>) -> Json {
        Json::Str(text.into())
    }

    /// The value as compact JSON text.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::U64(n) => {
                let _ = write!(out, "{n}");
            }
            Json::I64(n) => {
                let _ = write!(out, "{n}");
            }
            Json::Str(s) => escape(s, out),
            Json::Arr(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Json::Obj(fields) => {
                out.push('{');
                for (i, (key, value)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    escape(key, out);
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
        }
    }
}

/// `s` as a JSON string: `"` and `\` escaped, every control character
/// below U+0020 as an escape; everything else as it is (the text is
/// UTF-8).
fn escape(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
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
    fn values_are_written_compactly_in_field_order() {
        let v = Json::Obj(vec![
            ("b", Json::U64(1)),
            (
                "a",
                Json::Arr(vec![Json::Null, Json::Bool(true), Json::I64(-3)]),
            ),
            ("s", Json::str("x")),
            ("e", Json::Arr(vec![])),
            ("o", Json::Obj(vec![])),
        ]);
        assert_eq!(
            v.to_text(),
            "{\"b\":1,\"a\":[null,true,-3],\"s\":\"x\",\"e\":[],\"o\":{}}"
        );
    }

    #[test]
    fn strings_are_escaped_and_parse_back() {
        let nasty = "q\"b\\s/\n\r\t\u{0}\u{1f}\u{7f}é😀\u{2028}";
        let text = Json::str(nasty).to_text();
        assert_eq!(
            text,
            "\"q\\\"b\\\\s/\\n\\r\\t\\u0000\\u001f\u{7f}é😀\u{2028}\""
        );
        let back: String = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(back, nasty);
    }
}
