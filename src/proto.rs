use std::io::{self, Read, Write};

pub const BEGIN_REQUEST: u8 = 1;
pub const END_REQUEST: u8 = 3;
pub const PARAMS: u8 = 4;
pub const STDIN: u8 = 5;
pub const STDOUT: u8 = 6;
pub const STDERR: u8 = 7;

pub struct Record {
    pub typ: u8,
    #[allow(dead_code)]
    pub request_id: u16,
    pub data: Vec<u8>,
}

fn write_record(w: &mut impl Write, typ: u8, request_id: u16, data: &[u8]) -> io::Result<()> {
    debug_assert!(
        data.len() <= u16::MAX as usize,
        "record data too long for content_len field"
    );
    let content_len = data.len() as u16;
    let padding = (8 - (data.len() % 8)) % 8;
    w.write_all(&[
        1, // version
        typ,
        (request_id >> 8) as u8,
        (request_id & 0xff) as u8,
        (content_len >> 8) as u8,
        (content_len & 0xff) as u8,
        padding as u8,
        0, // reserved
    ])?;
    w.write_all(data)?;
    if padding > 0 {
        w.write_all(&vec![0u8; padding])?;
    }
    Ok(())
}

pub fn write_begin_request(w: &mut impl Write, request_id: u16) -> io::Result<()> {
    // role=RESPONDER(1) as u16 BE, flags=0 (close conn after), reserved[5]
    write_record(w, BEGIN_REQUEST, request_id, &[0, 1, 0, 0, 0, 0, 0, 0])
}

fn encode_length(buf: &mut Vec<u8>, len: usize) {
    if len <= 127 {
        buf.push(len as u8);
    } else {
        buf.push(((len >> 24) as u8) | 0x80);
        buf.push((len >> 16) as u8);
        buf.push((len >> 8) as u8);
        buf.push(len as u8);
    }
}

pub fn write_params(w: &mut impl Write, request_id: u16, pairs: &[(&str, &str)]) -> io::Result<()> {
    let mut buf = Vec::new();
    for (name, value) in pairs {
        encode_length(&mut buf, name.len());
        encode_length(&mut buf, value.len());
        buf.extend_from_slice(name.as_bytes());
        buf.extend_from_slice(value.as_bytes());
    }
    for chunk in buf.chunks(0xffff) {
        write_record(w, PARAMS, request_id, chunk)?;
    }
    // empty PARAMS signals end of parameters
    write_record(w, PARAMS, request_id, &[])
}

pub fn write_stdin_record(w: &mut impl Write, request_id: u16, data: &[u8]) -> io::Result<()> {
    write_record(w, STDIN, request_id, data)
}

pub fn read_record(r: &mut impl Read) -> io::Result<Record> {
    let mut hdr = [0u8; 8];
    r.read_exact(&mut hdr)?;
    let typ = hdr[1];
    let request_id = u16::from_be_bytes([hdr[2], hdr[3]]);
    let content_len = u16::from_be_bytes([hdr[4], hdr[5]]) as usize;
    let padding_len = hdr[6] as usize;
    let mut data = vec![0u8; content_len];
    r.read_exact(&mut data)?;
    if padding_len > 0 {
        let mut pad = vec![0u8; padding_len];
        r.read_exact(&mut pad)?;
    }
    Ok(Record {
        typ,
        request_id,
        data,
    })
}

/// Parse a CGI response: returns (status, headers, body_offset).
/// CGI response headers are separated from the body by \r\n\r\n or \n\n.
/// A "Status: NNN ..." header sets the HTTP status code; it is consumed and not forwarded.
pub fn parse_cgi_response(data: &[u8]) -> (u16, Vec<(String, String)>, usize) {
    let sep = data
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| (p, p + 4))
        .or_else(|| {
            data.windows(2)
                .position(|w| w == b"\n\n")
                .map(|p| (p, p + 2))
        });

    let (header_end, body_start) = sep.unwrap_or((data.len(), data.len()));

    let mut status = 200u16;
    let mut headers = Vec::new();

    for line in data[..header_end].split(|&b| b == b'\n') {
        let line = if line.last() == Some(&b'\r') {
            &line[..line.len() - 1]
        } else {
            line
        };
        if line.is_empty() {
            continue;
        }
        if let Some(colon) = line.iter().position(|&b| b == b':') {
            let name = std::str::from_utf8(&line[..colon]).unwrap_or("").trim();
            let value = std::str::from_utf8(&line[colon + 1..]).unwrap_or("").trim();
            if name.eq_ignore_ascii_case("Status") {
                if let Some(code_str) = value.split(' ').next() {
                    status = code_str.parse().unwrap_or(200);
                }
            } else if !name.is_empty() {
                headers.push((name.to_string(), value.to_string()));
            }
        }
    }

    (status, headers, body_start)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_roundtrip() {
        let mut buf = Vec::new();
        write_record(&mut buf, STDOUT, 42, b"hello").unwrap();
        let rec = read_record(&mut buf.as_slice()).unwrap();
        assert_eq!(rec.typ, STDOUT);
        assert_eq!(rec.request_id, 42);
        assert_eq!(rec.data, b"hello");
    }

    #[test]
    fn record_roundtrip_needs_padding() {
        // 5-byte payload needs 3 bytes of padding to reach an 8-byte boundary.
        let mut buf = Vec::new();
        write_record(&mut buf, STDIN, 1, b"abcde").unwrap();
        assert_eq!(buf.len(), 8 + 5 + 3);
        let rec = read_record(&mut buf.as_slice()).unwrap();
        assert_eq!(rec.data, b"abcde");
    }

    #[test]
    fn record_roundtrip_empty() {
        let mut buf = Vec::new();
        write_record(&mut buf, PARAMS, 1, &[]).unwrap();
        assert_eq!(buf.len(), 8);
        let rec = read_record(&mut buf.as_slice()).unwrap();
        assert!(rec.data.is_empty());
    }

    #[test]
    #[should_panic(expected = "record data too long")]
    fn write_record_rejects_oversized_data() {
        let mut buf = Vec::new();
        let oversized = vec![0u8; u16::MAX as usize + 1];
        let _ = write_record(&mut buf, STDOUT, 1, &oversized);
    }

    #[test]
    fn encode_length_boundary() {
        let mut buf = Vec::new();
        encode_length(&mut buf, 0);
        assert_eq!(buf, vec![0]);

        let mut buf = Vec::new();
        encode_length(&mut buf, 127);
        assert_eq!(buf, vec![127]);

        let mut buf = Vec::new();
        encode_length(&mut buf, 128);
        assert_eq!(buf, vec![0x80, 0, 0, 128]);

        let mut buf = Vec::new();
        encode_length(&mut buf, 300);
        assert_eq!(buf, vec![0x80, 0, 1, 44]);
    }

    #[test]
    fn write_params_roundtrip() {
        let mut buf = Vec::new();
        write_params(
            &mut buf,
            1,
            &[("SCRIPT_NAME", "/index.php"), ("QUERY_STRING", "")],
        )
        .unwrap();

        let mut cursor = buf.as_slice();
        let rec = read_record(&mut cursor).unwrap();
        assert_eq!(rec.typ, PARAMS);
        assert!(!rec.data.is_empty());

        // Second record is the empty PARAMS terminator.
        let terminator = read_record(&mut cursor).unwrap();
        assert_eq!(terminator.typ, PARAMS);
        assert!(terminator.data.is_empty());
    }

    #[test]
    fn parse_cgi_response_crlf_separator() {
        let data = b"Status: 404 Not Found\r\nContent-Type: text/html\r\n\r\nbody";
        let (status, headers, body_start) = parse_cgi_response(data);
        assert_eq!(status, 404);
        assert_eq!(
            headers,
            vec![("Content-Type".to_string(), "text/html".to_string())]
        );
        assert_eq!(&data[body_start..], b"body");
    }

    #[test]
    fn parse_cgi_response_lf_separator() {
        let data = b"Content-Type: text/plain\n\nbody";
        let (status, headers, body_start) = parse_cgi_response(data);
        assert_eq!(status, 200);
        assert_eq!(
            headers,
            vec![("Content-Type".to_string(), "text/plain".to_string())]
        );
        assert_eq!(&data[body_start..], b"body");
    }

    #[test]
    fn parse_cgi_response_no_separator() {
        // No header/body separator found at all: everything is treated as headers, no body.
        let data = b"Content-Type: text/plain";
        let (status, headers, body_start) = parse_cgi_response(data);
        assert_eq!(status, 200);
        assert_eq!(
            headers,
            vec![("Content-Type".to_string(), "text/plain".to_string())]
        );
        assert_eq!(body_start, data.len());
    }

    #[test]
    fn parse_cgi_response_status_without_reason() {
        let data = b"Status: 301\r\n\r\n";
        let (status, _, _) = parse_cgi_response(data);
        assert_eq!(status, 301);
    }

    #[test]
    fn parse_cgi_response_ignores_lines_without_colon() {
        let data = b"garbage line\r\nContent-Type: text/html\r\n\r\n";
        let (_, headers, _) = parse_cgi_response(data);
        assert_eq!(
            headers,
            vec![("Content-Type".to_string(), "text/html".to_string())]
        );
    }

    #[test]
    fn parse_cgi_response_preserves_duplicate_headers() {
        let data = b"Set-Cookie: a=1\r\nSet-Cookie: b=2\r\n\r\n";
        let (_, headers, _) = parse_cgi_response(data);
        assert_eq!(
            headers,
            vec![
                ("Set-Cookie".to_string(), "a=1".to_string()),
                ("Set-Cookie".to_string(), "b=2".to_string()),
            ]
        );
    }
}
