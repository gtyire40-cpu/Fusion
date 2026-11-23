use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use fusion_network::{
    ca_fingerprint, default_data_dir, master_identity, sign_worker_csr, tonic_ca_certificate,
    tonic_identity, validate_worker_csr,
};
use fusion_proto::v1::{
    HeartbeatRequest, NodeRegister, NodeStatus, PairingRequest, PairingResponse, PairingState,
    PairingStatusRequest, PairingStatusResponse,
    node_service_server::{NodeService, NodeServiceServer},
    pairing_service_server::{PairingService, PairingServiceServer},
};
use rusqlite::{Connection, OptionalExtension, params};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{
    Request, Response, Status,
    transport::{Server, ServerTlsConfig},
};
use x509_parser::prelude::*;

const OFFLINE_AFTER_MS: i64 = 15_000;

#[derive(Debug, Subcommand)]
pub enum MasterCommand {
    /// Start the Fusion Master services.
    Start {
        #[arg(long, default_value = "127.0.0.1")]
        advertise_host: String,
        #[arg(long, default_value = "0.0.0.0:50051")]
        pairing_bind: SocketAddr,
        #[arg(long, default_value = "0.0.0.0:50052")]
        node_bind: SocketAddr,
        #[arg(long, default_value = "0.0.0.0:50053")]
        ca_bind: SocketAddr,
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Show workers waiting for approval.
    Pending {
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Approve a worker's pairing request.
    Approve {
        node_id: String,
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Reject a worker's pairing request.
    Reject {
        node_id: String,
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// List paired workers and their current status.
    Nodes {
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
}

pub async fn run(command: MasterCommand) -> Result<()> {
    match command {
        MasterCommand::Start {
            advertise_host,
            pairing_bind,
            node_bind,
            ca_bind,
            data_dir,
        } => {
            start(
                &advertise_host,
                pairing_bind,
                node_bind,
                ca_bind,
                resolve_data_dir(data_dir)?,
            )
            .await
        }
        MasterCommand::Pending { data_dir } => list_pending(&resolve_data_dir(data_dir)?),
        MasterCommand::Approve { node_id, data_dir } => {
            approve(&resolve_data_dir(data_dir)?, &node_id)
        }
        MasterCommand::Reject { node_id, data_dir } => {
            reject(&resolve_data_dir(data_dir)?, &node_id)
        }
        MasterCommand::Nodes { data_dir } => list_nodes(&resolve_data_dir(data_dir)?),
    }
}

pub fn resolve_data_dir(data_dir: Option<PathBuf>) -> Result<PathBuf> {
    data_dir.map(Ok).unwrap_or_else(default_data_dir)
}

pub fn list_nodes(data_dir: &Path) -> Result<()> {
    let connection = open_database(data_dir)?;
    let mut statement = connection.prepare(
        "SELECT node_id, node_name, last_seen_ms FROM nodes ORDER BY node_name, node_id",
    )?;
    let now = now_unix_ms()?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    println!("{:<36}  {:<20}  STATUS", "NODE ID", "NAME");
    for row in rows {
        let (node_id, node_name, last_seen_ms) = row?;
        let status = if now.saturating_sub(last_seen_ms) <= OFFLINE_AFTER_MS {
            "online"
        } else {
            "offline"
        };
        println!("{node_id:<36}  {node_name:<20}  {status}");
    }
    Ok(())
}

fn list_pending(data_dir: &Path) -> Result<()> {
    let connection = open_database(data_dir)?;
    let mut statement = connection.prepare(
        "SELECT node_id, node_name FROM pairing_requests WHERE state = 1 ORDER BY node_name",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut any = false;
    for row in rows {
        any = true;
        let (node_id, node_name) = row?;
        println!("{node_id}  {node_name}");
    }
    if !any {
        println!("No pending pairing requests.");
    }
    Ok(())
}

fn approve(data_dir: &Path, node_id: &str) -> Result<()> {
    let connection = open_database(data_dir)?;
    let csr: Option<String> = connection
        .query_row(
            "SELECT csr_pem FROM pairing_requests WHERE node_id = ?1 AND state = 1",
            [node_id],
            |row| row.get(0),
        )
        .optional()?;
    let csr = csr.with_context(|| format!("no pending pairing request for {node_id}"))?;
    let cert_pem = sign_worker_csr(data_dir, node_id, &csr)?;
    let changed = connection.execute(
        "UPDATE pairing_requests SET state = 2, certificate_pem = ?2 WHERE node_id = ?1 AND state = 1",
        params![node_id, cert_pem],
    )?;
    if changed != 1 {
        bail!("pairing request for {node_id} changed while it was being approved");
    }
    println!("Approved worker {node_id}. It can now connect to the Master.");
    Ok(())
}

fn reject(data_dir: &Path, node_id: &str) -> Result<()> {
    let connection = open_database(data_dir)?;
    let changed = connection.execute(
        "UPDATE pairing_requests SET state = 3 WHERE node_id = ?1 AND state = 1",
        [node_id],
    )?;
    if changed != 1 {
        bail!("no pending pairing request for {node_id}");
    }
    println!("Rejected worker {node_id}.");
    Ok(())
}

async fn start(
    advertise_host: &str,
    pairing_bind: SocketAddr,
    node_bind: SocketAddr,
    ca_bind: SocketAddr,
    data_dir: PathBuf,
) -> Result<()> {
    let identity = master_identity(&data_dir, advertise_host)?;
    initialize_database(&data_dir)?;
    let pairing_listener = TcpListener::bind(pairing_bind)
        .await
        .with_context(|| format!("binding pairing service at {pairing_bind}"))?;
    let node_listener = TcpListener::bind(node_bind)
        .await
        .with_context(|| format!("binding node service at {node_bind}"))?;
    let ca_listener = TcpListener::bind(ca_bind)
        .await
        .with_context(|| format!("binding CA endpoint at {ca_bind}"))?;

    let fingerprint = ca_fingerprint(&identity.ca_pem)?;
    println!("Fusion Master starting.");
    println!("Master CA SHA-256 fingerprint: {fingerprint}");
    println!("Pairing TLS: {pairing_bind}");
    println!("Node mTLS:   {node_bind}");
    println!("CA download: http://{ca_bind}/ca.pem");
    println!("Approve requests with: fusion master pending / fusion master approve <node-id>");
    println!("Data directory: {}", data_dir.display());

    let pair_tls = ServerTlsConfig::new().identity(tonic_identity(
        &identity.server_cert_pem,
        &identity.server_key_pem,
    ));
    let node_tls = ServerTlsConfig::new()
        .identity(tonic_identity(
            &identity.server_cert_pem,
            &identity.server_key_pem,
        ))
        .client_ca_root(tonic_ca_certificate(&identity.ca_pem));
    let pairing = PairingApi {
        data_dir: data_dir.clone(),
    };
    let nodes = NodeApi {
        data_dir: data_dir.clone(),
    };

    let pair_server = Server::builder()
        .tls_config(pair_tls)?
        .add_service(PairingServiceServer::new(pairing))
        .serve_with_incoming(TcpListenerStream::new(pairing_listener));
    let node_server = Server::builder()
        .tls_config(node_tls)?
        .add_service(NodeServiceServer::new(nodes))
        .serve_with_incoming(TcpListenerStream::new(node_listener));
    let ca_server = serve_ca(ca_listener, Arc::new(identity.ca_pem));
    tokio::pin!(pair_server, node_server, ca_server);

    tokio::select! {
        result = &mut pair_server => result.context("pairing service stopped"),
        result = &mut node_server => result.context("node service stopped"),
        result = &mut ca_server => result.context("CA download service stopped"),
        result = tokio::signal::ctrl_c() => {
            result.context("waiting for shutdown signal")?;
            println!("Stopping Fusion Master.");
            Ok(())
        }
    }
}

async fn serve_ca(listener: TcpListener, ca_pem: Arc<String>) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let ca_pem = Arc::clone(&ca_pem);
        tokio::spawn(async move {
            if let Err(error) = handle_ca_request(stream, &ca_pem).await {
                eprintln!("CA endpoint request failed: {error}");
            }
        });
    }
}

async fn handle_ca_request(mut stream: TcpStream, ca_pem: &str) -> Result<()> {
    let mut request = [0_u8; 1024];
    let read = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut request))
        .await
        .context("timed out waiting for CA request")??;
    let is_ca_request = request[..read].starts_with(b"GET /ca.pem HTTP/1.");
    let (status, body) = if is_ca_request {
        ("200 OK", ca_pem)
    } else {
        ("404 Not Found", "Not found\n")
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    Ok(())
}

#[derive(Clone)]
struct PairingApi {
    data_dir: PathBuf,
}

#[tonic::async_trait]
impl PairingService for PairingApi {
    async fn request_pairing(
        &self,
        request: Request<PairingRequest>,
    ) -> Result<Response<PairingResponse>, Status> {
        let request = request.into_inner();
        if request.request_id.len() > 64
            || request.node_name.trim().is_empty()
            || request.node_name.len() > 128
            || request.certificate_signing_request_pem.len() > 16_384
        {
            return Err(Status::invalid_argument("invalid pairing request fields"));
        }
        validate_worker_csr(&request.certificate_signing_request_pem)
            .map_err(|_| Status::invalid_argument("invalid Worker certificate signing request"))?;
        uuid::Uuid::parse_str(&request.request_id)
            .map_err(|_| Status::invalid_argument("request_id must be a UUID"))?;

        let connection = open_database(&self.data_dir)
            .map_err(|error| Status::internal(format!("opening pairing database: {error}")))?;
        connection
            .execute(
                "INSERT INTO pairing_requests (node_id, node_name, csr_pem, state, certificate_pem) VALUES (?1, ?2, ?3, 1, '')",
                params![request.request_id, request.node_name, request.certificate_signing_request_pem],
            )
            .map_err(|error| match error {
                rusqlite::Error::SqliteFailure(code, _) if code.code == rusqlite::ErrorCode::ConstraintViolation => {
                    Status::already_exists("pairing request already exists")
                }
                other => Status::internal(format!("storing pairing request: {other}")),
            })?;

        Ok(Response::new(PairingResponse {
            request_id: request.request_id,
        }))
    }

    async fn get_pairing_status(
        &self,
        request: Request<PairingStatusRequest>,
    ) -> Result<Response<PairingStatusResponse>, Status> {
        let request_id = request.into_inner().request_id;
        let connection = open_database(&self.data_dir)
            .map_err(|error| Status::internal(format!("opening pairing database: {error}")))?;
        let result = connection
            .query_row(
                "SELECT state, certificate_pem FROM pairing_requests WHERE node_id = ?1",
                [request_id],
                |row| Ok((row.get::<_, i32>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|error| Status::internal(format!("reading pairing request: {error}")))?
            .ok_or_else(|| Status::not_found("pairing request not found"))?;
        let state = match result.0 {
            1 => PairingState::Pending,
            2 => PairingState::Approved,
            3 => PairingState::Rejected,
            _ => return Err(Status::internal("unknown pairing request state")),
        };
        Ok(Response::new(PairingStatusResponse {
            state: state as i32,
            certificate_pem: result.1,
            message: String::new(),
        }))
    }
}

#[derive(Clone)]
struct NodeApi {
    data_dir: PathBuf,
}

#[tonic::async_trait]
impl NodeService for NodeApi {
    async fn register(
        &self,
        request: Request<NodeRegister>,
    ) -> Result<Response<NodeStatus>, Status> {
        let cert_node_id = peer_node_id(&request)?;
        let register = request.into_inner();
        if register.node_id != cert_node_id {
            return Err(Status::permission_denied(
                "node_id does not match the client certificate",
            ));
        }
        if register.node_name.trim().is_empty() || register.protocol_version != "1" {
            return Err(Status::invalid_argument(
                "invalid node name or unsupported protocol version",
            ));
        }

        let connection = open_database(&self.data_dir)
            .map_err(|error| Status::internal(format!("opening node database: {error}")))?;
        let paired_name = connection
            .query_row(
                "SELECT node_name FROM pairing_requests WHERE node_id = ?1 AND state = 2",
                [&register.node_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| Status::internal(format!("checking worker pairing: {error}")))?
            .ok_or_else(|| Status::permission_denied("worker has not been approved"))?;
        if paired_name != register.node_name {
            return Err(Status::permission_denied(
                "node name does not match the approved pairing request",
            ));
        }
        let last_seen = now_unix_ms()
            .map_err(|error| Status::internal(format!("reading system clock: {error}")))?;
        connection
            .execute(
                "INSERT INTO nodes (node_id, node_name, protocol_version, last_seen_ms) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(node_id) DO UPDATE SET node_name = excluded.node_name, protocol_version = excluded.protocol_version, last_seen_ms = excluded.last_seen_ms",
                params![register.node_id, register.node_name, register.protocol_version, last_seen],
            )
            .map_err(|error| Status::internal(format!("registering worker: {error}")))?;

        Ok(Response::new(node_status(
            register.node_id,
            register.node_name,
            last_seen,
        )))
    }

    async fn heartbeat(
        &self,
        request: Request<HeartbeatRequest>,
    ) -> Result<Response<NodeStatus>, Status> {
        let cert_node_id = peer_node_id(&request)?;
        let heartbeat = request.into_inner();
        if heartbeat.node_id != cert_node_id {
            return Err(Status::permission_denied(
                "node_id does not match the client certificate",
            ));
        }
        let connection = open_database(&self.data_dir)
            .map_err(|error| Status::internal(format!("opening node database: {error}")))?;
        let node_name = connection
            .query_row(
                "SELECT node_name FROM nodes WHERE node_id = ?1",
                [&heartbeat.node_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| Status::internal(format!("checking registered worker: {error}")))?
            .ok_or_else(|| Status::permission_denied("worker is not registered"))?;
        let last_seen = now_unix_ms()
            .map_err(|error| Status::internal(format!("reading system clock: {error}")))?;
        connection
            .execute(
                "UPDATE nodes SET last_seen_ms = ?2 WHERE node_id = ?1",
                params![heartbeat.node_id, last_seen],
            )
            .map_err(|error| Status::internal(format!("updating heartbeat: {error}")))?;
        Ok(Response::new(node_status(
            heartbeat.node_id,
            node_name,
            last_seen,
        )))
    }
}

fn peer_node_id<T>(request: &Request<T>) -> Result<String, Status> {
    let certificates = request
        .peer_certs()
        .ok_or_else(|| Status::unauthenticated("client certificate is required"))?;
    let certificate = certificates
        .first()
        .ok_or_else(|| Status::unauthenticated("client certificate is missing"))?;
    let (_, certificate) = parse_x509_certificate(certificate.as_ref())
        .map_err(|_| Status::unauthenticated("invalid client certificate"))?;
    certificate
        .subject()
        .iter_common_name()
        .next()
        .and_then(|name| name.as_str().ok())
        .map(str::to_owned)
        .ok_or_else(|| Status::unauthenticated("client certificate has no node identity"))
}

fn node_status(node_id: String, node_name: String, last_seen: i64) -> NodeStatus {
    NodeStatus {
        node_id,
        node_name,
        last_seen_unix_ms: last_seen,
        online: true,
    }
}

fn initialize_database(data_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let connection = Connection::open(data_dir.join("fusion.sqlite3"))?;
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE IF NOT EXISTS pairing_requests (
             node_id TEXT PRIMARY KEY NOT NULL,
             node_name TEXT NOT NULL,
             csr_pem TEXT NOT NULL,
             state INTEGER NOT NULL CHECK (state IN (1, 2, 3)),
             certificate_pem TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS nodes (
             node_id TEXT PRIMARY KEY NOT NULL,
             node_name TEXT NOT NULL,
             protocol_version TEXT NOT NULL,
             last_seen_ms INTEGER NOT NULL
         );",
    )?;
    Ok(())
}

fn open_database(data_dir: &Path) -> Result<Connection> {
    let connection = Connection::open(data_dir.join("fusion.sqlite3"))
        .with_context(|| format!("opening database in {}", data_dir.display()))?;
    connection.busy_timeout(Duration::from_secs(5))?;
    Ok(connection)
}

fn now_unix_ms() -> Result<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?;
    duration
        .as_millis()
        .try_into()
        .context("system clock timestamp exceeds supported range")
}
