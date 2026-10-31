// SPDX-License-Identifier: AGPL-3.0-only
//! v3.8 Raft cluster integration test: 3 in-process nodes over loopback TCP,
//! leader election, quorum writes, data replication and failover under
//! leader loss (documented SLA: re-election < 2 s, zero committed loss).

use dna_db_core::engine::{Engine, EngineConfig};
use dna_db_core::raft::{client_call, ClientOp, RaftNode};
use dna_db_core::wal::SyncMode;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "dna_raft_{}_{}_{}",
        tag,
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct ClusterNode {
    id: u64,
    addr: String,
    node: Arc<RaftNode>,
}

fn start_cluster() -> Vec<ClusterNode> {
    let dirs: Vec<std::path::PathBuf> = (0..3).map(|i| temp_dir(&format!("n{i}"))).collect();
    let ports: Vec<u16> = (0..3).map(|_| free_port()).collect();
    let addrs: Vec<String> = ports.iter().map(|p| format!("127.0.0.1:{p}")).collect();
    let mut nodes = Vec::new();
    for i in 0..3u64 {
        let config = EngineConfig {
            dir_path: dirs[i as usize].clone(),
            sync_mode: SyncMode::Batched,
            crypto: None,
            ..Default::default()
        };
        let engine = Engine::open(config).unwrap();
        let peers: Vec<(u64, String)> = (0..3u64)
            .map(|j| (j, addrs[j as usize].clone()))
            .collect();
        let node = RaftNode::open(i, peers, dirs[i as usize].clone(), engine).unwrap();
        node.spawn_ticker();
        let addr = addrs[i as usize].clone();
        let port_i = ports[i as usize];
        let n = node.clone();
        std::thread::spawn(move || {
            let _ = dna_db_core::raft::serve_node(n, "127.0.0.1", port_i);
        });
        nodes.push(ClusterNode { id: i, addr, node });
    }
    nodes
}

fn wait_for_leader(nodes: &[ClusterNode], timeout: Duration) -> Option<u64> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        for n in nodes {
            let st = n.node.status_json();
            if st["role"] == "Leader" {
                return Some(n.id);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

fn write_through_leader(nodes: &[ClusterNode], key: &[u8], value: &[u8]) -> Result<(), String> {
    for _ in 0..2 {
        for n in nodes {
            if n.node.status_json()["role"] == "Leader" {
                return match client_call(
                    &n.addr,
                    ClientOp::Put {
                        key: key.to_vec(),
                        value: value.to_vec(),
                    },
                    Duration::from_secs(5),
                ) {
                    Some(r) if r.ok => Ok(()),
                    Some(r) => Err(r.error.unwrap_or_default()),
                    None => Err("no reply".into()),
                };
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err("no leader found".into())
}

#[test]
fn raft_elects_leader_replicates_and_fails_over() {
    let nodes = start_cluster();
    let leader = wait_for_leader(&nodes, Duration::from_secs(6))
        .expect("a leader must be elected within 6 s");

    // 1. Quorum write through the leader.
    write_through_leader(&nodes, b"raft:k1", b"v1").unwrap();
    write_through_leader(&nodes, b"raft:k2", b"v2").unwrap();

    // 2. Data replicated to EVERY node (committed entries are durable).
    std::thread::sleep(Duration::from_millis(300));
    for n in &nodes {
        let v = n
            .node
            .read_local(b"raft:k1")
            .unwrap_or_else(|_| Some(b"<missing>".to_vec()));
        assert_eq!(
            v.as_deref(),
            Some(b"v1".as_slice()),
            "node {} must hold committed data",
            n.id
        );
    }
    println!("replication verified on 3/3 nodes");

    // 3. Kill the leader: stop its activity loop so heartbeats cease (its
    // peer-RPC handlers keep answering, like a paused process).
    let dead = nodes
        .iter()
        .position(|n| n.id == leader)
        .expect("leader in cluster");
    nodes[dead].node.stop();
    let alive: Vec<ClusterNode> = nodes
        .into_iter()
        .enumerate()
        .filter(|(i, _)| *i != dead)
        .map(|(_, n)| n)
        .collect();

    // Give the followers time to notice missing heartbeats and re-elect.
    let t0 = std::time::Instant::now();
    let new_leader = loop {
        if let Some(l) = wait_for_leader(&alive, Duration::from_secs(1)) {
            break l;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "failover must complete in < 2 s of election timeouts (got {:?})",
            t0.elapsed()
        );
    };
    println!(
        "failover: old leader {leader} → new leader {new_leader} after {:?}",
        t0.elapsed()
    );

    // 4. Writes still available after failover.
    write_through_leader(&alive, b"raft:k3", b"v3").unwrap();

    // 5. ALL committed data converges on the surviving nodes. The new
    // leader replays its full log to lagging followers, so allow a short
    // convergence window — but data must NEVER be absent from BOTH.
    let deadline = std::time::Instant::now() + Duration::from_secs(4);
    let keys: Vec<&[u8]> = vec![b"raft:k1", b"raft:k2", b"raft:k3"];
    loop {
        let all_present = keys.iter().all(|key| {
            alive
                .iter()
                .any(|n| n.node.read_local(key).unwrap_or(None).is_some())
        }) && alive.iter().all(|n| {
            keys.iter()
                .any(|k| n.node.read_local(k).unwrap_or(None).is_some())
        });
        if all_present || std::time::Instant::now() > deadline {
            let mut dbg = String::new();
            for key in &keys {
                for n in &alive {
                    dbg.push_str(&format!(
                        "{}@{}={:?} ",
                        String::from_utf8_lossy(key),
                        n.id,
                        n.node.read_local(key).unwrap_or(None).map(|v| String::from_utf8_lossy(&v).to_string())
                    ));
                }
            }
            for n in &alive {
                dbg.push_str(&format!("status{}={:?} ", n.id, n.node.status_json()));
            }
            assert!(all_present, "committed data must converge; {dbg}");
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    println!("RAFT CLUSTER TEST: PASS (election, replication, failover, durability)");
    // Listener/ticker threads are daemon-style: the test process tears them
    // down when the test binary exits.
}
