use std::{
    fs,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
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
const WORKER_IDENTITY_FILE: &str = "worker-identity.json";

#[derive(Debug, Subcommand)]
pub enum WorkerCommand {
    /// Pair on first run, then reuse the saved identity and send heartbeats.
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
        /// Select a legacy Worker identity when multiple saved identities exist.
        #[arg(long, value_name = "UUID")]
        recover_node_id: Option<String>,
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
    node_name: Option<String>,
    recover_node_id: Option<String>,
    approval_timeout_secs: u64,
    data_dir: PathBuf,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct WorkerIdentity {
    node_id: String,
    node_name: String,
    ca_fingerprint: String,
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
            recover_node_id,
            approval_timeout_secs,
            data_dir,
        } => {
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
                node_name: name,
                recover_node_id,
                approval_timeout_secs,
                data_dir,
            })
            .await
        }
    }
}

async fn join(options: JoinOptions) -> Result<()> {
    if options
        .node_name
        .as_ref()
        .is_some_and(|name| name.trim().is_empty() || name.len() > 128)
    {
        bail!("Worker name must contain 1 to 128 characters");
    }
    let recover_node_id = options
        .recover_node_id
        .as_deref()
        .map(|node_id| {
            Uuid::parse_str(node_id)
                .map(|node_id| node_id.to_string())
                .context("--recover-node-id must be a UUID")
        })
        .transpose()?;
    let ca_pem = download_ca(options.master_ip, options.ca_port)
        .await
        .context("ensure `fusion master start` is running and reachable")?;
    verify_fingerprint(&ca_pem, &options.expected_fingerprint)?;
    let current_ca_fingerprint = fusion_network::ca_fingerprint(&ca_pem)?;

    let (identity, recovered_legacy_identity) = match load_worker_identity(&options.data_dir)? {
        Some(identity)
            if recover_node_id
                .as_ref()
                .is_none_or(|node_id| node_id.eq_ignore_ascii_case(&identity.node_id)) =>
        {
            (Some(identity), false)
        }
        stored_identity => {
            let fallback_name = options.node_name.as_deref().or_else(|| {
                stored_identity
                    .as_ref()
                    .map(|identity| identity.node_name.as_str())
            });
            let identity = recover_legacy_worker_identity(
                &options.data_dir,
                &current_ca_fingerprint,
                fallback_name,
                recover_node_id.as_deref(),
            )?;
            let recovered = identity.is_some();
            (identity, recovered)
        }
    };
    let (node_id, node_name, certificate_pem, private_key_pem) = if let Some(identity) = identity {
        if identity.ca_fingerprint != current_ca_fingerprint {
            bail!("saved Worker identity belongs to a different Master CA");
        }
        if options
            .node_name
            .as_ref()
            .is_some_and(|name| name != &identity.node_name)
        {
            bail!(
                "saved Worker identity is named {:?}; use that name or the original data directory",
                identity.node_name
            );
        }
        let worker_dir = options
            .data_dir
            .join(format!("worker-{}", identity.node_id));
        let certificate_pem = fs::read_to_string(worker_dir.join("worker-cert.pem"))
            .context("reading saved Worker certificate")?;
        let private_key_pem = fs::read_to_string(worker_dir.join("worker-key.pem"))
            .context("reading saved Worker private key")?;
        println!("Reusing Worker ID: {}", identity.node_id);
        (
            identity.node_id,
            identity.node_name,
            certificate_pem,
            private_key_pem,
        )
    } else {
        let node_name = options.node_name.unwrap_or_else(default_node_name);
        let node_id = Uuid::new_v4().to_string();
        let (csr_pem, private_key_pem) = create_worker_csr(&node_id, &node_name)?;
        let pairing_channel = tls_channel(options.master_ip, options.pairing_port, &ca_pem, None)
            .await
            .context("connecting to the Master pairing service")?;
        let mut pairing_client = PairingServiceClient::new(pairing_channel);
        pairing_client
            .request_pairing(PairingRequest {
                request_id: node_id.clone(),
                node_name: node_name.clone(),
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
        fs::create_dir_all(&worker_dir)?;
        write_private_file(
            &worker_dir.join("worker-key.pem"),
            private_key_pem.as_bytes(),
        )?;
        fs::write(worker_dir.join("worker-cert.pem"), &certificate_pem)?;
        fs::write(worker_dir.join("ca.pem"), &ca_pem)?;
        save_worker_identity(
            &options.data_dir,
            &WorkerIdentity {
                node_id: node_id.clone(),
                node_name: node_name.clone(),
                ca_fingerprint: current_ca_fingerprint.clone(),
            },
        )?;
        (node_id, node_name, certificate_pem, private_key_pem)
    };

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
            node_name: node_name.clone(),
            protocol_version: "2".to_owned(),
        })
        .await
        .context("registering Worker with the Master")?;
    if recovered_legacy_identity {
        save_worker_identity(
            &options.data_dir,
            &WorkerIdentity {
                node_id: node_id.clone(),
                node_name,
                ca_fingerprint: current_ca_fingerprint,
            },
        )?;
    }

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

fn load_worker_identity(data_dir: &Path) -> Result<Option<WorkerIdentity>> {
    let path = data_dir.join(WORKER_IDENTITY_FILE);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let identity: WorkerIdentity =
        serde_json::from_str(&contents).context("parsing saved Worker identity")?;
    Uuid::parse_str(&identity.node_id).context("saved Worker identity has an invalid node ID")?;
    Ok(Some(identity))
}

fn recover_legacy_worker_identity(
    data_dir: &Path,
    current_ca_fingerprint: &str,
    requested_node_name: Option<&str>,
    requested_node_id: Option<&str>,
) -> Result<Option<WorkerIdentity>> {
    let entries = match fs::read_dir(data_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", data_dir.display())),
    };
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let directory_name = entry.file_name();
        let Some(node_id) = directory_name
            .to_string_lossy()
            .strip_prefix("worker-")
            .map(str::to_owned)
        else {
            continue;
        };
        if Uuid::parse_str(&node_id).is_err() {
            continue;
        }
        let worker_dir = entry.path();
        let ca_pem = match fs::read_to_string(worker_dir.join("ca.pem")) {
            Ok(ca_pem) => ca_pem,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("reading legacy Worker CA certificate"),
        };
        if fusion_network::ca_fingerprint(&ca_pem)? != current_ca_fingerprint {
            continue;
        }
        if !worker_dir.join("worker-cert.pem").is_file()
            || !worker_dir.join("worker-key.pem").is_file()
        {
            continue;
        }
        candidates.push(node_id);
    }

    let Some(node_id) = select_recovery_candidate(candidates, requested_node_id)? else {
        return Ok(None);
    };
    let identity = WorkerIdentity {
        node_id: node_id.clone(),
        node_name: requested_node_name
            .map(str::to_owned)
            .unwrap_or_else(default_node_name),
        ca_fingerprint: current_ca_fingerprint.to_owned(),
    };
    println!("Recovered legacy Worker ID: {node_id}");
    Ok(Some(identity))
}

fn select_recovery_candidate(
    mut candidates: Vec<String>,
    requested_node_id: Option<&str>,
) -> Result<Option<String>> {
    candidates.sort();
    candidates.dedup();
    if let Some(requested_node_id) = requested_node_id {
        return candidates
            .into_iter()
            .find(|node_id| node_id == requested_node_id)
            .map(Some)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Worker identity {requested_node_id} was not found among saved identities for this Master"
                )
            });
    }
    match candidates.len() {
        0 => Ok(None),
        1 => Ok(candidates.pop()),
        _ => bail!(
            "multiple saved Worker identities found: {}. Choose one with --recover-node-id <UUID>",
            candidates.join(", ")
        ),
    }
}

fn save_worker_identity(data_dir: &Path, identity: &WorkerIdentity) -> Result<()> {
    fs::write(
        data_dir.join(WORKER_IDENTITY_FILE),
        serde_json::to_vec_pretty(identity)?,
    )
    .context("saving Worker identity")
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

#[cfg(test)]
mod tests {
    use super::select_recovery_candidate;

    #[test]
    fn multiple_legacy_identities_require_an_explicit_choice() {
        let error = select_recovery_candidate(
            vec![
                "e33becfe-ee3f-4b7d-918c-8a5a5139f92c".to_owned(),
                "d01eb058-5df6-4e2d-aca7-3b535d85467d".to_owned(),
            ],
            None,
        )
        .expect_err("ambiguous identities must not be selected automatically")
        .to_string();

        assert!(error.contains("e33becfe-ee3f-4b7d-918c-8a5a5139f92c"));
        assert!(error.contains("d01eb058-5df6-4e2d-aca7-3b535d85467d"));
        assert!(error.contains("--recover-node-id"));
    }

    #[test]
    fn explicit_legacy_identity_is_selected() {
        let selected = select_recovery_candidate(
            vec![
                "e33becfe-ee3f-4b7d-918c-8a5a5139f92c".to_owned(),
                "d01eb058-5df6-4e2d-aca7-3b535d85467d".to_owned(),
            ],
            Some("e33becfe-ee3f-4b7d-918c-8a5a5139f92c"),
        )
        .expect("identity selection should succeed");

        assert_eq!(
            selected.as_deref(),
            Some("e33becfe-ee3f-4b7d-918c-8a5a5139f92c")
        );
    }
}
