//! One loopback dashboard session with bounded, read-only HTTP requests.

use super::{CoverageCache, ReadState, read_review_queue, snapshot, view};
use anyhow::{Context, Result};
use std::{
    fs::OpenOptions,
    io::{self, Read, Write},
    net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_HEADERS: usize = 32;
const READ_LIMIT: Duration = Duration::from_millis(500);
const WRITE_LIMIT: Duration = Duration::from_millis(500);
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

#[derive(Debug, PartialEq, Eq)]
enum Route {
    Shell,
    Style,
    Script,
    Cards,
    Sources,
    Source(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rejection {
    BadRequest,
    Unauthorized,
    Forbidden,
    NotFound,
    Method,
    Timeout,
    HeadersTooLarge,
}

impl Rejection {
    fn status(self) -> &'static str {
        match self {
            Self::BadRequest => "400 Bad Request",
            Self::Unauthorized => "401 Unauthorized",
            Self::Forbidden => "403 Forbidden",
            Self::NotFound => "404 Not Found",
            Self::Method => "405 Method Not Allowed",
            Self::Timeout => "408 Request Timeout",
            Self::HeadersTooLarge => "431 Request Header Fields Too Large",
        }
    }
}

fn unique<'a>(
    headers: &[httparse::Header<'a>],
    name: &str,
) -> std::result::Result<Option<&'a [u8]>, Rejection> {
    let mut matching = headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case(name));
    let first = matching.next().map(|header| header.value);
    if matching.next().is_some() {
        return Err(Rejection::BadRequest);
    }
    Ok(first)
}

fn route(
    bytes: &[u8],
    authority: &str,
    origin: &str,
    access_code: &str,
) -> std::result::Result<Route, Rejection> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut request = httparse::Request::new(&mut headers);
    let parsed = request.parse(bytes).map_err(|error| match error {
        httparse::Error::TooManyHeaders => Rejection::HeadersTooLarge,
        _ => Rejection::BadRequest,
    })?;
    if parsed != httparse::Status::Complete(bytes.len()) || request.version != Some(1) {
        return Err(Rejection::BadRequest);
    }
    let headers = request.headers;
    let host = unique(headers, "Host")?;
    let supplied_origin = unique(headers, "Origin")?;
    if host != Some(authority.as_bytes())
        || supplied_origin.is_some_and(|value| value != origin.as_bytes())
    {
        return Err(Rejection::Forbidden);
    }
    let fetch_site = unique(headers, "Sec-Fetch-Site")?;
    if fetch_site.is_some_and(|value| value != b"same-origin" && value != b"none") {
        return Err(Rejection::Forbidden);
    }
    if request.method != Some("GET") {
        return Err(Rejection::Method);
    }
    if unique(headers, "Content-Length")?.is_some_and(|value| value != b"0")
        || unique(headers, "Transfer-Encoding")?.is_some()
        || unique(headers, "Expect")?.is_some()
        || unique(headers, "Upgrade")?.is_some()
    {
        return Err(Rejection::BadRequest);
    }
    match request.path {
        Some("/") => Ok(Route::Shell),
        Some("/dashboard.css") => Ok(Route::Style),
        Some("/dashboard.js") => Ok(Route::Script),
        Some(path) if path == "/cards" || path == "/sources" || path.starts_with("/sources/") => {
            if fetch_site.is_some_and(|value| value != b"same-origin") {
                return Err(Rejection::Forbidden);
            }
            let supplied =
                unique(headers, "Authorization")?.and_then(|value| value.strip_prefix(b"Bearer "));
            if !supplied.is_some_and(|value| bool::from(value.ct_eq(access_code.as_bytes()))) {
                return Err(Rejection::Unauthorized);
            }
            match path {
                "/cards" => Ok(Route::Cards),
                "/sources" => Ok(Route::Sources),
                _ => path
                    .strip_prefix("/sources/")
                    .filter(|id| id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
                    .map(|id| Route::Source(id.to_owned()))
                    .ok_or(Rejection::NotFound),
            }
        }
        _ => Err(Rejection::NotFound),
    }
}

fn read_headers<'a>(
    stream: &mut TcpStream,
    buffer: &'a mut [u8; MAX_HEADER_BYTES],
) -> std::result::Result<&'a [u8], Rejection> {
    let deadline = Instant::now() + READ_LIMIT;
    let mut used: usize = 0;
    while used < buffer.len() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Rejection::Timeout);
        }
        stream
            .set_read_timeout(Some(remaining.max(Duration::from_millis(1))))
            .map_err(|_| Rejection::BadRequest)?;
        let start = used.saturating_sub(3);
        match stream.read(&mut buffer[used..]) {
            Ok(0) => return Err(Rejection::BadRequest),
            Ok(count) => used += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                return Err(Rejection::Timeout);
            }
            Err(_) => return Err(Rejection::BadRequest),
        }
        if let Some(offset) = buffer[start..used]
            .windows(4)
            .position(|part| part == b"\r\n\r\n")
        {
            if start + offset + 4 != used {
                return Err(Rejection::BadRequest);
            }
            return Ok(&buffer[..used]);
        }
    }
    Err(Rejection::HeadersTooLarge)
}

fn write_response(
    stream: &mut TcpStream,
    status: &str,
    mime: &str,
    extra_header: &str,
    body: &str,
) -> io::Result<()> {
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {mime}; charset=utf-8\r\nContent-Length: {}\r\n\
         Connection: close\r\nCache-Control: no-store\r\nContent-Security-Policy: {CSP}\r\n\
         X-Content-Type-Options: nosniff\r\nCross-Origin-Resource-Policy: same-origin\r\n\
         Referrer-Policy: no-referrer\r\n{extra_header}\r\n",
        body.len()
    );
    let deadline = Instant::now() + WRITE_LIMIT;
    for mut bytes in [header.as_bytes(), body.as_bytes()] {
        while !bytes.is_empty() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            stream.set_write_timeout(Some(remaining.max(Duration::from_millis(1))))?;
            match stream.write(bytes) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(count) => bytes = &bytes[count..],
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

struct Session<'a> {
    authority: String,
    origin: String,
    access_code: String,
    requested: &'a Path,
    show_review_queue: bool,
    shell: String,
    coverage: CoverageCache,
}

impl Session<'_> {
    fn handle(&mut self, mut stream: TcpStream) -> io::Result<()> {
        let mut buffer = [0; MAX_HEADER_BYTES];
        let route = read_headers(&mut stream, &mut buffer)
            .and_then(|bytes| route(bytes, &self.authority, &self.origin, &self.access_code));
        let (mime, body, header) = match route {
            Ok(Route::Shell) => ("text/html", self.shell.as_str(), ""),
            Ok(Route::Style) => ("text/css", view::STYLE, ""),
            Ok(Route::Script) => ("text/javascript", view::SCRIPT, ""),
            Ok(route @ (Route::Sources | Route::Source(_))) => {
                let id = match route {
                    Route::Source(ref id) => Some(id.as_str()),
                    _ => None,
                };
                return match super::read_sources(self.requested, id) {
                    Ok(html) => write_response(&mut stream, "200 OK", "text/html", "", &html),
                    Err(_) => {
                        write_response(&mut stream, "503 Service Unavailable", "text/plain", "", "")
                    }
                };
            }
            Ok(Route::Cards) => {
                let observed = snapshot(self.requested, &mut self.coverage);
                let available = matches!(&observed.state, ReadState::Available);
                let header = if available {
                    "X-AnyRAG-Index-State: available\r\n"
                } else {
                    "X-AnyRAG-Index-State: unavailable\r\n"
                };
                let review = self.show_review_queue.then(|| {
                    anyhow::ensure!(available, "Review queue unavailable.");
                    read_review_queue(self.requested)
                });
                return write_response(
                    &mut stream,
                    "200 OK",
                    "text/html",
                    header,
                    &view::cards(&observed, review.as_ref()),
                );
            }
            Err(error) => {
                let header = match error {
                    Rejection::Method => "Allow: GET\r\n",
                    Rejection::Unauthorized => "WWW-Authenticate: Bearer\r\n",
                    _ => "",
                };
                return write_response(&mut stream, error.status(), "text/plain", header, "");
            }
        };
        write_response(&mut stream, "200 OK", mime, header, body)
    }
}

fn access_code() -> Result<String> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut random = [0_u8; 32];
    getrandom::fill(&mut random)
        .map_err(|_| anyhow::anyhow!("Could not create a secure dashboard session."))?;
    let mut code = String::with_capacity(64);
    for byte in random {
        code.push(char::from(HEX[usize::from(byte >> 4)]));
        code.push(char::from(HEX[usize::from(byte & 15)]));
    }
    Ok(code)
}

fn show_session(url: &str, code: &str) -> Result<()> {
    // The code goes directly to the controlling console, even when process
    // output is redirected. It is never part of stdout, stderr or the URL.
    let device = if cfg!(windows) { "CONOUT$" } else { "/dev/tty" };
    let mut terminal = OpenOptions::new()
        .write(true)
        .open(device)
        .context("Run dashboard from an interactive terminal for its local access code.")?;
    writeln!(
        terminal,
        "Dashboard: {url}\nDashboard access code: {code}\nPress Ctrl+C to stop the dashboard."
    )
    .context("Could not display the dashboard session in the terminal.")?;
    Ok(())
}

fn open_browser(url: &str) {
    #[cfg(windows)]
    let mut launcher = {
        let mut command = Command::new("rundll32.exe");
        command.arg("url.dll,FileProtocolHandler");
        command
    };
    #[cfg(target_os = "macos")]
    let mut launcher = Command::new("open");
    #[cfg(all(not(windows), not(target_os = "macos")))]
    let mut launcher = Command::new("xdg-open");
    let result = launcher
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = result {
        std::thread::spawn(move || {
            if !child.wait().is_ok_and(|status| status.success()) {
                eprintln!("Open the displayed dashboard URL in your browser.");
            }
        });
    } else {
        eprintln!("Open the displayed dashboard URL in your browser.");
    }
}

pub(super) fn serve(requested: &Path, open: bool, show_review_queue: bool) -> Result<()> {
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
        .context("Could not bind the local dashboard.")?;
    let authority = listener.local_addr()?.to_string();
    let origin = format!("http://{authority}");
    let access_code = access_code()?;
    let url = format!("{origin}/");
    show_session(&url, &access_code)?;
    if open {
        open_browser(&url);
    }
    let mut session = Session {
        authority,
        origin,
        access_code,
        requested,
        show_review_queue,
        shell: view::live_shell(show_review_queue),
        coverage: CoverageCache::default(),
    };
    loop {
        match listener.accept() {
            Ok((stream, address)) if address.ip().is_loopback() => {
                let _ = session.handle(stream);
            }
            Ok(_) => (),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => anyhow::bail!("The local dashboard listener stopped."),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cards_require_the_launch_code_and_exact_browser_origin() {
        let host = "127.0.0.1:32145";
        let origin = format!("http://{host}");
        let code = "c".repeat(64);
        let check = |method: &str, target: &str, headers: &str| {
            route(
                format!("{method} {target} HTTP/1.1\r\n{headers}\r\n").as_bytes(),
                host,
                &origin,
                &code,
            )
        };
        let canonical = format!("Host: {host}\r\n");
        let authorized = format!("{canonical}Authorization: Bearer {code}\r\n");
        assert_eq!(check("GET", "/", &canonical), Ok(Route::Shell));
        assert_eq!(check("GET", "/cards", &authorized), Ok(Route::Cards));
        assert_eq!(
            check("GET", "/cards", &canonical),
            Err(Rejection::Unauthorized)
        );
        assert_eq!(
            check("GET", "/cards", "Host: localhost:32145\r\n"),
            Err(Rejection::Forbidden)
        );
        assert_eq!(
            check(
                "GET",
                "/cards",
                &format!("{authorized}Origin: http://127.0.0.1:8888\r\n")
            ),
            Err(Rejection::Forbidden)
        );
        assert_eq!(
            check(
                "GET",
                "/cards",
                &format!("{authorized}Sec-Fetch-Site: same-site\r\n")
            ),
            Err(Rejection::Forbidden)
        );
        assert_eq!(
            check("GET", "/cards", &format!("{authorized}Host: {host}\r\n")),
            Err(Rejection::BadRequest)
        );
        assert_eq!(
            check(
                "GET",
                "/cards",
                &format!("{authorized}Authorization: Bearer invalid\r\n")
            ),
            Err(Rejection::BadRequest)
        );
        assert_eq!(check("POST", "/cards", &authorized), Err(Rejection::Method));
        assert_eq!(
            check("GET", "/cards?x=1", &authorized),
            Err(Rejection::NotFound)
        );
        assert_eq!(
            check(
                "GET",
                "/cards",
                &format!("{authorized}Transfer-Encoding: chunked\r\n")
            ),
            Err(Rejection::BadRequest)
        );
    }

    #[test]
    fn socket_read_has_an_absolute_header_deadline() {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        peer.write_all(b"GET / HTTP/1.1\r\nHost:").unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        let mut buffer = [0; MAX_HEADER_BYTES];
        let started = Instant::now();
        assert_eq!(
            read_headers(&mut stream, &mut buffer),
            Err(Rejection::Timeout)
        );
        assert!(started.elapsed() < READ_LIMIT * 5);
    }
}
