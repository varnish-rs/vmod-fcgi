# vmod_fastcgi

[![CI](https://github.com/varnish-rs/vmod-fcgi/actions/workflows/ci.yml/badge.svg)](https://github.com/varnish-rs/vmod-fcgi/actions/workflows/ci.yml)

A Varnish VMOD that lets Varnish speak directly to a FastCGI application server
(PHP-FPM and similar) as a backend, without an intermediate HTTP server.

## Building

Requires a Rust toolchain and the [`varnish`](https://github.com/varnish-rs/varnish-rs)
crate's build dependencies (a Varnish installation with headers, matching the
version the crate targets).

```sh
cargo build --release
```

The resulting shared library is at `target/release/libvmod_fastcgi.so`.

## Usage

```vcl
vcl 4.1;

import fastcgi;
# or, without installing to the standard vmod path:
# import fastcgi from "/path/to/libvmod_fastcgi.so";

backend default none;

sub vcl_init {
    # TCP endpoint:
    new php = fastcgi.new("127.0.0.1:9000", "/var/www/html");

    # or a Unix domain socket:
    # new php = fastcgi.new("unix:/run/php/php-fpm.sock", "/var/www/html");

    # or a hostname (resolved per-request, so DNS changes are picked up):
    # new php = fastcgi.new("php-fpm.internal:9000", "/var/www/html");
}

sub vcl_recv {
    set req.backend_hint = php.backend();
}
```

The second argument to `fastcgi.new()` is the docroot: it's prepended to the
URL path to build `SCRIPT_FILENAME` (e.g. PHP-FPM needs this to find the
script to execute).

### Extra FastCGI params

`fastcgi.set_parameter(name, value)` adds an extra name/value pair to send as
a FastCGI PARAMS entry, on top of the usual CGI ones. Only callable from
backend-side subs (e.g. `vcl_backend_fetch`) — it's a VCL compile error
otherwise. Can be called multiple times; pairs are sent in call order, after
the built-in params, so a pair here can override a same-named built-in one.
It's request-scoped, not tied to a specific `fastcgi.new()` object — it
applies to whichever backend ends up handling the fetch.

```vcl
sub vcl_backend_fetch {
    fastcgi.set_parameter("REMOTE_USER", "some-authenticated-user");
}
```

### Requirements

- **`X-Forwarded-For` must resolve to a real client IP by the time the backend
  fetch runs.** Varnish's core sets this header from the real client address
  before any VCL executes, so this is satisfied by default — no VCL changes
  needed in the common case. The vmod reads the *last* comma-separated entry
  (the hop Varnish itself appended) as `REMOTE_ADDR`, since earlier entries
  are attacker-suppliable. If the header is missing entirely (e.g. explicitly
  `unset` in custom VCL), the fetch fails rather than reporting a fake
  `REMOTE_ADDR`.
- **`Content-Length` is required on requests that carry a body** (`POST`,
  `PUT`, `PATCH`, or any request with `Transfer-Encoding` set). The vmod does
  not attempt to derive a length from the body itself, and fails the fetch if
  it's missing on such a request.

### Known gaps

- **Chunked request bodies (`Transfer-Encoding: chunked`) are not supported.**
  A chunked POST/PUT/PATCH has no upfront `Content-Length`, and unlike a
  bodyless request (where Varnish sets `Content-Length: 0` itself), Varnish
  does not surface a computed length to the backend fetch for a real chunked
  body. Since this vmod won't guess, every chunked request fails the fetch
  (503). Clients that need to POST to a backend through this vmod must send
  an explicit `Content-Length`.

- `PATH_INFO` is not sent. Computing it correctly requires probing the
  filesystem to find where the real script path ends and the "extra" path
  segments begin — real complexity for a CGI feature mostly used by
  old-style multi-script apps. Modern front-controller frameworks route off
  `REQUEST_URI`/`SCRIPT_NAME` instead.
- `REMOTE_PORT` is not sent — there's no reliable source for it once
  `REMOTE_ADDR` comes from `X-Forwarded-For`, which doesn't carry a port.

## Testing

```sh
cargo test
```

Runs unit tests for the FastCGI record/param encoding (`src/proto.rs`) plus a
set of `.vtc` integration tests (`tests/test*.vtc`, one scenario per file)
that drive the vmod through a real `varnishd`, using
[`fcgiwrap`](https://github.com/gnosek/fcgiwrap) (a generic FastCGI↔CGI
bridge) as the backend. Requires `fcgiwrap` to be
installed (`pacman -S fcgiwrap` / `apt install fcgiwrap` / `dnf install
fcgiwrap`). New test scenarios are added as plain CGI scripts under
`${tmpdir}` inside the `.vtc` — any language, no FastCGI protocol code
needed; `fcgiwrap` handles the wire framing.
