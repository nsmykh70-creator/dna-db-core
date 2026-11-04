// SPDX-License-Identifier: AGPL-3.0-only

//! Interactive client shell (`shell` subcommand): TLS/plain TCP stream
//! framing and the SQL/DnaQL read-eval-print loop.

use std::io::{BufRead, Read, Write};

pub(crate) enum ShellStream {
    Plain(std::net::TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>>),
}

impl Read for ShellStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            ShellStream::Plain(stream) => stream.read(buf),
            ShellStream::Tls(stream) => stream.read(buf),
        }
    }
}

impl Write for ShellStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            ShellStream::Plain(stream) => stream.write(buf),
            ShellStream::Tls(stream) => stream.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            ShellStream::Plain(stream) => stream.flush(),
            ShellStream::Tls(stream) => stream.flush(),
        }
    }
}

pub(crate) fn shell_connect(
    host: &str,
    port: u16,
    no_tls: bool,
    tls_ca: Option<&str>,
    tls_client_cert: Option<&str>,
    tls_client_key: Option<&str>,
) -> std::io::Result<ShellStream> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};

    let tcp = std::net::TcpStream::connect((host, port))?;
    tcp.set_nodelay(true).ok();
    if no_tls {
        return Ok(ShellStream::Plain(tcp));
    }

    let mut roots = rustls::RootCertStore::empty();
    if let Some(path) = tls_ca {
        let cert = CertificateDer::from_pem_file(path)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
        roots
            .add(cert)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
    } else {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }
    let builder = rustls::ClientConfig::builder().with_root_certificates(roots);
    let config = match (tls_client_cert, tls_client_key) {
        (Some(cert_path), Some(key_path)) => {
            let certs = CertificateDer::pem_file_iter(cert_path)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string())
                })?;
            let key = PrivateKeyDer::from_pem_file(key_path).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string())
            })?;
            builder
                .with_client_auth_cert(certs, key)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?
        }
        (None, None) => builder.with_no_client_auth(),
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "--tls-client-cert and --tls-client-key must be supplied together",
            ))
        }
    };
    let server_name = match host.parse::<std::net::IpAddr>() {
        Ok(ip) => ServerName::IpAddress(ip.into()),
        Err(_) => ServerName::try_from(host.to_string())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?,
    };
    let connection = rustls::ClientConnection::new(std::sync::Arc::new(config), server_name)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    Ok(ShellStream::Tls(Box::new(rustls::StreamOwned::new(
        connection, tcp,
    ))))
}

pub(crate) fn run_shell(
    host: &str,
    port: u16,
    user: &str,
    pass: &str,
    no_tls: bool,
    tls_ca: Option<&str>,
    tls_client_cert: Option<&str>,
    tls_client_key: Option<&str>,
) -> i32 {
    let mut stream =
        match shell_connect(host, port, no_tls, tls_ca, tls_client_cert, tls_client_key) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("cannot connect to {}:{}: {}", host, port, e);
                return 1;
            }
        };

    // Auth
    let msg = format!("\x01{}:{}", user, pass);
    if send_frame(&mut stream, msg.as_bytes()).is_err() {
        return 1;
    }
    let mut ok = [0u8; 2];
    if stream.read_exact(&mut ok).is_err() || &ok != b"OK" {
        eprintln!("authentication failed");
        return 1;
    }
    println!(
        "Connected to {}:{} as {}{} (type SQL or DnaQL; .quit to exit)",
        host,
        port,
        user,
        if no_tls { " [plaintext]" } else { " [TLS]" }
    );

    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        print!("dna-db> ");
        std::io::stdout().flush().ok();
        line.clear();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let q = line.trim();
        if q.is_empty() {
            continue;
        }
        if q == ".quit" || q == ".exit" {
            break;
        }
        if q == ".status" {
            if send_frame(&mut stream, b"\x00").is_err() {
                break;
            }
            match read_frame_resp(&mut stream) {
                Ok(Some(resp)) => println!("{}", String::from_utf8_lossy(&resp)),
                _ => break,
            }
            continue;
        }

        let upper = q.to_uppercase();
        let opcode: u8 = if upper.starts_with("SELECT")
            || upper.starts_with("INSERT")
            || upper.starts_with("UPDATE")
            || upper.starts_with("DELETE")
            || upper.starts_with("CREATE")
            || upper.starts_with("DROP")
        {
            0x11
        } else {
            0x10
        };
        let payload = format!("{}{}", opcode as char, q);
        if send_frame(&mut stream, payload.as_bytes()).is_err() {
            eprintln!("connection lost");
            break;
        }
        match read_frame_resp(&mut stream) {
            Ok(Some(resp)) => println!("{}", String::from_utf8_lossy(&resp)),
            Ok(None) => break,
            Err(e) => {
                eprintln!("recv error: {}", e);
                break;
            }
        }
    }
    0
}

pub(crate) fn send_frame<W: Write + ?Sized>(stream: &mut W, payload: &[u8]) -> std::io::Result<()> {
    stream.write_all(&(payload.len() as u32).to_le_bytes())?;
    stream.write_all(payload)
}

pub(crate) fn read_frame_resp<R: Read + ?Sized>(stream: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    Ok(Some(payload))
}
