use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProtoError {
    #[error("empty message")]
    Empty,
    #[error("no command in message")]
    NoCommand,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub tags: Vec<(String, String)>,
    pub prefix: Option<String>,
    pub command: String,
    pub params: Vec<String>,
}

impl Message {
    #[cfg(test)]
    pub fn new(command: impl Into<String>, params: Vec<String>) -> Self {
        Self { tags: Vec::new(), prefix: None, command: command.into(), params }
    }

    pub fn with_prefix(prefix: impl Into<String>, command: impl Into<String>, params: Vec<String>) -> Self {
        Self { tags: Vec::new(), prefix: Some(prefix.into()), command: command.into(), params }
    }

    pub fn with_tag(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.tags.push((key.into(), value.into()));
        self
    }

    pub fn parse(line: &str) -> Result<Self, ProtoError> {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            return Err(ProtoError::Empty);
        }

        let mut rest = line;
        let tags = if let Some(stripped) = rest.strip_prefix('@') {
            let (tag_section, r) = split_once_space(stripped);
            rest = r.trim_start_matches(' ');
            parse_tags(tag_section)
        } else {
            Vec::new()
        };

        let prefix = if let Some(stripped) = rest.strip_prefix(':') {
            let (p, r) = split_once_space(stripped);
            rest = r.trim_start_matches(' ');
            Some(p.to_string())
        } else {
            None
        };

        let (command, mut rest) = split_once_space(rest);
        if command.is_empty() {
            return Err(ProtoError::NoCommand);
        }
        let command = command.to_ascii_uppercase();

        let mut params = Vec::new();
        loop {
            rest = rest.trim_start_matches(' ');
            if rest.is_empty() {
                break;
            }
            if let Some(trailing) = rest.strip_prefix(':') {
                params.push(trailing.to_string());
                break;
            }
            let (p, r) = split_once_space(rest);
            params.push(p.to_string());
            rest = r;
        }

        Ok(Self { tags, prefix, command, params })
    }

    /// Serialize to wire format (without trailing CRLF).
    pub fn to_wire(&self) -> String {
        let mut out = String::new();
        if !self.tags.is_empty() {
            out.push('@');
            for (i, (k, v)) in self.tags.iter().enumerate() {
                if i > 0 {
                    out.push(';');
                }
                out.push_str(k);
                if !v.is_empty() {
                    out.push('=');
                    out.push_str(&escape_tag_value(v));
                }
            }
            out.push(' ');
        }
        if let Some(p) = &self.prefix {
            out.push(':');
            out.push_str(p);
            out.push(' ');
        }
        out.push_str(&self.command);
        for (i, param) in self.params.iter().enumerate() {
            out.push(' ');
            let last = i == self.params.len() - 1;
            let needs_trailing = last && (param.is_empty() || param.contains(' ') || param.starts_with(':'));
            if needs_trailing {
                out.push(':');
            }
            out.push_str(param);
        }
        out
    }
}

fn split_once_space(s: &str) -> (&str, &str) {
    match s.find(' ') {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, ""),
    }
}

fn parse_tags(section: &str) -> Vec<(String, String)> {
    section
        .split(';')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (k.to_string(), unescape_tag_value(v)),
            None => (p.to_string(), String::new()),
        })
        .collect()
}

fn escape_tag_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            ';' => out.push_str("\\:"),
            ' ' => out.push_str("\\s"),
            '\\' => out.push_str("\\\\"),
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

fn unescape_tag_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(':') => out.push(';'),
                Some('s') => out.push(' '),
                Some('\\') => out.push('\\'),
                Some('r') => out.push('\r'),
                Some('n') => out.push('\n'),
                Some(c) => out.push(c),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(cmd: &str, params: &[&str]) -> Message {
        Message::new(cmd, params.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn parse_inputs() {
        let cases: &[(&str, &str, &[&str])] = &[
            ("NICK foo",                     "NICK",    &["foo"]),
            ("USER foo 0 * :Real Name",      "USER",    &["foo", "0", "*", "Real Name"]),
            ("PING :token123",               "PING",    &["token123"]),
            ("CAP LS 302",                   "CAP",     &["LS", "302"]),
            ("CAP END",                      "CAP",     &["END"]),
            ("QUIT :bye now",                "QUIT",    &["bye now"]),
            ("LIST",                         "LIST",    &[]),
            ("NICK foo\r\n",                 "NICK",    &["foo"]),
            ("USER  foo  0  *  :Real Name", "USER",    &["foo", "0", "*", "Real Name"]),
        ];
        for (line, cmd, params) in cases {
            assert_eq!(Message::parse(line).unwrap(), m(cmd, params), "parse {line:?}");
        }
    }

    #[test]
    fn parse_with_prefix_and_trailing() {
        let got = Message::parse(":nick!user@host PRIVMSG #x :hi there").unwrap();
        assert_eq!(got.prefix.as_deref(), Some("nick!user@host"));
        assert_eq!(got.command, "PRIVMSG");
        assert_eq!(got.params, vec!["#x".to_string(), "hi there".to_string()]);
    }

    #[test]
    fn parse_normalizes_command_case() {
        assert_eq!(Message::parse("nick foo").unwrap().command, "NICK");
        assert_eq!(Message::parse("Cap eND").unwrap().command, "CAP");
    }

    #[test]
    fn parse_errors() {
        assert!(matches!(Message::parse(""), Err(ProtoError::Empty)));
        assert!(matches!(Message::parse("\r\n"), Err(ProtoError::Empty)));
        assert!(matches!(Message::parse(":nick"), Err(ProtoError::NoCommand)));
    }

    #[test]
    fn serialize_trailing_rules() {
        let cases: &[(&[&str], &str)] = &[
            (&["token"],            "PONG token"),
            (&["#chan", "hi world"], "PONG #chan :hi world"),
            (&["x", ":weird"],       "PONG x ::weird"),
            (&["*", "LS", ""],       "PONG * LS :"),
        ];
        for (params, want) in cases {
            let got = m("PONG", params).to_wire();
            assert_eq!(&got, want);
        }
    }

    #[test]
    fn serialize_with_prefix() {
        let m = Message::with_prefix("matrirc.local", "001", vec!["foo".into(), "Welcome".into()]);
        assert_eq!(m.to_wire(), ":matrirc.local 001 foo Welcome");
    }

    #[test]
    fn tags_parse_and_unescape() {
        let m = Message::parse("@time=2024-03-14T12:34:56.789Z :n!u@h PRIVMSG #c :hi").unwrap();
        assert_eq!(m.tags, vec![("time".into(), "2024-03-14T12:34:56.789Z".into())]);
        let m = Message::parse("@a=one;b=two\\sthree;c PING :x").unwrap();
        assert_eq!(m.tags, vec![
            ("a".into(), "one".into()),
            ("b".into(), "two three".into()),
            ("c".into(), String::new()),
        ]);
    }

    #[test]
    fn tags_serialize_and_escape() {
        let m = Message::with_prefix("n!u@h", "PRIVMSG", vec!["#c".into(), "hi".into()])
            .with_tag("time", "2024-03-14T12:34:56.789Z");
        assert_eq!(m.to_wire(), "@time=2024-03-14T12:34:56.789Z :n!u@h PRIVMSG #c hi");
        let m = Message::new("PING", vec!["x".into()]).with_tag("k", "a b;c");
        assert_eq!(m.to_wire(), "@k=a\\sb\\:c PING x");
    }

    #[test]
    fn round_trip() {
        let msgs = [
            Message::with_prefix("matrirc.local", "001", vec!["foo".into(), "Welcome foo".into()]),
            Message::new("USER", vec!["foo".into(), "0".into(), "*".into(), "Real Name".into()]),
            Message::with_prefix("x", "PRIVMSG", vec!["#c".into(), "body space".into()])
                .with_tag("time", "2024-01-01T00:00:00Z")
                .with_tag("msgid", "abc;123"),
        ];
        for m in msgs {
            let parsed = Message::parse(&m.to_wire()).unwrap();
            assert_eq!(parsed, m);
        }
    }
}
