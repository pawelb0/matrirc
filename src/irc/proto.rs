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
    #[allow(dead_code)] // used in Step 2+ for client-bound messages without server prefix
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

    #[track_caller]
    fn parses_to(line: &str, expected: Message) {
        let got = Message::parse(line).unwrap();
        assert_eq!(got, expected, "parsing {line:?}");
    }

    #[test]
    fn parse_nick() {
        parses_to("NICK foo", Message::new("NICK", vec!["foo".into()]));
    }

    #[test]
    fn parse_user() {
        parses_to(
            "USER foo 0 * :Real Name",
            Message::new("USER", vec!["foo".into(), "0".into(), "*".into(), "Real Name".into()]),
        );
    }

    #[test]
    fn parse_ping_trailing() {
        parses_to("PING :token123", Message::new("PING", vec!["token123".into()]));
    }

    #[test]
    fn parse_cap_ls() {
        parses_to(
            "CAP LS 302",
            Message::new("CAP", vec!["LS".into(), "302".into()]),
        );
    }

    #[test]
    fn parse_cap_end() {
        parses_to("CAP END", Message::new("CAP", vec!["END".into()]));
    }

    #[test]
    fn parse_quit_with_reason() {
        parses_to(
            "QUIT :bye now",
            Message::new("QUIT", vec!["bye now".into()]),
        );
    }

    #[test]
    fn parse_with_prefix() {
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
    fn parse_strips_crlf() {
        parses_to("NICK foo\r\n", Message::new("NICK", vec!["foo".into()]));
        parses_to("NICK foo\n", Message::new("NICK", vec!["foo".into()]));
    }

    #[test]
    fn parse_collapses_extra_spaces() {
        parses_to(
            "USER  foo  0  *  :Real Name",
            Message::new("USER", vec!["foo".into(), "0".into(), "*".into(), "Real Name".into()]),
        );
    }

    #[test]
    fn parse_command_only() {
        parses_to("LIST", Message::new("LIST", vec![]));
    }

    #[test]
    fn parse_empty_errors() {
        assert!(matches!(Message::parse(""), Err(ProtoError::Empty)));
        assert!(matches!(Message::parse("\r\n"), Err(ProtoError::Empty)));
    }

    #[test]
    fn parse_prefix_only_errors() {
        // ":nick" with no command => after stripping prefix, command is empty
        assert!(matches!(Message::parse(":nick"), Err(ProtoError::NoCommand)));
    }

    #[test]
    fn serialize_simple() {
        assert_eq!(
            Message::new("PONG", vec!["token".into()]).to_wire(),
            "PONG token"
        );
    }

    #[test]
    fn serialize_trailing_when_spaces() {
        assert_eq!(
            Message::new("PRIVMSG", vec!["#chan".into(), "hello world".into()]).to_wire(),
            "PRIVMSG #chan :hello world"
        );
    }

    #[test]
    fn serialize_trailing_when_starts_with_colon() {
        assert_eq!(
            Message::new("NOTICE", vec!["x".into(), ":weird".into()]).to_wire(),
            "NOTICE x ::weird"
        );
    }

    #[test]
    fn serialize_trailing_when_empty() {
        assert_eq!(
            Message::new("CAP", vec!["*".into(), "LS".into(), "".into()]).to_wire(),
            "CAP * LS :"
        );
    }

    #[test]
    fn serialize_with_prefix() {
        assert_eq!(
            Message::with_prefix("matrirc.local", "001", vec!["foo".into(), "Welcome to matrirc".into()])
                .to_wire(),
            ":matrirc.local 001 foo :Welcome to matrirc"
        );
    }

    #[test]
    fn round_trip_001_welcome() {
        let m = Message::with_prefix(
            "matrirc.local",
            "001",
            vec!["foo".into(), "Welcome foo".into()],
        );
        let wire = m.to_wire();
        let parsed = Message::parse(&wire).unwrap();
        assert_eq!(parsed, m);
    }

    #[test]
    fn parse_tags_basic() {
        let m = Message::parse("@time=2024-03-14T12:34:56.789Z :nick!u@h PRIVMSG #c :hi").unwrap();
        assert_eq!(m.tags, vec![("time".into(), "2024-03-14T12:34:56.789Z".into())]);
        assert_eq!(m.prefix.as_deref(), Some("nick!u@h"));
        assert_eq!(m.command, "PRIVMSG");
        assert_eq!(m.params, vec!["#c".to_string(), "hi".to_string()]);
    }

    #[test]
    fn parse_multiple_tags_and_unescape() {
        let m = Message::parse("@a=one;b=two\\sthree;c PING :x").unwrap();
        assert_eq!(
            m.tags,
            vec![
                ("a".into(), "one".into()),
                ("b".into(), "two three".into()),
                ("c".into(), String::new()),
            ]
        );
    }

    #[test]
    fn serialize_tag_with_server_time() {
        let m = Message::with_prefix("nick!u@h", "PRIVMSG", vec!["#c".into(), "hi".into()])
            .with_tag("time", "2024-03-14T12:34:56.789Z");
        assert_eq!(
            m.to_wire(),
            "@time=2024-03-14T12:34:56.789Z :nick!u@h PRIVMSG #c hi"
        );
    }

    #[test]
    fn serialize_tag_escapes_space_and_semicolon() {
        let m = Message::new("PING", vec!["x".into()]).with_tag("k", "a b;c");
        assert_eq!(m.to_wire(), "@k=a\\sb\\:c PING x");
    }

    #[test]
    fn round_trip_tag() {
        let original = Message::with_prefix("x", "PRIVMSG", vec!["#c".into(), "body space".into()])
            .with_tag("time", "2024-01-01T00:00:00Z")
            .with_tag("msgid", "abc;123");
        let wire = original.to_wire();
        let parsed = Message::parse(&wire).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn round_trip_user() {
        let m = Message::new(
            "USER",
            vec!["foo".into(), "0".into(), "*".into(), "Real Name".into()],
        );
        let wire = m.to_wire();
        let parsed = Message::parse(&wire).unwrap();
        assert_eq!(parsed, m);
    }
}
