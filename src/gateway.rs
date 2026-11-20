use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    net::TcpStream,
    path::PathBuf,
    sync::mpsc::Sender,
    time::{Duration, Instant},
};
use tungstenite::{Message, WebSocket};

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub params: Value,
    pub epoch: u64,
    pub serial: u64,
}
impl Request {
    fn background(method: &str, params: Value) -> Self {
        Self {
            method: method.into(),
            params,
            epoch: 0,
            serial: 0,
        }
    }
}

pub enum Update {
    Status(String),
    Sessions(Value),
    Usage(Value),
    Error(String),
    Reply(String, Value),
    Response(Request, Value, bool),
    RequestFailed(Request),
    Event(Value),
}
type Socket = WebSocket<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}
fn identity() -> Result<SigningKey> {
    use std::os::unix::fs::OpenOptionsExt;
    let home = std::env::var("HOME")?;
    let dir = PathBuf::from(home).join(".config/veyterm");
    fs::create_dir_all(&dir)?;
    let path = dir.join("device.key");
    if !path.exists() {
        let mut seed = [0u8; 32];
        fs::File::open("/dev/urandom")?.read_exact(&mut seed)?;
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(&seed)?;
                file.sync_all()?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.into()),
        }
    }
    let seed: [u8; 32] = fs::read(path)?
        .try_into()
        .map_err(|_| "invalid veyterm device key")?;
    Ok(SigningKey::from_bytes(&seed))
}
fn receive(socket: &mut Socket) -> Result<Value> {
    loop {
        match socket.read()? {
            Message::Text(text) => return Ok(serde_json::from_str(&text)?),
            Message::Ping(_) => socket.flush()?,
            Message::Close(_) => return Err("Gateway closed the connection".into()),
            _ => (),
        }
    }
}
fn rpc(socket: &mut Socket, method: &str, params: Value, id: &mut u64) -> Result<Value> {
    *id += 1;
    let request_id = id.to_string();
    socket.send(Message::Text(
        json!({"type":"req","id":request_id,"method":method,"params":params})
            .to_string()
            .into(),
    ))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if Instant::now() > deadline {
            return Err(format!("{method}: request timed out").into());
        }
        let response = receive(socket)?;
        if response["type"] == "res" && response["id"] == request_id {
            if response["ok"] == true {
                return Ok(response["payload"].clone());
            }
            return Err(format!("{method}: {}", response["error"]).into());
        }
    }
}
fn connect(
    endpoint: &str,
    pin: Option<&str>,
    insecure: bool,
    token: &str,
) -> Result<(Socket, Value)> {
    use std::net::ToSocketAddrs;
    let url = url::Url::parse(endpoint)?;
    if url.scheme() != "wss"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("Use wss://host:port (no credentials/query in URL)".into());
    }
    let host = url.host_str().ok_or("missing host")?;
    let port = url.port_or_known_default().ok_or("missing port")?;
    let addresses = (host, port).to_socket_addrs()?;
    let mut tcp = None;
    for address in addresses {
        if let Ok(s) = TcpStream::connect_timeout(&address, Duration::from_secs(8)) {
            tcp = Some(s);
            break;
        }
    }
    let tcp = tcp.ok_or("Unable to reach Gateway")?;
    tcp.set_read_timeout(Some(Duration::from_secs(30)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(10)))?;
    let config = tls_config(pin, insecure)?;
    let name = rustls::pki_types::ServerName::try_from(host.to_owned())?;
    let conn = rustls::ClientConnection::new(std::sync::Arc::new(config), name)?;
    let tls = rustls::StreamOwned::new(conn, tcp);
    let (mut socket, _) = tungstenite::client(endpoint, tls)?;
    let challenge = receive(&mut socket)?;
    if challenge["event"] != "connect.challenge" {
        return Err("Gateway did not send connect.challenge".into());
    }
    let nonce = challenge["payload"]["nonce"]
        .as_str()
        .ok_or("challenge missing nonce")?;
    let ts = challenge["payload"]["ts"]
        .as_u64()
        .ok_or("challenge missing timestamp")?;
    let key = identity()?;
    let public = key.verifying_key().to_bytes();
    let device_id = hex(Sha256::digest(public));
    let scopes = ["operator.read", "operator.write"];
    let payload = format!(
        "v2|{device_id}|cli|cli|operator|{}|{ts}|{token}|{nonce}",
        scopes.join(",")
    );
    let signature = URL_SAFE_NO_PAD.encode(key.sign(payload.as_bytes()).to_bytes());
    let mut id = 0;
    let hello = rpc(
        &mut socket,
        "connect",
        json!({
            "minProtocol":4,"maxProtocol":4,
            "client":{"id":"cli","displayName":"veyterm terminal","version":"0.1.0","platform":std::env::consts::OS,"mode":"cli"},
            "role":"operator","scopes":scopes,"caps":["tool-events"],
            "auth":{"token":token},
            "device":{"id":device_id,"publicKey":URL_SAFE_NO_PAD.encode(public),"signature":signature,"signedAt":ts,"nonce":nonce}
        }),
        &mut id,
    )?;
    Ok((socket, hello))
}
pub fn start(
    endpoint: String,
    pin: Option<String>,
    insecure: bool,
    tx: Sender<Update>,
    commands: std::sync::mpsc::Receiver<Request>,
) {
    std::thread::spawn(move || {
        let token = match std::env::var("VEYTERM_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                let _ = tx.send(Update::Error(
                    "Set VEYTERM_TOKEN in your shell, then restart veyterm".into(),
                ));
                return;
            }
        };
        loop {
            let mut pending: std::collections::HashMap<String, (Request, Instant)> =
                Default::default();
            let result: Result<()> = (|| {
                let _ = tx.send(Update::Status("CONNECTING".into()));
                let (mut socket, hello) = connect(&endpoint, pin.as_deref(), insecure, &token)?;
                let _ = tx.send(Update::Status(format!(
                    "CONNECTED · Gateway {}",
                    hello["server"]["version"].as_str().unwrap_or("?")
                )));
                let methods = hello["features"]["methods"].as_array();
                let usage_available =
                    methods.is_some_and(|m| m.iter().any(|v| v == "usage.status"));
                socket
                    .get_mut()
                    .sock
                    .set_read_timeout(Some(Duration::from_millis(100)))?;
                let mut id = 10u64;
                let mut last_ping = Instant::now();
                let mut awaiting_pong: Option<Instant> = None;
                let mut refresh = Instant::now() - Duration::from_secs(31);
                loop {
                    if awaiting_pong.is_some_and(|at| at.elapsed() >= Duration::from_secs(10)) {
                        return Err("Gateway heartbeat timed out".into());
                    }
                    if awaiting_pong.is_none() && last_ping.elapsed() >= Duration::from_secs(5) {
                        socket.send(Message::Ping(b"veyterm-heartbeat".to_vec().into()))?;
                        last_ping = Instant::now();
                        awaiting_pong = Some(last_ping);
                    }
                    if pending
                        .values()
                        .any(|(_, at)| at.elapsed() > Duration::from_secs(30))
                    {
                        return Err("Gateway request acknowledgment timed out".into());
                    }
                    let mut requests: Vec<_> = commands.try_iter().collect();
                    if refresh.elapsed() >= Duration::from_secs(30) {
                        requests.push(Request::background("sessions.list",
                        json!({"limit":200,"excludeSubagents":true,"includeDerivedTitles":true})));
                        if usage_available {
                            requests.push(Request::background("usage.status", json!({})));
                        }
                        refresh = Instant::now();
                    }
                    // Register the entire batch before writing: a mid-batch failure
                    // must not silently discard the unsent tail or replay accepted sends.
                    let mut batch = Vec::new();
                    for request in requests {
                        id += 1;
                        let key = id.to_string();
                        batch.push(key.clone());
                        pending.insert(key, (request, Instant::now()));
                    }
                    for key in batch {
                        let request = &pending[&key].0;
                        socket.send(Message::Text(
                            json!({"type":"req","id":key,"method":request.method,"params":request.params}).to_string().into(),
                        ))?;
                    }
                    match socket.read() {
                        Ok(Message::Text(t)) => {
                            let v: Value = serde_json::from_str(&t)?;
                            if v["type"] == "event" {
                                let _ = tx.send(Update::Event(v));
                            } else if v["type"] == "res"
                                && let Some((request, _)) =
                                    v["id"].as_str().and_then(|id| pending.remove(id))
                            {
                                let ok = v["ok"] == true;
                                let payload = if ok {
                                    v["payload"].clone()
                                } else {
                                    v["error"].clone()
                                };
                                let update = if ok && request.method == "sessions.list" {
                                    Update::Sessions(payload)
                                } else if ok && request.method == "usage.status" {
                                    Update::Usage(payload)
                                } else {
                                    Update::Response(request, payload, ok)
                                };
                                if tx.send(update).is_err() {
                                    break;
                                }
                            }
                        }
                        Ok(Message::Pong(data)) if data.as_ref() == b"veyterm-heartbeat" => {
                            awaiting_pong = None;
                        }
                        Ok(Message::Close(_)) => return Err("Gateway disconnected".into()),
                        Ok(Message::Ping(_)) => socket.flush()?,
                        Ok(_) => (),
                        Err(tungstenite::Error::Io(e))
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) => {}
                        Err(e) => return Err(e.into()),
                    }
                }
                Ok(())
            })();
            if let Err(e) = result {
                if tx
                    .send(Update::Error(e.to_string().replace(&token, "[redacted]")))
                    .is_err()
                {
                    return;
                }
            } else {
                return;
            }
            // Report uncertain delivery, never replay a potentially accepted chat.send.
            for (_, (request, _)) in pending.drain() {
                let _ = tx.send(Update::RequestFailed(request));
            }
            for request in commands.try_iter() {
                let _ = tx.send(Update::RequestFailed(request));
            }
            std::thread::sleep(Duration::from_secs(2));
            for request in commands.try_iter() {
                let _ = tx.send(Update::RequestFailed(request));
            }
        }
    });
}

#[derive(Debug)]
struct PinnedCert {
    insecure: bool,
    pin: String,
    algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
}
impl rustls::client::danger::ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        cert: &rustls::pki_types::CertificateDer<'_>,
        _chain: &[rustls::pki_types::CertificateDer<'_>],
        _name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if self.insecure || hex(Sha256::digest(cert.as_ref())) == self.pin {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "TLS certificate fingerprint mismatch".into(),
            ))
        }
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        signature: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, signature, &self.algorithms)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        signature: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, signature, &self.algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

pub fn tls_config(pin: Option<&str>, insecure: bool) -> Result<rustls::ClientConfig> {
    let provider = rustls::crypto::ring::default_provider();
    let config = if pin.is_some() || insecure {
        let pin = pin.unwrap_or("");
        if !insecure && (pin.len() != 64 || !pin.bytes().all(|c| c.is_ascii_hexdigit())) {
            return Err("TLS pin must be 64 hexadecimal characters".into());
        }
        rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(provider.clone()))
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(std::sync::Arc::new(PinnedCert {
                pin: pin.to_lowercase(),
                insecure,
                algorithms: provider.signature_verification_algorithms,
            }))
            .with_no_client_auth()
    } else {
        let roots =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(provider))
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots)
            .with_no_client_auth()
    };
    Ok(config)
}
