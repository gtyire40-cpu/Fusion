use std::{
    io::{BufRead, BufReader},
    net::TcpListener,
    process::{Child, Command, Stdio},
    time::Duration,
};

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_ports() -> [u16; 3] {
    let listeners = (0..3)
        .map(|_| TcpListener::bind("127.0.0.1:0").expect("reserve a local port"))
        .collect::<Vec<_>>();
    [
        listeners[0].local_addr().expect("get local port").port(),
        listeners[1].local_addr().expect("get local port").port(),
        listeners[2].local_addr().expect("get local port").port(),
    ]
}

fn read_until<R: BufRead>(reader: &mut R, prefix: &str) -> String {
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).expect("read child output");
        assert_ne!(read, 0, "process exited before output matching {prefix:?}");
        if line.contains(prefix) {
            return line;
        }
    }
}

#[test]
fn worker_requires_approval_then_registers_with_mtls() -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = tempfile::tempdir()?;
    let worker_dir = tempfile::tempdir()?;
    let [pairing_port, node_port, ca_port] = free_ports();
    let binary = env!("CARGO_BIN_EXE_fusion");
    let mut master = ChildGuard(
        Command::new(binary)
            .args([
                "master",
                "start",
                "--advertise-host",
                "127.0.0.1",
                "--pairing-bind",
                &format!("127.0.0.1:{pairing_port}"),
                "--node-bind",
                &format!("127.0.0.1:{node_port}"),
                "--ca-bind",
                &format!("127.0.0.1:{ca_port}"),
                "--data-dir",
                data_dir.path().to_str().expect("UTF-8 temp path"),
            ])
            .stdout(Stdio::piped())
            .spawn()?,
    );
    let master_stdout = master.0.stdout.take().expect("capture Master output");
    let mut master_lines = BufReader::new(master_stdout);
    let fingerprint_line = read_until(&mut master_lines, "Master CA SHA-256 fingerprint:");
    let fingerprint = fingerprint_line
        .split_once(": ")
        .expect("fingerprint output format")
        .1
        .trim()
        .to_owned();

    let mut worker = ChildGuard(
        Command::new(binary)
            .args([
                "worker",
                "join",
                "127.0.0.1",
                "--fingerprint",
                &fingerprint,
                "--pairing-port",
                &pairing_port.to_string(),
                "--node-port",
                &node_port.to_string(),
                "--ca-port",
                &ca_port.to_string(),
                "--name",
                "integration-worker",
                "--approval-timeout-secs",
                "30",
                "--data-dir",
                worker_dir.path().to_str().expect("UTF-8 temp path"),
            ])
            .stdout(Stdio::piped())
            .spawn()?,
    );
    let worker_stdout = worker.0.stdout.take().expect("capture Worker output");
    let mut worker_lines = BufReader::new(worker_stdout);
    let request_line = read_until(&mut worker_lines, "Worker ID:");
    let node_id = request_line
        .split_once("Worker ID: ")
        .expect("Worker ID output format")
        .1
        .trim()
        .to_owned();

    let pending = Command::new(binary)
        .args([
            "master",
            "pending",
            "--data-dir",
            data_dir.path().to_str().expect("UTF-8 temp path"),
        ])
        .output()?;
    assert!(pending.status.success());
    assert!(String::from_utf8(pending.stdout)?.contains(&node_id));
    let unapproved_nodes = Command::new(binary)
        .args([
            "nodes",
            "list",
            "--data-dir",
            data_dir.path().to_str().expect("UTF-8 temp path"),
        ])
        .output()?;
    assert!(unapproved_nodes.status.success());
    assert!(!String::from_utf8(unapproved_nodes.stdout)?.contains(&node_id));

    let approval = Command::new(binary)
        .args([
            "master",
            "approve",
            &node_id,
            "--data-dir",
            data_dir.path().to_str().expect("UTF-8 temp path"),
        ])
        .output()?;
    assert!(
        approval.status.success(),
        "Master failed to approve worker: {}",
        String::from_utf8_lossy(&approval.stderr)
    );
    read_until(&mut worker_lines, "Worker registered and connected.");

    let nodes = Command::new(binary)
        .args([
            "nodes",
            "list",
            "--data-dir",
            data_dir.path().to_str().expect("UTF-8 temp path"),
        ])
        .output()?;
    assert!(nodes.status.success());
    let nodes = String::from_utf8(nodes.stdout)?;
    assert!(nodes.contains(&node_id));
    assert!(nodes.contains("online"));

    worker.0.kill()?;
    worker.0.wait()?;
    std::thread::sleep(Duration::from_secs(16));
    let nodes = Command::new(binary)
        .args([
            "nodes",
            "list",
            "--data-dir",
            data_dir.path().to_str().expect("UTF-8 temp path"),
        ])
        .output()?;
    assert!(nodes.status.success());
    assert!(String::from_utf8(nodes.stdout)?.contains("offline"));
    master.0.kill()?;
    master.0.wait()?;
    Ok(())
}
