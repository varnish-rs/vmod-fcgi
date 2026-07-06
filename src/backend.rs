use std::io::{self, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use socket2::{Domain, SockAddr, Socket, Type};
use varnish::ffi;
use varnish::vcl::{Ctx, VclBackend, VclError, VclResponse};

use crate::proto;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(60);
const REQUEST_ID: u16 = 1;
const MAX_RESPONSE_HEADER_BYTES: usize = 64 * 1024;

pub enum Endpoint {
    Tcp { host: String, port: u16 },
    Unix(PathBuf),
}

pub struct FastCgiBackend {
    pub endpoint: Endpoint,
    pub docroot: String,
}

pub struct FastCgiResponse {
    stream: Option<Stream>,
    // body bytes already read from the STDOUT record that contained the header/body separator
    buf: Vec<u8>,
    buf_pos: usize,
    done: bool,
    ip: Option<SocketAddr>,
}

enum Stream {
    Tcp(BufReader<TcpStream>),
    Unix(BufReader<UnixStream>),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Tcp(r) => r.read(buf),
            Stream::Unix(r) => r.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Tcp(r) => r.get_mut().write(buf),
            Stream::Unix(r) => r.get_mut().write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Tcp(r) => r.get_mut().flush(),
            Stream::Unix(r) => r.get_mut().flush(),
        }
    }
}

fn connect(endpoint: &Endpoint) -> io::Result<(Stream, Option<SocketAddr>)> {
    match endpoint {
        Endpoint::Tcp { host, port } => {
            let mut last_err = None;
            for addr in (host.as_str(), *port).to_socket_addrs()? {
                match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
                    Ok(tcp) => {
                        tcp.set_read_timeout(Some(IO_TIMEOUT))?;
                        tcp.set_write_timeout(Some(IO_TIMEOUT))?;
                        let peer = tcp.peer_addr().ok();
                        return Ok((Stream::Tcp(BufReader::new(tcp)), peer));
                    }
                    Err(e) => last_err = Some(e),
                }
            }
            Err(last_err.unwrap_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no addresses resolved for {host}:{port}"),
                )
            }))
        }
        Endpoint::Unix(path) => {
            let sock = Socket::new(Domain::UNIX, Type::STREAM, None)?;
            sock.connect_timeout(&SockAddr::unix(path)?, CONNECT_TIMEOUT)?;
            let unix = UnixStream::from(OwnedFd::from(sock));
            unix.set_read_timeout(Some(IO_TIMEOUT))?;
            unix.set_write_timeout(Some(IO_TIMEOUT))?;
            Ok((Stream::Unix(BufReader::new(unix)), None))
        }
    }
}

// Stream request body directly to the FastCGI backend via VRB_Iterate, writing each
// chunk as a STDIN record as it arrives. Mirrors cache_http1_fetch.c's approach.
fn stream_body_to_backend(ctx: &mut Ctx, stream: &mut Stream, request_id: u16) -> io::Result<()> {
    use std::ffi::{c_int, c_uint, c_void};

    struct WriteCtx {
        stream: *mut Stream,
        request_id: u16,
        error: Option<io::Error>,
    }

    unsafe extern "C" fn callback(
        priv_: *mut c_void,
        _flush: c_uint,
        ptr: *const c_void,
        len: isize,
    ) -> c_int {
        let w = &mut *priv_.cast::<WriteCtx>();
        let data = std::slice::from_raw_parts(ptr.cast::<u8>(), len as usize);
        for chunk in data.chunks(0xffff) {
            if let Err(e) = proto::write_stdin_record(&mut *w.stream, w.request_id, chunk) {
                w.error = Some(e);
                return -1;
            }
        }
        0
    }

    unsafe {
        let Some(bo) = ctx.raw.bo.as_mut() else {
            return proto::write_stdin_record(stream, request_id, &[]);
        };
        let Some(req) = bo.req.as_mut() else {
            return proto::write_stdin_record(stream, request_id, &[]);
        };

        let mut wctx = WriteCtx {
            stream: stream as *mut Stream,
            request_id,
            error: None,
        };
        let p = (&raw mut wctx).cast::<c_void>();
        ffi::VRB_Iterate(bo.wrk, bo.vsl.as_mut_ptr(), req, Some(callback), p);
        if let Some(e) = wctx.error {
            return Err(e);
        }
    }

    proto::write_stdin_record(stream, request_id, &[])
}

fn sob_str(s: impl AsRef<[u8]>) -> String {
    String::from_utf8_lossy(s.as_ref()).into_owned()
}

fn has_cgi_separator(data: &[u8]) -> bool {
    data.windows(4).any(|w| w == b"\r\n\r\n") || data.windows(2).any(|w| w == b"\n\n")
}

// Split a `Host` header into (SERVER_NAME, SERVER_PORT). IPv6 literals are always
// bracket-quoted per RFC 7230, e.g. "[::1]:8080" or "[::1]".
fn split_host_port(host: &str) -> (String, String) {
    if let Some(rest) = host.strip_prefix('[') {
        if let Some((addr, rest)) = rest.split_once(']') {
            let port = rest.strip_prefix(':').unwrap_or("80");
            return (addr.to_string(), port.to_string());
        }
    }
    let name = host.split(':').next().unwrap_or("localhost").to_string();
    let port = host.split(':').nth(1).unwrap_or("80").to_string();
    (name, port)
}

impl VclBackend<FastCgiResponse> for FastCgiBackend {
    fn get_response(&self, ctx: &mut Ctx) -> Result<Option<FastCgiResponse>, VclError> {
        // Phase 1: collect bereq data into owned strings before touching beresp
        let params: Vec<(String, String)> = {
            let bereq = ctx
                .http_bereq
                .as_ref()
                .ok_or_else(|| VclError::new("bereq unavailable".to_string()))?;

            let method = sob_str(bereq.method().expect("bereq.method is always set"));
            let url = sob_str(bereq.url().expect("bereq.url is always set"));
            let proto = sob_str(bereq.proto().expect("bereq.proto is always set"));

            let (path, query) = url
                .split_once('?')
                .map(|(p, q)| (p.to_string(), q.to_string()))
                .unwrap_or_else(|| (url.clone(), String::new()));

            let script_filename = format!("{}{path}", self.docroot);

            let host = bereq
                .header("Host")
                .map(sob_str)
                .unwrap_or_else(|| "localhost".to_string());
            let (server_name, server_port) = split_host_port(&host);

            let xff = bereq
                .header("X-Forwarded-For")
                .map(sob_str)
                .ok_or_else(|| {
                    VclError::new(
                        "fastcgi: X-Forwarded-For header required for REMOTE_ADDR".to_string(),
                    )
                })?;
            // Varnish's core prepends/appends the real client address to this header before
            // vcl_recv ever runs, so the *last* hop is the one Varnish itself vouches for.
            // Earlier hops are attacker-controlled (anyone can send their own XFF value) and
            // must not be trusted as REMOTE_ADDR.
            let last_hop = xff.rsplit(',').next().unwrap_or("").trim();
            let remote_addr: IpAddr = last_hop.parse().map_err(|_| {
                VclError::new(format!(
                    "fastcgi: invalid X-Forwarded-For value '{last_hop}'"
                ))
            })?;

            let content_type = bereq
                .header("Content-Type")
                .map(sob_str)
                .unwrap_or_default();
            let content_length = bereq.header("Content-Length").map(sob_str);
            // A body is only expected on these methods (or when chunked); require an explicit
            // Content-Length in that case since we don't derive it from the cached body.
            let body_expected = matches!(method.as_str(), "POST" | "PUT" | "PATCH")
                || bereq.header("Transfer-Encoding").is_some();
            if content_length.is_none() && body_expected {
                return Err(VclError::new(format!(
                    "fastcgi: {method} request missing Content-Length header"
                )));
            }

            let mut params: Vec<(String, String)> = vec![
                ("GATEWAY_INTERFACE".into(), "FastCGI/1.0".into()),
                (
                    "SERVER_SOFTWARE".into(),
                    concat!("vmod_fastcgi/", env!("CARGO_PKG_VERSION")).into(),
                ),
                ("SERVER_PROTOCOL".into(), proto),
                ("REQUEST_METHOD".into(), method),
                ("REQUEST_URI".into(), url),
                ("SCRIPT_NAME".into(), path),
                ("SCRIPT_FILENAME".into(), script_filename),
                ("DOCUMENT_ROOT".into(), self.docroot.clone()),
                ("QUERY_STRING".into(), query),
                ("SERVER_NAME".into(), server_name),
                ("SERVER_PORT".into(), server_port),
                ("REMOTE_ADDR".into(), remote_addr.to_string()),
            ];

            if !content_type.is_empty() {
                params.push(("CONTENT_TYPE".into(), content_type));
            }
            if let Some(content_length) = content_length {
                params.push(("CONTENT_LENGTH".into(), content_length));
            }

            let skip = ["host", "content-type", "content-length"];
            for (name, value) in bereq.iter() {
                if skip.iter().any(|s| s.eq_ignore_ascii_case(name)) {
                    continue;
                }
                let cgi_name = format!("HTTP_{}", name.to_uppercase().replace('-', "_"));
                params.push((cgi_name, sob_str(value)));
            }

            params
        }; // bereq borrow released here

        // Phase 2: connect, send request, read CGI headers
        let (ip, status, resp_headers, body_prefix, stream, request_ended) = {
            let (mut stream, ip) = connect(&self.endpoint)
                .map_err(|e| VclError::new(format!("fastcgi connect: {e}")))?;

            let pairs: Vec<(&str, &str)> = params
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();

            proto::write_begin_request(&mut stream, REQUEST_ID)
                .map_err(|e| VclError::new(format!("fastcgi write begin: {e}")))?;
            proto::write_params(&mut stream, REQUEST_ID, &pairs)
                .map_err(|e| VclError::new(format!("fastcgi write params: {e}")))?;
            stream_body_to_backend(ctx, &mut stream, REQUEST_ID)
                .map_err(|e| VclError::new(format!("fastcgi write stdin: {e}")))?;
            stream
                .flush()
                .map_err(|e| VclError::new(format!("fastcgi flush: {e}")))?;

            // Read STDOUT records until we have the full CGI header section
            let mut stdout_buf: Vec<u8> = Vec::new();
            let mut request_ended = false;

            loop {
                let rec = proto::read_record(&mut stream)
                    .map_err(|e| VclError::new(format!("fastcgi read: {e}")))?;
                match rec.typ {
                    proto::STDOUT => {
                        if rec.data.is_empty() {
                            break; // end of stdout without finding separator
                        }
                        stdout_buf.extend_from_slice(&rec.data);
                        if stdout_buf.len() > MAX_RESPONSE_HEADER_BYTES {
                            return Err(VclError::new(format!(
                                "fastcgi: response headers exceeded {MAX_RESPONSE_HEADER_BYTES} bytes without terminator"
                            )));
                        }
                        if has_cgi_separator(&stdout_buf) {
                            break;
                        }
                    }
                    proto::STDERR => {}
                    proto::END_REQUEST => {
                        request_ended = true;
                        break;
                    }
                    _ => {}
                }
            }

            let (status, resp_headers, body_start) = proto::parse_cgi_response(&stdout_buf);
            let body_prefix = stdout_buf[body_start..].to_vec();

            (ip, status, resp_headers, body_prefix, stream, request_ended)
        };

        // Phase 3: set beresp headers
        let beresp = ctx
            .http_beresp
            .as_mut()
            .ok_or_else(|| VclError::new("beresp unavailable".to_string()))?;
        beresp.set_status(status);
        for (name, value) in &resp_headers {
            beresp
                .set_header(name, value)
                .map_err(|e| VclError::new(format!("fastcgi set header {name}: {e}")))?;
        }

        Ok(Some(FastCgiResponse {
            stream: if request_ended { None } else { Some(stream) },
            buf: body_prefix,
            buf_pos: 0,
            done: request_ended,
            ip,
        }))
    }
}

impl VclResponse for FastCgiResponse {
    fn read(&mut self, out: &mut [u8]) -> Result<usize, VclError> {
        if out.is_empty() {
            return Ok(0);
        }

        // Drain buffered bytes first (body prefix, or overflow from a large STDOUT record)
        if self.buf_pos < self.buf.len() {
            let avail = &self.buf[self.buf_pos..];
            let n = avail.len().min(out.len());
            out[..n].copy_from_slice(&avail[..n]);
            self.buf_pos += n;
            return Ok(n);
        }

        if self.done {
            return Ok(0);
        }

        // Pull more records from the stream
        loop {
            let stream = match self.stream.as_mut() {
                Some(s) => s,
                None => return Ok(0),
            };

            let rec = proto::read_record(stream)
                .map_err(|e| VclError::new(format!("fastcgi read body: {e}")))?;

            match rec.typ {
                proto::STDOUT => {
                    if rec.data.is_empty() {
                        // empty STDOUT = no more body; wait for END_REQUEST
                        continue;
                    }
                    let n = rec.data.len().min(out.len());
                    out[..n].copy_from_slice(&rec.data[..n]);
                    if n < rec.data.len() {
                        // record larger than caller's buffer; stash the rest
                        self.buf = rec.data[n..].to_vec();
                        self.buf_pos = 0;
                    }
                    return Ok(n);
                }
                proto::STDERR => continue,
                proto::END_REQUEST => {
                    self.done = true;
                    self.stream = None;
                    return Ok(0);
                }
                _ => continue,
            }
        }
    }

    fn get_ip(&self) -> Result<Option<SocketAddr>, VclError> {
        Ok(self.ip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_host_port_plain() {
        assert_eq!(
            split_host_port("example.com:8080"),
            ("example.com".to_string(), "8080".to_string())
        );
    }

    #[test]
    fn split_host_port_no_port_defaults_to_80() {
        assert_eq!(
            split_host_port("example.com"),
            ("example.com".to_string(), "80".to_string())
        );
    }

    #[test]
    fn split_host_port_ipv6_with_port() {
        assert_eq!(
            split_host_port("[::1]:8080"),
            ("::1".to_string(), "8080".to_string())
        );
    }

    #[test]
    fn split_host_port_ipv6_without_port() {
        assert_eq!(
            split_host_port("[::1]"),
            ("::1".to_string(), "80".to_string())
        );
    }

    #[test]
    fn has_cgi_separator_detects_both_forms() {
        assert!(has_cgi_separator(b"a\r\n\r\nb"));
        assert!(has_cgi_separator(b"a\n\nb"));
        assert!(!has_cgi_separator(b"a\r\nb"));
    }
}
