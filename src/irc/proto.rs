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
    pub prefix: Option<String>,
    pub command: String,
    pub params: Vec<String>,
}

impl Message {
    #[allow(dead_code)] // used in Step 2+ for client-bound messages without server prefix
    pub fn new(command: impl Into<String>, params: Vec<String>) -> Self {
        Self { prefix: None, command: command.into(), params }
    }

    pub fn with_prefix(prefix: impl Into<String>, command: impl Into<String>, params: Vec<String>) -> Self {
        Self { prefix: Some(prefix.into()), command: command.into(), params }
    }

    pub fn parse(line: &str) -> Result<Self, ProtoError> {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            return Err(ProtoError::Empty);
        }

        let mut rest = line;
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

        Ok(Self { prefix, command, params })
    }

    /// Serialize to wire format (without trailing CRLF).
    pub fn to_wire(&self) -> String {
        let mut out = String::new();
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
