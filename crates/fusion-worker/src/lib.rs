use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use fusion_discovery::{ConnectionType, HardwareProbe, HardwareSnapshot};
use fusion_network::{
    create_worker_csr, default_data_dir, tonic_ca_certificate, tonic_identity, verify_fingerprint,
    write_private_file,
};
use fusion_proto::v1::{
    ConnectionType as ProtoConnectionType, CpuCapabilities as ProtoCpuCapabilities,
    HeartbeatRequest, MemoryCapabilities as ProtoMemoryCapabilities,
    NetworkInterface as ProtoNetworkInterface, NodeCapabilities as ProtoNodeCapabilities,
    NodeRegister, PairingRequest, PairingState, PairingStatusRequest, ResourceUpdate,
    node_service_client::NodeServiceClient, pairing_service_client::PairingServiceClient,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::Instant,
};
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use uuid::Uuid;

const PAIRING_PORT: u16 = 50051;
const NODE_PORT: u16 = 50052;
const CA_PORT: u16 = 50053;

#[derive(Debug, Subcommand)]
pub enum WorkerCommand {
    /// Request approval, register, and keep sending heartbeats.
    Join {
        master_ip: IpAddr,
        #[arg(long)]
        fingerprint: String,
        #[arg(long, default_value_t = PAIRING_PORT)]
        pairing_port: u16,
        #[arg(long, default_value_t = NODE_PORT)]
        node_port: u16,
        #[arg(long, default_value_t = CA_PORT)]
        ca_port: u16,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value_t = 300)]
        approval_timeout_secs: u64,
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
}

struct JoinOptions {
    master_ip: IpAddr,
    expected_fingerprint: String,
    pairing_port: u16,
    node_port: u16,
    ca_port: u16,
    node_name: String,
    approval_timeout_secs: u64,
    data_dir: PathBuf,
}

pub async fn run(command: WorkerCommand) -> Result<()> {
    match command {
        WorkerCommand::Join {
            master_ip,
            fingerprint,
            pairing_port,
            node_port,
            ca_port,
            name,
            approval_timeout_secs,
            data_dir,
        } => {
            let node_name = name.unwrap_or_else(default_node_name);
            let data_dir = match data_dir {
                Some(path) => path,
                None => default_data_dir()?,
            };
            join(JoinOptions {
                master_ip,
                expected_fingerprint: fingerprint,
                pairing_port,
                node_port,
                ca_port,
                node_name,
                approval_timeout_secs,
                data_dir,
            })
            .await
        }
    }
}

async fn join(options: JoinOptions) -> Result<()> {
    if options.node_name.trim().is_empty() || options.node_name.len() > 128 {
        bail!("Worker name must contain 1 to 128 characters");
    }
    let ca_pem = download_ca(options.master_ip, options.ca_port).await?;
    verify_fingerprint(&ca_pem, &options.expected_fingerprint)?;

    let node_id = Uuid::new_v4().to_string();
    let (csr_pem, private_key_pem) = create_worker_csr(&node_id, &options.node_name)?;
    let pairing_channel = tls_channel(options.master_ip, options.pairing_port, &ca_pem, None)
        .await
        .context("connecting to the Master pairing service")?;
    let mut pairing_client = PairingServiceClient::new(pairing_channel);
    pairing_client
        .request_pairing(PairingRequest {
            request_id: node_id.clone(),
            node_name: options.node_name.clone(),
            certificate_signing_request_pem: csr_pem,
        })
        .await
        .context("requesting Worker approval")?;

    println!("Pairing request sent. Worker ID: {node_id}");
    println!("Approve it on the Master with: fusion master approve {node_id}");
    let deadline = Instant::now() + Duration::from_secs(options.approval_timeout_secs);
    let certificate_pem = loop {
        if Instant::now() >= deadline {
            bail!("timed out waiting for Master approval");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        let response = pairing_client
            .get_pairing_status(PairingStatusRequest {
                request_id: node_id.clone(),
            })
            .await
            .context("checking pairing approval")?
            .into_inner();
        match response.state {
            value if value == PairingState::Pending as i32 => {}
            value if value == PairingState::Approved as i32 => {
                if response.certificate_pem.is_empty() {
                    bail!("Master approved the request but returned no certificate");
                }
                break response.certificate_pem;
            }
            value if value == PairingState::Rejected as i32 => {
                bail!("Master rejected the pairing request");
            }
            _ => bail!("Master returned an unknown pairing state"),
        }
    };
    let worker_dir = options.data_dir.join(format!("worker-{node_id}"));
    std::fs::create_dir_all(&worker_dir)?;
    write_private_file(
        &worker_dir.join("worker-key.pem"),
        private_key_pem.as_bytes(),
    )?;
    std::fs::write(worker_dir.join("worker-cert.pem"), &certificate_pem)?;
    std::fs::write(worker_dir.join("ca.pem"), &ca_pem)?;

    let node_channel = tls_channel(
        options.master_ip,
        options.node_port,
        &ca_pem,
        Some((&certificate_pem, &private_key_pem)),
    )
    .await
    .context("connecting to the Master node service with mTLS")?;
    let mut node_client = NodeServiceClient::new(node_channel);
    node_client
        .register(NodeRegister {
            node_id: node_id.clone(),
            node_name: options.node_name,
            protocol_version: "2".to_owned(),
        })
        .await
        .context("registering Worker with the Master")?;

    println!("Worker registered and connected. Sending heartbeat every 5 seconds.");
    let master_addr = SocketAddr::new(options.master_ip, options.node_port);
    let mut probe = HardwareProbe::new();
    let initial_snapshot = probe
        .collect(Some(master_addr), None)
        .context("collecting initial hardware capabilities")?;
    node_client
        .update_resources(ResourceUpdate {
            capabilities: Some(snapshot_to_proto(&node_id, initial_snapshot)),
        })
        .await
        .context("sending initial hardware capabilities")?;

    let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    heartbeat.tick().await;
    let mut telemetry = tokio::time::interval(Duration::from_secs(30));
    telemetry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    telemetry.tick().await;
    let mut master_latency_ms = None;
    loop {
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result.context("waiting for shutdown signal")?;
                println!("Stopping Worker.");
                break;
            }
            _ = heartbeat.tick() => {
                let started = Instant::now();
                node_client
                    .heartbeat(HeartbeatRequest {
                        node_id: node_id.clone(),
                    })
                    .await
                    .context("sending heartbeat to the Master")?;
                master_latency_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
            }
            _ = telemetry.tick() => {
                let snapshot = probe
                    .collect(Some(master_addr), master_latency_ms)
                    .context("collecting updated hardware capabilities")?;
                node_client
                    .update_resources(ResourceUpdate {
                        capabilities: Some(snapshot_to_proto(&node_id, snapshot)),
                    })
                    .await
                    .context("updating hardware capabilities on the Master")?;
            }
        }
    }
    Ok(())
}

fn snapshot_to_proto(node_id: &str, snapshot: HardwareSnapshot) -> ProtoNodeCapabilities {
    ProtoNodeCapabilities {
        node_id: node_id.to_owned(),
        observed_unix_ms: snapshot.observed_unix_ms,
        cpu: Some(ProtoCpuCapabilities {
            vendor: snapshot.cpu.vendor,
            model: snapshot.cpu.model,
            architecture: snapshot.cpu.architecture,
            physical_cores: snapshot.cpu.physical_cores,
            logical_threads: snapshot.cpu.logical_threads,
            max_frequency_mhz: snapshot.cpu.max_frequency_mhz,
            utilization_percent: snapshot.cpu.utilization_percent,
            temperature_celsius: snapshot.cpu.temperature_celsius,
            load_average_1: snapshot.cpu.load_average_1,
            load_average_5: snapshot.cpu.load_average_5,
            load_average_15: snapshot.cpu.load_average_15,
        }),
        memory: Some(ProtoMemoryCapabilities {
            total_bytes: snapshot.memory.total_bytes,
            used_bytes: snapshot.memory.used_bytes,
            available_bytes: snapshot.memory.available_bytes,
            free_bytes: snapshot.memory.free_bytes,
        }),
        network_interfaces: snapshot
            .network_interfaces
            .into_iter()
            .map(|interface| ProtoNetworkInterface {
                name: interface.name,
                addresses: interface.addresses,
                connection_type: match interface.connection_type {
                    ConnectionType::Ethernet => ProtoConnectionType::Ethernet as i32,
                    ConnectionType::Wifi => ProtoConnectionType::Wifi as i32,
                    ConnectionType::Loopback => ProtoConnectionType::Loopback as i32,
                    ConnectionType::Other => ProtoConnectionType::Other as i32,
                },
                speed_mbps: interface.speed_mbps,
                is_up: interface.is_up,
            })
            .collect(),
        primary_local_ip: snapshot.primary_local_ip,
        master_latency_ms: snapshot.master_latency_ms,
    }
}

async fn tls_channel(
    master_ip: IpAddr,
    port: u16,
    ca_pem: &str,
    identity: Option<(&str, &str)>,
) -> Result<Channel> {
    let authority = SocketAddr::new(master_ip, port);
    let mut tls = ClientTlsConfig::new()
        .ca_certificate(tonic_ca_certificate(ca_pem))
        .domain_name(master_ip.to_string());
    if let Some((certificate_pem, key_pem)) = identity {
        tls = tls.identity(tonic_identity(certificate_pem, key_pem));
    }
    Ok(Endpoint::from_shared(format!("https://{authority}"))?
        .tls_config(tls)?
        .connect()
        .await?)
}

async fn download_ca(master_ip: IpAddr, ca_port: u16) -> Result<String> {
    let address = SocketAddr::new(master_ip, ca_port);
    let mut stream = TcpStream::connect(address)
        .await
        .with_context(|| format!("connecting to CA endpoint at {address}"))?;
    stream
        .write_all(b"GET /ca.pem HTTP/1.1\r\nHost: fusion\r\nConnection: close\r\n\r\n")
        .await?;
    let mut response = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        stream.take(16_385).read_to_end(&mut response),
    )
    .await
    .context("timed out downloading Master CA certificate")??;
    if response.len() > 16_384 {
        bail!("Master CA response exceeds the maximum permitted size");
    }
    let response = String::from_utf8(response).context("CA endpoint returned non-UTF-8 data")?;
    let (headers, body) = response
        .split_once("\r\n\r\n")
        .context("invalid response from Master CA endpoint")?;
    if !headers.starts_with("HTTP/1.1 200 ") {
        bail!(
            "Master CA endpoint returned {}",
            headers.lines().next().unwrap_or("an error")
        );
    }
    if body.len() > 16_384 || !body.contains("-----BEGIN CERTIFICATE-----") {
        bail!("Master returned an invalid CA certificate");
    }
    Ok(body.to_owned())
}

fn default_node_name() -> String {
    std::env::var("HOSTNAME").unwrap_or_else(|_| "fusion-worker".to_owned())
}
