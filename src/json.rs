//! Minimal write-only JSON encoder.

pub fn escape_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Non-finite floats encode as `null` (JSON has no NaN/Inf).
pub fn number(out: &mut String, v: f64) {
    if v.is_finite() {
        out.push_str(&format!("{v}"));
    } else {
        out.push_str("null");
    }
}

/// Builder for JSON objects/arrays without intermediate value trees.
pub struct JsonWriter {
    buf: String,
    needs_comma: Vec<bool>,
}

impl Default for JsonWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl JsonWriter {
    pub fn new() -> Self {
        JsonWriter {
            buf: String::new(),
            needs_comma: vec![false],
        }
    }

    fn pre_value(&mut self) {
        if let Some(top) = self.needs_comma.last_mut() {
            if *top {
                self.buf.push(',');
            }
            *top = true;
        }
    }

    pub fn begin_object(&mut self) -> &mut Self {
        self.pre_value();
        self.buf.push('{');
        self.needs_comma.push(false);
        self
    }

    pub fn end_object(&mut self) -> &mut Self {
        self.needs_comma.pop();
        self.buf.push('}');
        self
    }

    pub fn begin_array(&mut self) -> &mut Self {
        self.pre_value();
        self.buf.push('[');
        self.needs_comma.push(false);
        self
    }

    pub fn end_array(&mut self) -> &mut Self {
        self.needs_comma.pop();
        self.buf.push(']');
        self
    }

    pub fn key(&mut self, k: &str) -> &mut Self {
        self.pre_value();
        escape_str(&mut self.buf, k);
        self.buf.push(':');
        // key consumed the comma slot; the value that follows must not add one
        if let Some(top) = self.needs_comma.last_mut() {
            *top = false;
        }
        self
    }

    pub fn string(&mut self, s: &str) -> &mut Self {
        self.pre_value();
        escape_str(&mut self.buf, s);
        self
    }

    pub fn number(&mut self, v: f64) -> &mut Self {
        self.pre_value();
        number(&mut self.buf, v);
        self
    }

    pub fn int(&mut self, v: i64) -> &mut Self {
        self.pre_value();
        self.buf.push_str(&v.to_string());
        self
    }

    pub fn finish(self) -> String {
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_strings() {
        let mut out = String::new();
        escape_str(&mut out, "a\"b\\c\nd\te\u{1}");
        assert_eq!(out, r#""a\"b\\c\nd\te\u0001""#);
    }

    #[test]
    fn non_finite_numbers_are_null() {
        let mut out = String::new();
        number(&mut out, f64::NAN);
        out.push(' ');
        number(&mut out, f64::INFINITY);
        out.push(' ');
        number(&mut out, 1.5);
        assert_eq!(out, "null null 1.5");
    }

    #[test]
    fn writer_builds_nested_structures() {
        let mut w = JsonWriter::new();
        w.begin_object();
        w.key("name").string("m");
        w.key("count").int(3);
        w.key("points").begin_array();
        w.begin_array().int(1000).number(1.5).end_array();
        w.begin_array().int(2000).number(f64::NAN).end_array();
        w.end_array();
        w.key("empty").begin_object().end_object();
        w.end_object();
        assert_eq!(
            w.finish(),
            r#"{"name":"m","count":3,"points":[[1000,1.5],[2000,null]],"empty":{}}"#
        );
    }
}
