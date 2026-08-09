//! A minimal streaming JSON writer.
//!
//! Requests are built by hand rather than through serde: the whole body is
//! written once, straight out of the transcript arena, with no intermediate
//! value tree and no per-message allocation. That matters because the full
//! history is re-encoded on every turn, which is the largest repeated cost in
//! an agent loop.

use std::fmt::Write as _;

/// Writes well-formed JSON into a growing `String`, tracking separators.
pub(crate) struct JsonWriter {
    buf: String,
    /// One flag per open container: whether it is still empty.
    empty: Vec<bool>,
    /// Set between a key and its value, where no comma belongs.
    expect_value: bool,
}

impl JsonWriter {
    pub(crate) fn with_capacity(bytes: usize) -> Self {
        Self {
            buf: String::with_capacity(bytes),
            empty: Vec::new(),
            expect_value: false,
        }
    }

    pub(crate) fn begin_object(&mut self) {
        self.separate();
        self.buf.push('{');
        self.empty.push(true);
    }

    pub(crate) fn end_object(&mut self) {
        self.empty.pop();
        self.buf.push('}');
    }

    pub(crate) fn begin_array(&mut self) {
        self.separate();
        self.buf.push('[');
        self.empty.push(true);
    }

    pub(crate) fn end_array(&mut self) {
        self.empty.pop();
        self.buf.push(']');
    }

    /// Write an object key; the next value written becomes its value.
    pub(crate) fn key(&mut self, key: &str) {
        self.separate();
        escape_into(&mut self.buf, key);
        self.buf.push(':');
        self.expect_value = true;
    }

    pub(crate) fn str(&mut self, value: &str) {
        self.separate();
        escape_into(&mut self.buf, value);
    }

    pub(crate) fn bool(&mut self, value: bool) {
        self.separate();
        self.buf.push_str(if value { "true" } else { "false" });
    }

    pub(crate) fn u32(&mut self, value: u32) {
        self.separate();
        let _ = write!(self.buf, "{value}");
    }

    pub(crate) fn f32(&mut self, value: f32) {
        self.separate();
        let _ = write!(self.buf, "{value}");
    }

    /// Splice in text that is already valid JSON: a tool schema, or the raw
    /// arguments a model streamed.
    pub(crate) fn raw(&mut self, json: &str) {
        self.separate();
        self.buf.push_str(json);
    }

    pub(crate) fn field_str(&mut self, key: &str, value: &str) {
        self.key(key);
        self.str(value);
    }

    pub(crate) fn field_bool(&mut self, key: &str, value: bool) {
        self.key(key);
        self.bool(value);
    }

    pub(crate) fn field_u32(&mut self, key: &str, value: u32) {
        self.key(key);
        self.u32(value);
    }

    pub(crate) fn field_f32(&mut self, key: &str, value: f32) {
        self.key(key);
        self.f32(value);
    }

    pub(crate) fn field_raw(&mut self, key: &str, json: &str) {
        self.key(key);
        self.raw(json);
    }

    pub(crate) fn finish(self) -> String {
        debug_assert!(self.empty.is_empty(), "unbalanced JSON containers");
        self.buf
    }

    /// Emit a comma if the enclosing container already holds something.
    fn separate(&mut self) {
        if self.expect_value {
            self.expect_value = false;
            return;
        }
        if let Some(empty) = self.empty.last_mut() {
            if *empty {
                *empty = false;
            } else {
                self.buf.push(',');
            }
        }
    }
}

/// Write `s` as a quoted JSON string, escaping only what the grammar requires.
fn escape_into(out: &mut String, s: &str) {
    out.push('"');
    let mut last = 0usize;
    for (i, ch) in s.char_indices() {
        let escaped: &str = match ch {
            '"' => "\\\"",
            '\\' => "\\\\",
            '\n' => "\\n",
            '\r' => "\\r",
            '\t' => "\\t",
            c if (c as u32) < 0x20 => {
                out.push_str(&s[last..i]);
                let _ = write!(out, "\\u{:04x}", c as u32);
                last = i + c.len_utf8();
                continue;
            }
            _ => continue,
        };
        out.push_str(&s[last..i]);
        out.push_str(escaped);
        last = i + ch.len_utf8();
    }
    out.push_str(&s[last..]);
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_nested_document() {
        let mut w = JsonWriter::with_capacity(64);
        w.begin_object();
        w.field_str("model", "deepseek-v4-flash");
        w.field_bool("stream", true);
        w.key("messages");
        w.begin_array();
        w.begin_object();
        w.field_str("role", "user");
        w.field_str("content", "hi");
        w.end_object();
        w.end_array();
        w.field_u32("max_tokens", 128);
        w.end_object();

        let out = w.finish();
        assert_eq!(
            out,
            concat!(
                r#"{"model":"deepseek-v4-flash","stream":true,"#,
                r#""messages":[{"role":"user","content":"hi"}],"max_tokens":128}"#
            )
        );
        // And it round-trips through a real parser.
        let _: serde_json::Value = serde_json::from_str(&out).unwrap();
    }

    #[test]
    fn escapes_everything_json_requires() {
        let raw = "quote\" back\\slash\nnew\ttab\u{1}ctl";
        let mut w = JsonWriter::with_capacity(16);
        w.str(raw);
        let out = w.finish();
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed, raw);
        assert!(out.contains("\\u0001"));
    }

    #[test]
    fn leaves_multibyte_text_intact() {
        let raw = "héllo → 世界 🦀";
        let mut w = JsonWriter::with_capacity(16);
        w.str(raw);
        let parsed: serde_json::Value = serde_json::from_str(&w.finish()).unwrap();
        assert_eq!(parsed, raw);
    }

    #[test]
    fn raw_values_are_spliced_verbatim() {
        let mut w = JsonWriter::with_capacity(16);
        w.begin_object();
        w.field_raw("arguments", r#"{"expr":"2+2"}"#);
        w.end_object();
        assert_eq!(w.finish(), r#"{"arguments":{"expr":"2+2"}}"#);
    }

    #[test]
    fn empty_containers_stay_valid() {
        let mut w = JsonWriter::with_capacity(16);
        w.begin_object();
        w.key("tools");
        w.begin_array();
        w.end_array();
        w.key("meta");
        w.begin_object();
        w.end_object();
        w.end_object();
        assert_eq!(w.finish(), r#"{"tools":[],"meta":{}}"#);
    }
}
