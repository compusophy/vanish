//! dev-only static file server. std only — no crates, no node, no python.
//!
//! it exists because wasm modules and workers must be served over http with
//! correct mime types; opening index.html from the filesystem fails on both
//! counts. it is never deployed: the built site is plain static files.
//!
//! run with:  cargo run --features devserver --bin serve

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};

const ROOT: &str = "web";

fn mime_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        // serving wasm as anything else makes instantiateStreaming refuse it
        Some("wasm") => "application/wasm",
        // es module imports fail outright under the wrong js mime type
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    }
}

/// resolve a request path inside ROOT, refusing anything that climbs out.
fn resolve(request_path: &str) -> Option<PathBuf> {
    let trimmed = request_path.split('?').next().unwrap_or("/");
    let relative = trimmed.trim_start_matches('/');
    let relative = if relative.is_empty() {
        "index.html"
    } else {
        relative
    };

    let mut out = PathBuf::from(ROOT);
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(c) => out.push(c),
            // no traversal, no absolute paths, no drive prefixes
            _ => return None,
        }
    }
    Some(out)
}

fn respond(stream: &mut TcpStream, status: &str, mime: &str, body: &[u8]) {
    let header = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {mime}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn handle(mut stream: TcpStream) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }

    // drain headers so the client does not see a reset before our response
    let mut header = String::new();
    while reader.read_line(&mut header).unwrap_or(0) > 2 {
        header.clear();
    }

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("/");

    if method != "GET" {
        respond(&mut stream, "405 Method Not Allowed", "text/plain", b"get only");
        return;
    }

    let Some(resolved) = resolve(path) else {
        respond(&mut stream, "403 Forbidden", "text/plain", b"forbidden");
        return;
    };

    match fs::File::open(&resolved) {
        Ok(mut f) => {
            let mut body = Vec::new();
            if f.read_to_end(&mut body).is_err() {
                respond(&mut stream, "500 Internal Server Error", "text/plain", b"read error");
                return;
            }
            println!("  200 {path}");
            respond(&mut stream, "200 OK", mime_for(&resolved), &body);
        }
        Err(_) => {
            println!("  404 {path}");
            respond(&mut stream, "404 Not Found", "text/plain", b"not found");
        }
    }
}

fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(8787);

    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("could not bind port {port}: {e}");
            std::process::exit(1);
        }
    };

    println!("vanish dev server -> http://localhost:{port}");
    println!("serving ./{ROOT}  (ctrl-c to stop)");

    for stream in listener.incoming().flatten() {
        // one connection at a time is plenty for a local dev server, and
        // keeps this file dependency-free.
        handle(stream);
    }
}
