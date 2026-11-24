use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use fusion_discovery::{
    ConnectionType, CpuCapabilities, HardwareSnapshot, MemoryCapabilities, NetworkInterface,
};
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
    /// Show detailed capabilities for a paired worker.
    Show {
        node_id: String,
        #[arg(long)]
        json: bool,
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
        MasterCommand::Show {
            node_id,
            json,
            data_dir,
        } => show_node(&resolve_data_dir(data_dir)?, &node_id, json),
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

pub fn show_node(data_dir: &Path, node_id: &str, json: bool) -> Result<()> {
    let connection = open_database(data_dir)?;
    let node = connection
        .query_row(
            "SELECT node_name, last_seen_ms, capabilities_json, capabilities_updated_ms FROM nodes WHERE node_id = ?1",
            [node_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            },
        )
        .optional()?
        .with_context(|| format!("unknown node {node_id}"))?;
    let capabilities_json = node
        .2
        .context("this node has not sent hardware capabilities yet")?;
    let capabilities: HardwareSnapshot =
        serde_json::from_str(&capabilities_json).context("stored node capabilities are invalid")?;
    let detail = NodeDetail {
        node_id,
        node_name: node.0,
        online: now_unix_ms()?.saturating_sub(node.1) <= OFFLINE_AFTER_MS,
        capabilities_updated_unix_ms: node.3.unwrap_or_default(),
        capabilities,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&detail)?);
    } else {
        print_node_detail(&detail);
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct NodeDetail<'a> {
    node_id: &'a str,
    node_name: String,
    online: bool,
    capabilities_updated_unix_ms: i64,
    capabilities: HardwareSnapshot,
}

fn print_node_detail(detail: &NodeDetail<'_>) {
    let cpu = &detail.capabilities.cpu;
    let memory = &detail.capabilities.memory;
    println!("Node: {} ({})", detail.node_name, detail.node_id);
    println!(
        "Status: {}",
        if detail.online { "online" } else { "offline" }
    );
    println!(
        "Capabilities updated: {} ms (Unix)",
        detail.capabilities_updated_unix_ms
    );
    println!("CPU: {} {} ({})", cpu.vendor, cpu.model, cpu.architecture);
    println!(
        "Cores: {} physical / {} logical",
        cpu.physical_cores
            .map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
        cpu.logical_threads
    );
    print_optional(
        "CPU maximum frequency",
        cpu.max_frequency_mhz.map(|value| format!("{value} MHz")),
    );
    print_optional(
        "CPU utilization",
        cpu.utilization_percent.map(|value| format!("{value:.1}%")),
    );
    print_optional(
        "CPU temperature",
        cpu.temperature_celsius.map(|value| format!("{value:.1} C")),
    );
    println!(
        "Load average: {} / {} / {}",
        format_optional(cpu.load_average_1),
        format_optional(cpu.load_average_5),
        format_optional(cpu.load_average_15)
    );
    println!("Memory total: {}", format_bytes(memory.total_bytes));
    println!("Memory used: {}", format_bytes(memory.used_bytes));
    println!("Memory available: {}", format_bytes(memory.available_bytes));
    println!("Memory free: {}", format_bytes(memory.free_bytes));
    print_optional(
        "Primary local IP",
        detail.capabilities.primary_local_ip.clone(),
    );
    print_optional(
        "Master round-trip latency",
        detail
            .capabilities
            .master_latency_ms
            .map(|value| format!("{value:.2} ms")),
    );
    println!("Network interfaces:");
    for interface in &detail.capabilities.network_interfaces {
        println!(
            "  {} ({:?}, {})",
            interface.name,
            interface.connection_type,
            if interface.is_up { "up" } else { "down" }
        );
        println!(
            "    addresses: {}",
            if interface.addresses.is_empty() {
                "none".to_owned()
            } else {
                interface.addresses.join(", ")
            }
        );
        print_optional(
            "    link speed",
            interface.speed_mbps.map(|value| format!("{value} Mbps")),
        );
    }
}

fn print_optional(label: &str, value: Option<String>) {
    println!(
        "{label}: {}",
        value.unwrap_or_else(|| "unavailable".to_owned())
    );
}

fn format_optional(value: Option<f64>) -> String {
    value.map_or_else(|| "unavailable".to_owned(), |value| format!("{value:.2}"))
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.2} {}", UNITS[unit])
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
        if register.node_name.trim().is_empty() || register.protocol_version != "2" {
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

    async fn update_resources(
        &self,
        request: Request<fusion_proto::v1::ResourceUpdate>,
    ) -> Result<Response<NodeStatus>, Status> {
        let cert_node_id = peer_node_id(&request)?;
        let capabilities = request
            .into_inner()
            .capabilities
            .ok_or_else(|| Status::invalid_argument("resource update is missing capabilities"))?;
        if capabilities.node_id != cert_node_id {
            return Err(Status::permission_denied(
                "node_id does not match the client certificate",
            ));
        }
        validate_capabilities(&capabilities)?;
        let connection = open_database(&self.data_dir)
            .map_err(|error| Status::internal(format!("opening node database: {error}")))?;
        let node_name = connection
            .query_row(
                "SELECT node_name FROM nodes WHERE node_id = ?1",
                [&capabilities.node_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| Status::internal(format!("checking registered worker: {error}")))?
            .ok_or_else(|| Status::permission_denied("worker is not registered"))?;

        let received_at = now_unix_ms()
            .map_err(|error| Status::internal(format!("reading system clock: {error}")))?;
        let node_id = capabilities.node_id.clone();
        let snapshot = snapshot_from_proto(capabilities)
            .map_err(|error| Status::invalid_argument(format!("invalid capabilities: {error}")))?;
        let serialized = serde_json::to_string(&snapshot)
            .map_err(|error| Status::internal(format!("serializing node capabilities: {error}")))?;
        connection
            .execute(
                "UPDATE nodes SET capabilities_json = ?2, capabilities_updated_ms = ?3, last_seen_ms = ?3 WHERE node_id = ?1",
                params![node_id, serialized, received_at],
            )
            .map_err(|error| Status::internal(format!("storing node capabilities: {error}")))?;

        Ok(Response::new(node_status(node_id, node_name, received_at)))
    }
}

fn validate_capabilities(capabilities: &fusion_proto::v1::NodeCapabilities) -> Result<(), Status> {
    let Some(cpu) = capabilities.cpu.as_ref() else {
        return Err(Status::invalid_argument("CPU capabilities are required"));
    };
    let Some(memory) = capabilities.memory.as_ref() else {
        return Err(Status::invalid_argument("memory capabilities are required"));
    };
    if capabilities.observed_unix_ms <= 0
        || cpu.vendor.len() > 256
        || cpu.model.len() > 512
        || cpu.architecture.len() > 64
        || cpu.architecture.is_empty()
        || cpu.logical_threads == 0
        || cpu.logical_threads > 4096
        || cpu
            .physical_cores
            .is_some_and(|cores| cores == 0 || cores > 4096)
        || memory.total_bytes == 0
        || memory.used_bytes > memory.total_bytes
        || memory.available_bytes > memory.total_bytes
        || memory.free_bytes > memory.total_bytes
        || memory.used_bytes != memory.total_bytes.saturating_sub(memory.available_bytes)
        || memory.free_bytes > memory.available_bytes
        || capabilities.network_interfaces.len() > 64
        || capabilities
            .primary_local_ip
            .as_ref()
            .is_some_and(|address| address.len() > 128)
    {
        return Err(Status::invalid_argument(
            "invalid hardware capability fields",
        ));
    }
    if let Some(value) = cpu.utilization_percent {
        if !value.is_finite() || !(0.0..=100.0).contains(&value) {
            return Err(Status::invalid_argument(
                "CPU utilization must be between 0 and 100",
            ));
        }
    }
    if let Some(value) = cpu.temperature_celsius {
        if !value.is_finite() || !(-20.0..=150.0).contains(&value) {
            return Err(Status::invalid_argument(
                "CPU temperature is outside supported range",
            ));
        }
    }
    for load in [cpu.load_average_1, cpu.load_average_5, cpu.load_average_15]
        .into_iter()
        .flatten()
    {
        if !load.is_finite() || load < 0.0 {
            return Err(Status::invalid_argument(
                "CPU load averages must be non-negative",
            ));
        }
    }
    if let Some(value) = capabilities.master_latency_ms {
        if !value.is_finite() || value < 0.0 {
            return Err(Status::invalid_argument(
                "master latency must be non-negative",
            ));
        }
    }
    for interface in &capabilities.network_interfaces {
        if interface.name.is_empty()
            || interface.name.len() > 64
            || interface.addresses.len() > 32
            || interface
                .addresses
                .iter()
                .any(|address| address.len() > 128)
        {
            return Err(Status::invalid_argument("invalid network interface fields"));
        }
        if fusion_proto::v1::ConnectionType::try_from(interface.connection_type).is_err() {
            return Err(Status::invalid_argument(
                "unknown network interface connection type",
            ));
        }
    }
    Ok(())
}

fn snapshot_from_proto(
    capabilities: fusion_proto::v1::NodeCapabilities,
) -> anyhow::Result<HardwareSnapshot> {
    let cpu = capabilities.cpu.context("CPU capabilities are required")?;
    let memory = capabilities
        .memory
        .context("memory capabilities are required")?;
    Ok(HardwareSnapshot {
        observed_unix_ms: capabilities.observed_unix_ms,
        cpu: CpuCapabilities {
            vendor: cpu.vendor,
            model: cpu.model,
            architecture: cpu.architecture,
            physical_cores: cpu.physical_cores,
            logical_threads: cpu.logical_threads,
            max_frequency_mhz: cpu.max_frequency_mhz,
            utilization_percent: cpu.utilization_percent,
            temperature_celsius: cpu.temperature_celsius,
            load_average_1: cpu.load_average_1,
            load_average_5: cpu.load_average_5,
            load_average_15: cpu.load_average_15,
        },
        memory: MemoryCapabilities {
            total_bytes: memory.total_bytes,
            used_bytes: memory.used_bytes,
            available_bytes: memory.available_bytes,
            free_bytes: memory.free_bytes,
        },
        network_interfaces: capabilities
            .network_interfaces
            .into_iter()
            .map(|interface| NetworkInterface {
                name: interface.name,
                addresses: interface.addresses,
                connection_type: match fusion_proto::v1::ConnectionType::try_from(
                    interface.connection_type,
                )
                .unwrap_or(fusion_proto::v1::ConnectionType::Unspecified)
                {
                    fusion_proto::v1::ConnectionType::Ethernet => ConnectionType::Ethernet,
                    fusion_proto::v1::ConnectionType::Wifi => ConnectionType::Wifi,
                    fusion_proto::v1::ConnectionType::Loopback => ConnectionType::Loopback,
                    _ => ConnectionType::Other,
                },
                speed_mbps: interface.speed_mbps,
                is_up: interface.is_up,
            })
            .collect(),
        primary_local_ip: capabilities.primary_local_ip,
        master_latency_ms: capabilities.master_latency_ms,
    })
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
             last_seen_ms INTEGER NOT NULL,
             capabilities_json TEXT,
             capabilities_updated_ms INTEGER
         );",
    )?;
    ensure_node_capability_columns(&connection)?;
    Ok(())
}

fn open_database(data_dir: &Path) -> Result<Connection> {
    let connection = Connection::open(data_dir.join("fusion.sqlite3"))
        .with_context(|| format!("opening database in {}", data_dir.display()))?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS pairing_requests (
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
             last_seen_ms INTEGER NOT NULL,
             capabilities_json TEXT,
             capabilities_updated_ms INTEGER
         );",
    )?;
    ensure_node_capability_columns(&connection)?;
    Ok(connection)
}

fn ensure_node_capability_columns(connection: &Connection) -> Result<()> {
    let mut statement = connection.prepare("PRAGMA table_info(nodes)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    if !columns.iter().any(|column| column == "capabilities_json") {
        connection.execute("ALTER TABLE nodes ADD COLUMN capabilities_json TEXT", [])?;
    }
    if !columns
        .iter()
        .any(|column| column == "capabilities_updated_ms")
    {
        connection.execute(
            "ALTER TABLE nodes ADD COLUMN capabilities_updated_ms INTEGER",
            [],
        )?;
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_phase_one_database_gains_capability_columns() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let database_path = directory.path().join("fusion.sqlite3");
        let connection = Connection::open(&database_path)?;
        connection.execute_batch(
            "CREATE TABLE nodes (
                node_id TEXT PRIMARY KEY NOT NULL,
                node_name TEXT NOT NULL,
                protocol_version TEXT NOT NULL,
                last_seen_ms INTEGER NOT NULL
            );
            INSERT INTO nodes VALUES ('worker-1', 'legacy-worker', '2', 1234);",
        )?;
        drop(connection);

        let connection = open_database(directory.path())?;
        let (name, capabilities): (String, Option<String>) = connection.query_row(
            "SELECT node_name, capabilities_json FROM nodes WHERE node_id = 'worker-1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(name, "legacy-worker");
        assert_eq!(capabilities, None);
        Ok(())
    }

    #[test]
    fn resource_updates_require_cpu_and_memory_capabilities() {
        let empty = fusion_proto::v1::NodeCapabilities::default();
        assert!(validate_capabilities(&empty).is_err());
    }
}
