use std::path::PathBuf;

varnish::run_vtc_tests!("tests/*.vtc");

mod backend;
mod proto;

use backend::{Endpoint, FastCgiBackend, FastCgiResponse};
use varnish::vcl::Backend;

struct Server {
    backend: Backend<FastCgiBackend, FastCgiResponse>,
}

#[varnish::vmod]
mod fastcgi {
    use varnish::ffi::VCL_BACKEND;
    use varnish::vcl::{Backend, Ctx, VclError};

    use super::{FastCgiBackend, Server};
    use crate::parse_endpoint;

    impl Server {
        /// Create a FastCGI backend.
        ///
        /// `endpoint` is either `"host:port"` for TCP or `"unix:/path/to/socket"` for
        /// a Unix domain socket.  `docroot` is prepended to the URL path to form
        /// `SCRIPT_FILENAME` (needed by PHP-FPM).
        pub fn new(
            ctx: &mut Ctx,
            #[vcl_name] name: &str,
            endpoint: &str,
            docroot: &str,
        ) -> Result<Self, VclError> {
            let ep = parse_endpoint(endpoint)
                .map_err(|e| VclError::new(format!("fastcgi.server: {e}")))?;

            let be = FastCgiBackend {
                endpoint: ep,
                docroot: docroot.into(),
            };

            let backend = Backend::new(ctx, "fastcgi", name, be, false)?;
            Ok(Server { backend })
        }

        /// Return the VCL_BACKEND handle for use in `set bereq.backend`.
        pub unsafe fn backend(&self) -> VCL_BACKEND {
            self.backend.as_ref().vcl_ptr()
        }
    }

    /// Add an extra name/value pair to send as a FastCGI PARAMS entry for the current
    /// backend fetch, in addition to the usual CGI params. Can be called multiple times;
    /// pairs are sent in call order, after the built-in params (so a pair here can
    /// override a same-named built-in one). Request-scoped: applies to whichever fastcgi
    /// backend ends up handling this fetch, not tied to a specific `fastcgi.new()` object.
    #[restrict(backend)]
    pub fn set_parameter(ctx: &mut Ctx, name: &str, value: &str) {
        crate::backend::add_extra_param(ctx, name, value);
    }
}

fn parse_endpoint(s: &str) -> Result<Endpoint, String> {
    if let Some(path) = s.strip_prefix("unix:") {
        return Ok(Endpoint::Unix(PathBuf::from(path)));
    }

    // Bracketed form for IPv6/hostname literals, e.g. "[::1]:9000".
    let (host, port) = if let Some(rest) = s.strip_prefix('[') {
        let (host, rest) = rest
            .split_once(']')
            .ok_or_else(|| format!("cannot parse '{s}' as host:port — unterminated '['"))?;
        let port = rest
            .strip_prefix(':')
            .ok_or_else(|| format!("cannot parse '{s}' as host:port — missing port after ']'"))?;
        (host, port)
    } else {
        s.rsplit_once(':')
            .ok_or_else(|| format!("cannot parse '{s}' as host:port or unix:path"))?
    };

    if host.is_empty() {
        return Err(format!("cannot parse '{s}' as host:port — empty host"));
    }
    let port: u16 = port
        .parse()
        .map_err(|e| format!("cannot parse '{s}' as host:port — invalid port: {e}"))?;

    Ok(Endpoint::Tcp {
        host: host.to_string(),
        port,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_unix_socket() {
        match parse_endpoint("unix:/var/run/fpm.sock").unwrap() {
            Endpoint::Unix(path) => assert_eq!(path, PathBuf::from("/var/run/fpm.sock")),
            Endpoint::Tcp { .. } => panic!("expected Unix endpoint"),
        }
    }

    #[test]
    fn parses_hostname_and_port() {
        match parse_endpoint("backend.local:9000").unwrap() {
            Endpoint::Tcp { host, port } => {
                assert_eq!(host, "backend.local");
                assert_eq!(port, 9000);
            }
            Endpoint::Unix(_) => panic!("expected Tcp endpoint"),
        }
    }

    #[test]
    fn parses_ipv4_and_port() {
        match parse_endpoint("127.0.0.1:9000").unwrap() {
            Endpoint::Tcp { host, port } => {
                assert_eq!(host, "127.0.0.1");
                assert_eq!(port, 9000);
            }
            Endpoint::Unix(_) => panic!("expected Tcp endpoint"),
        }
    }

    #[test]
    fn parses_bracketed_ipv6_and_port() {
        match parse_endpoint("[::1]:9000").unwrap() {
            Endpoint::Tcp { host, port } => {
                assert_eq!(host, "::1");
                assert_eq!(port, 9000);
            }
            Endpoint::Unix(_) => panic!("expected Tcp endpoint"),
        }
    }

    #[test]
    fn rejects_missing_port() {
        assert!(parse_endpoint("backend.local").is_err());
    }

    #[test]
    fn rejects_non_numeric_port() {
        assert!(parse_endpoint("backend.local:http").is_err());
    }

    #[test]
    fn rejects_empty_host() {
        assert!(parse_endpoint(":9000").is_err());
    }

    #[test]
    fn rejects_unterminated_bracket() {
        assert!(parse_endpoint("[::1:9000").is_err());
    }
}
