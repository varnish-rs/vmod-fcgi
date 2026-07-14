/*
 * Serves phpMyAdmin through this vmod, over both plain HTTP and native TLS
 * (self-signed cert), alongside vmod_fileserver for static assets (this vmod
 * only speaks FastCGI, no static-file serving).
 *
 * Prerequisites (paths below assume a typical Arch install):
 * - php-fpm running, listening on unix:/run/php-fpm/php-fpm.sock
 * - phpMyAdmin installed at /usr/share/webapps/phpMyAdmin
 * - vmod_fileserver built (https://github.com/varnish-rs/vmod_fileserver),
 *   adjust the import path below
 * - a MariaDB/MySQL server phpMyAdmin's own config.inc.php points at
 * - for the TLS listener: a `-A` TLS config file passed to varnishd, e.g.:
 *
 *     frontend = { host = "127.0.0.1" port = "8443" }
 *     pem-file = "/path/to/cert+key.pem"
 *
 *   generate a self-signed cert+key with:
 *     openssl req -x509 -newkey rsa:2048 -nodes -keyout key.pem -out cert.pem \
 *       -days 365 -subj "/CN=127.0.0.1"
 *     cat cert.pem key.pem > combined.pem
 *
 * Run with both listeners, e.g.:
 *   varnishd -F -n /tmp/foo_varnish -a 127.0.0.1:8088 -A tls.cfg -f phpmyadmin.vcl
 *
 * Adjust the two `import ... from` paths below to your own build output.
 */
vcl 4.1;

import fastcgi from "/home/guillaume/work/vmod-fastcgi/target/debug/libvmod_fastcgi.so";
import fileserver from "/home/guillaume/work/vmod_fileserver/target/debug/libvmod_fileserver.so";
import tls;

backend default none;

sub vcl_init {
    new pma = fastcgi.new("unix:/run/php-fpm/php-fpm.sock", "/usr/share/webapps");
    new pma_files = fileserver.root("/usr/share/webapps");
}

sub vcl_recv {
    if (req.url !~ "^/phpmyadmin(/|\?|$)") {
        return (synth(403, "Forbidden"));
    }

    # Reflect the *actual* client connection, not a hardcoded guess: this listener
    # also serves plain HTTP on :8088, so hardcoding "https" here would be wrong
    # for that listener the same way it was wrong before TLS was ever configured.
    if (tls.is_tls()) {
        set req.http.X-Forwarded-Proto = "https";
    } else {
        set req.http.X-Forwarded-Proto = "http";
    }
    return (pass);
}

sub vcl_hash {
    # Keep HTTP and HTTPS responses separate in cache -- falls through to the
    # builtin vcl_hash for the usual req.url/req.http.host hashing.
    hash_data(req.http.X-Forwarded-Proto);
}

sub vcl_backend_fetch {
    # "/phpmyadmin" and "/phpmyadmin/" both mean index.php. Only affects what the
    # backend receives (REQUEST_URI/SCRIPT_NAME), so it belongs on bereq.url, not
    # req.url -- leaves the client-facing/hashed URL alone.
    if (bereq.url == "/phpmyadmin" || bereq.url == "/phpmyadmin/") {
        set bereq.url = "/phpmyadmin/index.php";
    }

    if (bereq.url ~ "\.php(\?|$)") {
        # Do NOT rewrite bereq.url's case here. It flows into REQUEST_URI/SCRIPT_NAME,
        # which phpMyAdmin uses to derive its own root path -- and thus its cookie
        # Set-Cookie path. Rewrite that and the browser (on the real, lowercase
        # /phpmyadmin/ URL) will never send the session cookie back: permanent
        # login loop, not a credentials problem. The on-disk case mismatch is
        # instead fixed below via set_parameter() -- which only touches
        # SCRIPT_FILENAME, not what the client sees reflected back.
        set bereq.backend = pma.backend();
        fastcgi.set_parameter(
            "SCRIPT_FILENAME",
            regsub(bereq.url, "^/phpmyadmin([^?]*).*", "/usr/share/webapps/phpMyAdmin\1")
        );
    } else {
        # On-disk directory is "phpMyAdmin" (mixed case); the public URL prefix is
        # lowercase. vmod_fileserver has no override hook like set_parameter() --
        # it reads bereq.url directly to build the on-disk path -- so fix the case
        # here, before the fileserver backend runs. Harmless for static assets:
        # they don't set cookies or echo their own URL. (vmod_fileserver strips
        # the query string itself when resolving a file on disk.)
        set bereq.url = regsub(bereq.url, "^/phpmyadmin", "/phpMyAdmin");
        set bereq.backend = pma_files.backend();
    }

    # tls.is_tls() isn't callable here (backend context), so carry the vcl_recv
    # determination forward via the header already copied onto bereq.
    if (bereq.http.X-Forwarded-Proto == "https") {
        fastcgi.set_parameter("HTTPS", "on");
    }
}
