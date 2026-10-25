//! RESP2/RESP3 parsing (commands from clients) and encoding (replies).

use bytes::{Buf, BytesMut};

/// A reply value; encoded according to the connection's protocol version.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Simple(String),
    Error(String),
    Int(i64),
    Bulk(Vec<u8>),
    Null,
    Array(Vec<Reply>),
    /// RESP3 map (flattened to an array in RESP2).
    Map(Vec<(Reply, Reply)>),
    /// RESP3 set (an array in RESP2).
    Set(Vec<Reply>),
    /// RESP3 double (a bulk string in RESP2).
    Double(f64),
    /// RESP3 boolean (an integer in RESP2).
    Bool(bool),
    /// RESP3 null array (`*-1` in RESP2).
    NullArray,
    /// Out-of-band push message (pub/sub).
    Push(Vec<Reply>),
}

impl Reply {
    pub fn ok() -> Reply {
        Reply::Simple("OK".into())
    }

    pub fn err(msg: impl Into<String>) -> Reply {
        Reply::Error(msg.into())
    }

    pub fn bulk(s: impl AsRef<[u8]>) -> Reply {
        Reply::Bulk(s.as_ref().to_vec())
    }

    pub fn opt_bulk(v: Option<impl AsRef<[u8]>>) -> Reply {
        match v {
            Some(s) => Reply::bulk(s),
            None => Reply::Null,
        }
    }

    pub fn bulks<I, T>(items: I) -> Reply
    where
        I: IntoIterator<Item = T>,
        T: AsRef<[u8]>,
    {
        Reply::Array(items.into_iter().map(Reply::bulk).collect())
    }

    pub fn encode(&self, out: &mut Vec<u8>, resp3: bool) {
        match self {
            Reply::Simple(s) => {
                out.push(b'+');
                out.extend(s.as_bytes());
                out.extend(b"\r\n");
            }
            Reply::Error(s) => {
                out.push(b'-');
                out.extend(s.as_bytes());
                out.extend(b"\r\n");
            }
            Reply::Int(i) => {
                out.extend(format!(":{i}\r\n").as_bytes());
            }
            Reply::Bulk(b) => {
                out.extend(format!("${}\r\n", b.len()).as_bytes());
                out.extend(b);
                out.extend(b"\r\n");
            }
            Reply::Null => out.extend(if resp3 { &b"_\r\n"[..] } else { &b"$-1\r\n"[..] }),
            Reply::NullArray => out.extend(if resp3 { &b"_\r\n"[..] } else { &b"*-1\r\n"[..] }),
            Reply::Array(items) => {
                out.extend(format!("*{}\r\n", items.len()).as_bytes());
                for i in items {
                    i.encode(out, resp3);
                }
            }
            Reply::Push(items) => {
                out.extend(format!("{}{}\r\n", if resp3 { '>' } else { '*' }, items.len()).as_bytes());
                for i in items {
                    i.encode(out, resp3);
                }
            }
            Reply::Set(items) => {
                out.extend(format!("{}{}\r\n", if resp3 { '~' } else { '*' }, items.len()).as_bytes());
                for i in items {
                    i.encode(out, resp3);
                }
            }
            Reply::Map(pairs) => {
                if resp3 {
                    out.extend(format!("%{}\r\n", pairs.len()).as_bytes());
                } else {
                    out.extend(format!("*{}\r\n", pairs.len() * 2).as_bytes());
                }
                for (k, v) in pairs {
                    k.encode(out, resp3);
                    v.encode(out, resp3);
                }
            }
            Reply::Double(f) => {
                let s = format_double(*f);
                if resp3 {
                    out.extend(format!(",{s}\r\n").as_bytes());
                } else {
                    Reply::Bulk(s.into_bytes()).encode(out, resp3);
                }
            }
            Reply::Bool(b) => {
                if resp3 {
                    out.extend(if *b { &b"#t\r\n"[..] } else { &b"#f\r\n"[..] });
                } else {
                    Reply::Int(*b as i64).encode(out, resp3);
                }
            }
        }
    }
}

/// Redis' float formatting (`17.5`, `3`, `inf`).
pub fn format_double(f: f64) -> String {
    if f.is_infinite() {
        return if f > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if f == f.trunc() && f.abs() < 1e17 {
        return format!("{}", f as i64);
    }
    let s = format!("{f}");
    s
}

/// Parse one command from the buffer. Returns `Ok(None)` when more bytes are needed.
pub fn parse_command(buf: &mut BytesMut) -> Result<Option<Vec<Vec<u8>>>, String> {
    if buf.is_empty() {
        return Ok(None);
    }
    if buf[0] != b'*' {
        // Inline command (telnet style).
        let Some(end) = buf.windows(2).position(|w| w == b"\r\n").or_else(|| buf.iter().position(|b| *b == b'\n')) else {
            return Ok(None);
        };
        let line = String::from_utf8_lossy(&buf[..end]).into_owned();
        let skip = if buf.get(end) == Some(&b'\r') { end + 2 } else { end + 1 };
        buf.advance(skip);
        let parts = split_inline(&line);
        if parts.is_empty() {
            return parse_command(buf);
        }
        return Ok(Some(parts));
    }
    let mut pos = 0;
    let line = |pos: usize, buf: &BytesMut| -> Option<(String, usize)> {
        let rel = buf[pos..].windows(2).position(|w| w == b"\r\n")?;
        Some((String::from_utf8_lossy(&buf[pos..pos + rel]).into_owned(), pos + rel + 2))
    };
    let Some((header, next)) = line(pos, buf) else { return Ok(None) };
    let count: i64 = header[1..].parse().map_err(|_| "Protocol error: invalid multibulk length".to_string())?;
    // Redis's own limits (1M arguments, proto-max-bulk-len 512 MB), so a header cannot make the
    // server reserve memory it will never receive.
    if count > 1024 * 1024 {
        return Err("Protocol error: invalid multibulk length".into());
    }
    pos = next;
    let mut args = Vec::with_capacity(count.clamp(0, 1024) as usize);
    for _ in 0..count {
        let Some((h, next)) = line(pos, buf) else { return Ok(None) };
        if !h.starts_with('$') {
            return Err(format!("Protocol error: expected '$', got '{}'", h.chars().next().unwrap_or(' ')));
        }
        let len: usize =
            h[1..].parse().ok().filter(|l| *l <= 512 * 1024 * 1024).ok_or_else(|| "Protocol error: invalid bulk length".to_string())?;
        if buf.len() < next + len + 2 {
            return Ok(None);
        }
        args.push(buf[next..next + len].to_vec());
        pos = next + len + 2;
    }
    buf.advance(pos);
    Ok(Some(args))
}

fn split_inline(line: &str) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has = false;
    for c in line.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                has = true;
            }
            None if c.is_whitespace() => {
                if has || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur).into_bytes());
                    has = false;
                }
            }
            None => cur.push(c),
        }
    }
    if has || !cur.is_empty() {
        out.push(cur.into_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_huge_lengths_without_allocating() {
        let mut b = BytesMut::from(&b"*2147483647\r\n"[..]);
        assert!(parse_command(&mut b).is_err());
        let mut b = BytesMut::from(&b"*1\r\n$99999999999\r\n"[..]);
        assert!(parse_command(&mut b).is_err());
        // A plausible header waits for its data.
        let mut b = BytesMut::from(&b"*100000\r\n"[..]);
        assert_eq!(parse_command(&mut b), Ok(None));
    }

    #[test]
    fn parses_multibulk_and_inline() {
        let mut b = BytesMut::from(&b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\nPING\r\n"[..]);
        assert_eq!(parse_command(&mut b).unwrap().unwrap(), vec![b"GET".to_vec(), b"k".to_vec()]);
        assert_eq!(parse_command(&mut b).unwrap().unwrap(), vec![b"PING".to_vec()]);
        let mut partial = BytesMut::from(&b"*2\r\n$3\r\nGET\r\n$1\r\n"[..]);
        assert_eq!(parse_command(&mut partial).unwrap(), None);
    }

    #[test]
    fn encodes_resp2_and_resp3() {
        let r = Reply::Map(vec![(Reply::bulk("a"), Reply::Int(1))]);
        let mut v2 = Vec::new();
        r.encode(&mut v2, false);
        assert_eq!(v2, b"*2\r\n$1\r\na\r\n:1\r\n");
        let mut v3 = Vec::new();
        r.encode(&mut v3, true);
        assert_eq!(v3, b"%1\r\n$1\r\na\r\n:1\r\n");
        assert_eq!(format_double(2.0), "2");
        assert_eq!(format_double(1.5), "1.5");
    }
}
