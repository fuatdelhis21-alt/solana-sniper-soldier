//! Secret redaction for log output.
//!
//! RPC providers put the API key in the URL (Helius `?api-key=...`,
//! QuickNode `https://x.quiknode.pro/<token>/`), and those URLs end up in log
//! lines both directly (startup info) and inside RPC client error messages
//! ("... for url (https://...)"). Every log line goes through
//! [`RedactingMakeWriter`], which masks:
//! - every query-string value of an `http(s)://` / `ws(s)://` URL,
//! - every URL path segment of 20+ characters (path-style tokens),
//! - `api-key=` / `api_key=` / `apikey=` / `token=` values anywhere.

use std::io::{self, Write};

use tracing_subscriber::fmt::MakeWriter;

const MASK: &str = "***";

fn is_delim(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '"' | '\'' | ')' | '(' | ',' | ']' | '[' | '}' | '{' | '<' | '>' | '\\'
        )
}

/// Mask one URL (scheme already included in `url`).
fn redact_url(url: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (url, None),
    };
    let (scheme, rest) = base.split_once("://").unwrap_or(("", base));
    let mut parts = rest.split('/');
    let host = parts.next().unwrap_or("");
    let mut out = format!("{scheme}://{host}");
    for seg in parts {
        out.push('/');
        if seg.len() >= 20 {
            out.push_str(MASK);
        } else {
            out.push_str(seg);
        }
    }
    if let Some(q) = query {
        out.push('?');
        let masked: Vec<String> = q
            .split('&')
            .map(|kv| match kv.split_once('=') {
                Some((k, _)) => format!("{k}={MASK}"),
                None => kv.to_string(),
            })
            .collect();
        out.push_str(&masked.join("&"));
    }
    out
}

/// Return `s` with secrets masked (see module docs).
pub fn redact_secrets(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let bytes = s.as_bytes();
    while i < s.len() {
        let rest = &s[i..];
        let lower: String = rest
            .chars()
            .take(8)
            .collect::<String>()
            .to_ascii_lowercase();
        let is_url = ["http://", "https://", "ws://", "wss://"]
            .iter()
            .any(|p| lower.starts_with(p));
        if is_url {
            let end = rest.find(is_delim).unwrap_or(rest.len());
            out.push_str(&redact_url(&rest[..end]));
            i += end;
            continue;
        }
        let mut matched = false;
        for key in ["api-key=", "api_key=", "apikey=", "token="] {
            if rest
                .get(..key.len())
                .is_some_and(|p| p.eq_ignore_ascii_case(key))
            {
                let prev_ok = i == 0 || !(bytes[i - 1] as char).is_ascii_alphanumeric();
                if prev_ok {
                    out.push_str(&rest[..key.len()]);
                    let val = &rest[key.len()..];
                    let end = val
                        .find(|c: char| is_delim(c) || c == '&')
                        .unwrap_or(val.len());
                    out.push_str(MASK);
                    i += key.len() + end;
                    matched = true;
                    break;
                }
            }
        }
        if matched {
            continue;
        }
        let ch = rest.chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Writer that redacts each buffer before passing it on. `tracing` formats an
/// event completely before a single `write`, so a secret is never split.
pub struct RedactingWriter<W: Write> {
    inner: W,
}

impl<W: Write> Write for RedactingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let text = String::from_utf8_lossy(buf);
        self.inner.write_all(redact_secrets(&text).as_bytes())?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// `MakeWriter` wrapper applying [`redact_secrets`] to every log line.
pub struct RedactingMakeWriter<M> {
    inner: M,
}

impl<M> RedactingMakeWriter<M> {
    pub fn new(inner: M) -> Self {
        Self { inner }
    }
}

impl<'a, M> MakeWriter<'a> for RedactingMakeWriter<M>
where
    M: MakeWriter<'a>,
{
    type Writer = RedactingWriter<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter {
            inner: self.inner.make_writer(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_helius_query_key() {
        let s = r#"{"rpc":"https://mainnet.helius-rpc.com/?api-key=b7401c4e-2021-4a52-9125-5e29adfef684","x":1}"#;
        let r = redact_secrets(s);
        assert!(!r.contains("b7401c4e"), "{r}");
        assert!(
            r.contains("https://mainnet.helius-rpc.com/?api-key=***"),
            "{r}"
        );
        assert!(r.ends_with(r#"","x":1}"#), "{r}");
    }

    #[test]
    fn masks_error_message_url_and_ws() {
        let s = "HTTP status client error (429) for url (https://mainnet.helius-rpc.com/?api-key=SECRET&x=1) ws=wss://h.example/?api-key=SECRET2";
        let r = redact_secrets(s);
        assert!(!r.contains("SECRET"), "{r}");
        assert!(
            r.contains("(https://mainnet.helius-rpc.com/?api-key=***&x=***)"),
            "{r}"
        );
    }

    #[test]
    fn masks_path_style_token() {
        let r = redact_secrets(
            "rpc=https://abc.solana-mainnet.quiknode.pro/0123456789abcdef0123456789abcdef/ ok",
        );
        assert_eq!(r, "rpc=https://abc.solana-mainnet.quiknode.pro/***/ ok");
    }

    #[test]
    fn keeps_plain_public_rpc_and_pubkeys() {
        let s = "rpc https://api.mainnet-beta.solana.com mint BCNT4t3rv5Hva8RnUtJUJLnxzeFAabcYp8CghC1SmWin";
        assert_eq!(redact_secrets(s), s);
    }

    #[test]
    fn masks_bare_key_value() {
        assert_eq!(redact_secrets("token=abc def"), "token=*** def");
        assert_eq!(redact_secrets("mytoken=abc"), "mytoken=abc");
    }

    #[test]
    fn multibyte_text_never_panics() {
        let s = "paper market data unavailable — rejected ş ğ ü https://h/?api-key=S — tokenı";
        let r = redact_secrets(s);
        assert!(
            r.contains("— rejected ş ğ ü https://h/?api-key=*** — tokenı"),
            "{r}"
        );
        for i in 0..s.len() {
            if s.is_char_boundary(i) {
                let _ = redact_secrets(&s[i..]);
            }
        }
        assert_eq!(redact_secrets("ws://h/?a=b"), "ws://h/?a=***");
    }

    #[test]
    fn writer_redacts() {
        let mw = RedactingMakeWriter::new(|| Vec::<u8>::new());
        let mut w = mw.make_writer();
        w.write_all(b"x https://h/?api-key=S y").unwrap();
        assert_eq!(
            String::from_utf8(w.inner).unwrap(),
            "x https://h/?api-key=*** y"
        );
    }
}
