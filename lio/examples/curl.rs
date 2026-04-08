//! A very small `curl`-like example using plain HTTP over `lio`.
//!
//! Usage:
//! - `cargo run --example curl -- http://example.com/`

use lio::api::resource::Resource;
use lio::{Lio, api};
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};

fn main() -> io::Result<()> {
  let args: Vec<String> = std::env::args().skip(1).collect();
  let lio = Lio::new(64)?;
  if args.iter().any(|arg| arg == "--help" || arg == "-h") {
    print_help(&lio)?;
    return Ok(());
  }
  let verbose = args.iter().any(|arg| arg == "-v" || arg == "--verbose");
  let url = args
    .iter()
    .find(|arg| !arg.starts_with('-'))
    .cloned()
    .unwrap_or_else(|| {
      eprintln!("Usage: curl <http://host[:port][/path]> [-v|--verbose]");
    std::process::exit(1);
  });

  let request = HttpRequest::parse(&url)?;
  let addr = resolve_first(&request.host, request.port)?;

  let socket = run(
    &lio,
    api::socket(api::SockDomain::IPV4, api::SockType::STREAM, api::SockProto::TCP)
      .with_lio(&lio)
      .send(),
  )?;

  run(&lio, api::connect(&socket, addr).with_lio(&lio).send())?;

  let request_bytes = format!(
    "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nUser-Agent: lio-example-curl\r\nAccept: */*\r\n\r\n",
    request.path, request.host
  )
  .into_bytes();
  write_all(&lio, &socket, request_bytes)?;

  let mut buf = vec![0u8; 16 * 1024];
  let mut response = Vec::new();
  loop {
    let ((result, returned_buf), ()) = (run(&lio, api::recv(&socket, buf, None).with_lio(&lio).send()), ());
    buf = returned_buf;
    let n = result? as usize;
    if n == 0 {
      break;
    }
    response.extend_from_slice(&buf[..n]);
  }

  let stdout = Resource::stdout();
  let response = parse_http_response(response)?;
  if verbose {
    let mut head = format!(
      "{} {} {}\r\n",
      response.version, response.status, response.reason
    )
    .into_bytes();
    for (name, value) in &response.headers {
      head.extend_from_slice(name.as_bytes());
      head.extend_from_slice(b": ");
      head.extend_from_slice(value.as_bytes());
      head.extend_from_slice(b"\r\n");
    }
    head.extend_from_slice(b"\r\n");
    write_all_stdout(&lio, &stdout, head)?;
  }
  write_all_stdout(&lio, &stdout, response.body)?;

  Ok(())
}

struct HttpRequest {
  host: String,
  port: u16,
  path: String,
}

struct HttpResponse {
  version: String,
  status: u16,
  reason: String,
  headers: Vec<(String, String)>,
  body: Vec<u8>,
}

impl HttpRequest {
  fn parse(url: &str) -> io::Result<Self> {
    let rest = url.strip_prefix("http://").ok_or_else(|| {
      io::Error::new(
        io::ErrorKind::InvalidInput,
        "only plain http:// URLs are supported",
      )
    })?;

    let (authority, path) = match rest.split_once('/') {
      Some((authority, path)) => (authority, format!("/{}", path)),
      None => (rest, "/".to_string()),
    };

    if authority.is_empty() {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "URL host must not be empty",
      ));
    }

    let (host, port) = match authority.rsplit_once(':') {
      Some((host, port_str)) if !host.is_empty() && !port_str.is_empty() => {
        let port = port_str.parse::<u16>().map_err(|_| {
          io::Error::new(io::ErrorKind::InvalidInput, "invalid URL port")
        })?;
        (host.to_string(), port)
      }
      _ => (authority.to_string(), 80),
    };

    Ok(Self { host, port, path })
  }
}

fn resolve_first(host: &str, port: u16) -> io::Result<SocketAddr> {
  (host, port)
    .to_socket_addrs()?
    .find(|addr| addr.is_ipv4())
    .ok_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "no IPv4 address found"))
}

fn print_help(lio: &Lio) -> io::Result<()> {
  write_all_stdout(
    lio,
    &Resource::stdout(),
    b"\
curl - very small lio HTTP example

Usage:
  curl <http://host[:port][/path]> [options]

Supported options:
  -v, --verbose    Print the parsed HTTP status line and headers before the body
  -h, --help       Show this help message

Notes:
  - Only plain http:// URLs are supported
  - HTTPS is not supported
  - The example resolves and connects to the first IPv4 address
  - Chunked transfer encoding is decoded

Examples:
  curl http://example.com/
  curl http://example.com/ -v
  curl http://example.com:8080/path
"
    .to_vec(),
  )
}

fn parse_http_response(response: Vec<u8>) -> io::Result<HttpResponse> {
  let header_end = response
    .windows(4)
    .position(|window| window == b"\r\n\r\n")
    .map(|idx| idx + 4)
    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed HTTP response"))?;

  let headers = &response[..header_end - 4];
  let body = &response[header_end..];

  let header_text = std::str::from_utf8(headers)
    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

  let mut lines = header_text.split("\r\n");
  let status_line = lines
    .next()
    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing status line"))?;
  let mut status_parts = status_line.splitn(3, ' ');
  let version = status_parts
    .next()
    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing HTTP version"))?
    .to_string();
  let status = status_parts
    .next()
    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing HTTP status"))?
    .parse::<u16>()
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid HTTP status"))?;
  let reason = status_parts.next().unwrap_or("").to_string();

  let mut parsed_headers = Vec::new();
  let mut is_chunked = false;
  for line in lines {
    if line.is_empty() {
      continue;
    }
    let (name, value) = line.split_once(':').ok_or_else(|| {
      io::Error::new(io::ErrorKind::InvalidData, "malformed HTTP header")
    })?;
    let name = name.trim().to_string();
    let value = value.trim().to_string();
    if name.eq_ignore_ascii_case("transfer-encoding")
      && value.to_ascii_lowercase().contains("chunked")
    {
      is_chunked = true;
    }
    parsed_headers.push((name, value));
  }

  let body = if is_chunked { decode_chunked_body(body)? } else { body.to_vec() };

  Ok(HttpResponse { version, status, reason, headers: parsed_headers, body })
}

fn decode_chunked_body(mut body: &[u8]) -> io::Result<Vec<u8>> {
  let mut decoded = Vec::new();

  loop {
    let line_end = body
      .windows(2)
      .position(|window| window == b"\r\n")
      .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed chunk header"))?;

    let size_line = std::str::from_utf8(&body[..line_end])
      .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    let size_str = size_line.split(';').next().unwrap_or("").trim();
    let size = usize::from_str_radix(size_str, 16)
      .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid chunk size"))?;

    body = &body[line_end + 2..];

    if size == 0 {
      return Ok(decoded);
    }

    if body.len() < size + 2 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "truncated chunk body",
      ));
    }

    decoded.extend_from_slice(&body[..size]);

    if &body[size..size + 2] != b"\r\n" {
      return Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "missing chunk terminator",
      ));
    }

    body = &body[size + 2..];
  }
}

fn write_all(lio: &Lio, fd: &Resource, buf: Vec<u8>) -> io::Result<()> {
  let mut written = 0usize;
  while written < buf.len() {
    let (result, _) = run(lio, api::send(fd, buf[written..].to_vec(), None).with_lio(lio).send());
    let n = result? as usize;
    if n == 0 {
      return Err(io::Error::new(
        io::ErrorKind::WriteZero,
        "send returned zero before request/response write completed",
      ));
    }
    written += n;
  }
  Ok(())
}

fn write_all_stdout(lio: &Lio, fd: &Resource, buf: Vec<u8>) -> io::Result<()> {
  let mut written = 0usize;
  while written < buf.len() {
    let ((result, _), ()) =
      (run(lio, api::write(fd, buf[written..].to_vec()).with_lio(lio).send()), ());
    let n = result? as usize;
    if n == 0 {
      return Err(io::Error::new(
        io::ErrorKind::WriteZero,
        "write returned zero before stdout write completed",
      ));
    }
    written += n;
  }
  Ok(())
}

fn run<T>(lio: &Lio, mut rx: api::Receiver<T>) -> T {
  loop {
    if let Some(result) = rx.try_recv() {
      return result;
    }
    if lio.try_run().expect("lio.try_run()") == 0 {
      lio.run().expect("lio.run()");
    }
  }
}
