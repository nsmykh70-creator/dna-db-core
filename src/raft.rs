// SPDX-License-Identifier: AGPL-3.0-only
//! Distributed Raft consensus (v3.8 "Distributed Raft Consensus").
//!
//! A single Raft group replicates the engine's write stream across 3+ nodes:
//!   * leader election with randomised timeouts (heartbeat 300 ms,
//!     election timeout 800–1500 ms → failover well under 2 s);
//!   * log replication with nextIndex/matchIndex backtracking;
//!   * commit when the entry is on a majority, then apply to the
//!     DNA-DB engine (put/delete through one WAL batch per entry);
//!   * persistent state: current_term + voted_for + log in
//!     `DATA/raft/state.json` (crash-safe via write-then-fsync);
//!   * clients talk to ANY node: followers redirect to the leader.
//!
//! Honest scope (documented in RELEASE_NOTES_v4.0.0): single Raft group
//! (no sharding yet), no log compaction/snapshotting (the log is the
//! authoritative replicated stream and is compacted by checkpointing the
//! engine externally), linearisable writes, leader reads.
//!
//! Wire format: length-prefixed JSON frames over TCP.
//!   0x01 + u32 len + json  — peer RPC (RequestVote / AppendEntries)
//!   0x02 + u32 len + json  — client command  (Put / Delete / Status)

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestVoteArgs {
    pub term: u64,
    pub candidate_id: u64,
    pub last_log_index: u64,
    pub last_log_term: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestVoteReply {
    pub term: u64,
    pub vote_granted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppendEntriesArgs {
    pub term: u64,
    pub leader_id: u64,
    pub prev_log_index: u64,
    pub prev_log_term: u64,
    pub entries: Vec<LogEntry>,
    pub leader_commit: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppendEntriesReply {
    pub term: u64,
    pub success: bool,
    /// Hint for fast nextIndex backtracking (conflict index).
    pub conflict_index: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub term: u64,
    pub index: u64,
    pub op: RaftOp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RaftOp {
    Put { key: Vec<u8>, value: Vec<u8> },
    Delete { key: Vec<u8> },
    Noop,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientRequest {
    pub op: ClientOp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientOp {
    Put { key: Vec<u8>, value: Vec<u8> },
    Delete { key: Vec<u8> },
    Get { key: Vec<u8> },
    Status,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientReply {
    pub ok: bool,
    pub leader: Option<u64>,
    pub value: Option<Vec<u8>>,
    pub error: Option<String>,
    pub status: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Persistent state
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Default, Clone)]
struct PersistedState {
    current_term: u64,
    voted_for: Option<u64>,
    log: Vec<LogEntry>,
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
struct RaftCore {
    id: u64,
    peers: Vec<(u64, String)>, // id -> host:port
    data_dir: std::path::PathBuf,

    /// Guarded by the outer Mutex of RaftNode.
    state: PersistedState,
    commit_index: u64,
    last_applied: u64,
    role: Role,
    leader_id: Option<u64>,
    /// Per-follower replication cursors (leader only).
    next_index: HashMap<u64, u64>,
    match_index: HashMap<u64, u64>,
    votes_granted: std::collections::HashSet<u64>,
    last_heartbeat: Instant,
    election_deadline: Instant,
    /// Entries applied but not yet acked to a waiting client.
    pending_client: Option<(u64, std::sync::mpsc::SyncSender<Result<(), String>>)>,
    stats_writes: Arc<AtomicU64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Follower,
    Candidate,
    Leader,
}

impl RaftCore {
    fn state_path(&self) -> std::path::PathBuf {
        self.data_dir.join("raft").join("state.json")
    }

    fn persist(&self) -> Result<(), String> {
        let path = self.state_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_vec(&self.state).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json).map_err(|e| e.to_string())?;
        let f = std::fs::File::open(&tmp).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn last_log_index(&self) -> u64 {
        self.state.log.last().map(|e| e.index).unwrap_or(0)
    }

    fn last_log_term(&self) -> u64 {
        self.state.log.last().map(|e| e.term).unwrap_or(0)
    }

    fn entry_term(&self, index: u64) -> u64 {
        if index == 0 {
            return 0;
        }
        let idx = index as usize - 1;
        self.state.log.get(idx).map(|e| e.term).unwrap_or(0)
    }

    fn reset_election_timer(&mut self) {
        self.last_heartbeat = Instant::now();
        // Node-id-stirred jitter: wall-clock-only jitter is identical on
        // nodes started in the same millisecond and caused permanent split
        // votes in the very first 3-node run.
        let jitter = (now_millis().wrapping_add(self.id.wrapping_mul(173)) % 700) as u64;
        self.election_deadline = Instant::now() + Duration::from_millis(800 + jitter);
    }
}

// ---------------------------------------------------------------------------
// RaftNode — the thread-safe facade
// ---------------------------------------------------------------------------

pub struct RaftNode {
    core: Mutex<RaftCore>,
    engine: Arc<crate::engine::Engine>,
    stats_writes: Arc<AtomicU64>,
    /// Cooperative shutdown for the ticker (tests / graceful node removal).
    stop_flag: std::sync::atomic::AtomicBool,
}

impl RaftNode {
    pub fn open(
        id: u64,
        peers: Vec<(u64, String)>,
        data_dir: std::path::PathBuf,
        engine: Arc<crate::engine::Engine>,
    ) -> Result<Arc<Self>, String> {
        let state = match std::fs::read(data_dir.join("raft").join("state.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| e.to_string())?,
            Err(_) => PersistedState::default(),
        };
        let stats_writes = Arc::new(AtomicU64::new(0));
        let mut core = RaftCore {
            id,
            peers,
            data_dir,
            state,
            commit_index: 0,
            last_applied: 0,
            role: Role::Follower,
            leader_id: None,
            next_index: HashMap::new(),
            match_index: HashMap::new(),
            votes_granted: std::collections::HashSet::new(),
            last_heartbeat: Instant::now(),
            election_deadline: Instant::now() + Duration::from_millis(1000),
            pending_client: None,
            stats_writes: stats_writes.clone(),
        };
        core.reset_election_timer();
        let node = Arc::new(Self {
            core: Mutex::new(core),
            engine,
            stats_writes,
            stop_flag: std::sync::atomic::AtomicBool::new(false),
        });
        Ok(node)
    }

    /// Background ticker: drives elections, heartbeats and replication.
    pub fn spawn_ticker(self: &Arc<Self>) {
        let node = self.clone();
        std::thread::Builder::new()
            .name("raft-ticker".into())
            .spawn(move || loop {
                if node.stop_flag.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
                node.tick();
            })
            .expect("spawn raft ticker");
    }

    /// Stop this node's activity loop (crash simulation for tests, graceful
    /// removal for ops). Peer RPC handlers keep answering so a restarting
    /// node can rejoin, but this node no longer campaigns or replicates.
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::Relaxed);
    }

    fn tick(self: &Arc<Self>) {
        let (need_election, _elapsed) = {
            let core = self.core.lock().unwrap();
            let need = core.role != Role::Leader && Instant::now() >= core.election_deadline;
            (need, core.last_heartbeat.elapsed())
        };
        if need_election {
            self.start_election();
        }
        // Leader: replicate to everyone + advance commit.
        let is_leader = self.core.lock().unwrap().role == Role::Leader;
        if is_leader {
            self.replicate_all();
            self.advance_commit();
        }
    }

    fn start_election(self: &Arc<Self>) {
        let (term, last_idx, last_term, n_peers) = {
            let mut core = self.core.lock().unwrap();
            core.role = Role::Candidate;
            core.state.current_term += 1;
            core.leader_id = None;
            let self_id = core.id;
            core.votes_granted.clear();
            core.votes_granted.insert(self_id);
            core.reset_election_timer();
            (
                core.state.current_term,
                core.last_log_index(),
                core.last_log_term(),
                core.peers.len() + 1,
            )
        };
        eprintln!("[RAFT] node {} starting election for term {term}", self.id_str());
        let majority = n_peers / 2 + 1;
        for (peer_id, addr) in self.peer_list() {
            if peer_id == self.node_id() {
                continue; // peers may include self; never vote-request to self
            }
            let node = Arc::clone(self);
            let args = RequestVoteArgs {
                term,
                candidate_id: self.node_id(),
                last_log_index: last_idx,
                last_log_term: last_term,
            };
            let node = self.clone();
            std::thread::spawn(move || {
                let reply: Option<RequestVoteReply> =
                    rpc_call(&addr, "request_vote", &args, Duration::from_millis(400));
                if let Some(r) = reply {
                    node.handle_vote_reply(peer_id, r, term, majority);
                }
            });
        }
    }

    fn handle_vote_reply(&self, peer: u64, reply: RequestVoteReply, term: u64, majority: usize) {
        let mut core = self.core.lock().unwrap();
        if core.role != Role::Candidate || core.state.current_term != term {
            return;
        }
        if reply.term > core.state.current_term {
            core.state.current_term = reply.term;
            core.role = Role::Follower;
            core.state.voted_for = None;
            let _ = core.persist();
            core.reset_election_timer();
            return;
        }
        if reply.vote_granted {
            core.votes_granted.insert(peer);
            if core.votes_granted.len() >= majority {
                core.role = Role::Leader;
                core.leader_id = Some(core.id);
                let next = core.last_log_index() + 1;
                let peer_ids: Vec<u64> = core.peers.iter().map(|(pid, _)| *pid).collect();
                core.next_index.clear();
                core.match_index.clear();
                for pid in peer_ids {
                    core.next_index.insert(pid, next);
                    core.match_index.insert(pid, 0);
                }
                eprintln!(
                    "[RAFT] node {} became LEADER for term {}",
                    core.id, term
                );
                // Commit a no-op for this term so the commit index advances.
                core.state.log.push(LogEntry {
                    term,
                    index: next,
                    op: RaftOp::Noop,
                });
                let _ = core.persist();
                drop(core);
                self.replicate_all();
            }
        }
    }

    // ----- peer RPC handlers (called from the accept loop) -----

    pub fn handle_request_vote(&self, args: RequestVoteArgs) -> RequestVoteReply {
        let mut core = self.core.lock().unwrap();
        if args.term > core.state.current_term {
            core.state.current_term = args.term;
            core.role = Role::Follower;
            core.state.voted_for = None;
            core.reset_election_timer();
        }
        let up_to_date = (args.last_log_term, args.last_log_index)
            >= (core.last_log_term(), core.last_log_index());
        let grant = args.term == core.state.current_term
            && up_to_date
            && core.state.voted_for.map(|v| v == args.candidate_id).unwrap_or(true);
        if grant {
            core.state.voted_for = Some(args.candidate_id);
            core.reset_election_timer();
            let _ = core.persist();
        }
        RequestVoteReply {
            term: core.state.current_term,
            vote_granted: grant,
        }
    }

    pub fn handle_append_entries(&self, args: AppendEntriesArgs) -> AppendEntriesReply {
        let mut core = self.core.lock().unwrap();
        if args.term < core.state.current_term {
            return AppendEntriesReply {
                term: core.state.current_term,
                success: false,
                conflict_index: 0,
            };
        }
        // Valid leader for this term.
        core.role = Role::Follower;
        core.leader_id = Some(args.leader_id);
        core.reset_election_timer();
        if args.term > core.state.current_term {
            core.state.current_term = args.term;
            core.state.voted_for = None;
        }
        // Log consistency check.
        if args.prev_log_index > core.last_log_index()
            || core.entry_term(args.prev_log_index) != args.prev_log_term
        {
            let conflict = core.last_log_index().min(args.prev_log_index);
            return AppendEntriesReply {
                term: core.state.current_term,
                success: false,
                conflict_index: conflict,
            };
        }
        // Append / overwrite conflicting suffix.
        for entry in args.entries {
            let idx = entry.index as usize;
            if idx <= core.state.log.len() {
                if core.state.log.get(idx).map(|e| e.term) == Some(entry.term) {
                    continue; // already have it
                }
                core.state.log.truncate(idx);
            }
            core.state.log.push(entry);
        }
        let _ = core.persist();
        if args.leader_commit > core.commit_index {
            core.commit_index = args.leader_commit.min(core.last_log_index());
        }
        let reply = AppendEntriesReply {
            term: core.state.current_term,
            success: true,
            conflict_index: 0,
        };
        drop(core);
        self.apply_committed();
        reply
    }

    // ----- replication -----

    fn replicate_all(&self) {
        let me = self.node_id();
        let targets: Vec<(u64, String)> = self
            .peer_list()
            .into_iter()
            .filter(|(pid, _)| *pid != me)
            .collect();
        for (peer_id, addr) in targets {
            self.replicate_to(peer_id, addr);
        }
    }

    fn replicate_to(&self, peer_id: u64, addr: String) {
        loop {
            let (args, term) = {
                let mut core = self.core.lock().unwrap();
                if core.role != Role::Leader {
                    return;
                }
                let next = *core.next_index.get(&peer_id).unwrap_or(&1);
                let prev = next.saturating_sub(1);
                // Entry with index i lives at log[i-1]: entries FROM `next`
                // therefore start at slice position next-1 (the previous
                // code started one entry late, so the newest write never
                // reached followers and never committed).
                let entries: Vec<LogEntry> = core.state.log
                    [prev.min(core.state.log.len() as u64) as usize..]
                    .to_vec();
                (
                    AppendEntriesArgs {
                        term: core.state.current_term,
                        leader_id: core.id,
                        prev_log_index: prev,
                        prev_log_term: core.entry_term(prev),
                        entries,
                        leader_commit: core.commit_index,
                    },
                    core.state.current_term,
                )
            };
            let reply: Option<AppendEntriesReply> =
                rpc_call(&addr, "append_entries", &args, Duration::from_millis(600));
            let Some(reply) = reply else {
                eprintln!(
                    "[RAFT] heartbeat to node {peer_id} ({addr}) failed/no reply"
                );
                return;
            };
            let mut core = self.core.lock().unwrap();
            if core.role != Role::Leader || core.state.current_term != term {
                return;
            }
            if reply.term > core.state.current_term {
                core.state.current_term = reply.term;
                core.role = Role::Follower;
                core.state.voted_for = None;
                let _ = core.persist();
                core.reset_election_timer();
                return;
            }
            if reply.success {
                let sent_last = args.prev_log_index + args.entries.len() as u64;
                core.match_index.insert(peer_id, sent_last);
                core.next_index.insert(peer_id, sent_last + 1);
                return; // caught up (this round)
            }
            // Backtrack.
            let back = if reply.conflict_index > 0 && reply.conflict_index < args.prev_log_index {
                reply.conflict_index
            } else {
                args.prev_log_index.saturating_sub(1)
            };
            core.next_index.insert(peer_id, back.max(1));
            if back >= args.prev_log_index {
                return; // no progress possible this tick
            }
        }
    }

    fn advance_commit(&self) {
        let (n, current_term) = {
            let core = self.core.lock().unwrap();
            (core.peers.len() + 1, core.state.current_term)
        };
        let majority = n / 2 + 1;
        let mut new_commit = None;
        {
            let mut core = self.core.lock().unwrap();
            if core.role != Role::Leader {
                return;
            }
            // Try to commit entries from the current term (Raft §5.4.2).
            for idx in (core.commit_index + 1..=core.last_log_index()).rev() {
                if core.entry_term(idx) != current_term {
                    continue;
                }
                let replicated = core
                    .match_index
                    .values()
                    .filter(|&&m| m >= idx)
                    .count()
                    + 1; // self
                if replicated >= majority {
                    core.commit_index = idx;
                    new_commit = Some(idx);
                    break;
                }
            }
        }
        if new_commit.is_some() {
            self.apply_committed();
        }
    }

    fn apply_committed(&self) {
        loop {
            let (idx, op, waiter) = {
                let mut core = self.core.lock().unwrap();
                if core.last_applied >= core.commit_index {
                    return;
                }
                let idx = core.last_applied + 1;
                let Some(entry) = core.state.log.get(idx as usize - 1) else {
                    return;
                };
                let op = entry.op.clone();
                let waiter = core.pending_client.take().and_then(|(want, tx)| {
                    if want == idx {
                        Some(tx)
                    } else {
                        // Not this waiter's entry yet — put it back.
                        core.pending_client = Some((want, tx));
                        None
                    }
                });
                (idx, op, waiter)
            };
            let result = match &op {
                RaftOp::Put { key, value } => self
                    .engine
                    .write_batch(&[(crate::wal::WalOp::Put, key.clone(), Some(value.clone()))])
                    .map(|_| ()),
                RaftOp::Delete { key } => self
                    .engine
                    .write_batch(&[(crate::wal::WalOp::Delete, key.clone(), None)])
                    .map(|_| ()),
                RaftOp::Noop => Ok(()),
            };
            let send_result: Result<(), String> = match &result {
                Ok(()) => Ok(()),
                Err(e) => Err(e.to_string()),
            };
            if let Err(e) = &result {
                eprintln!("[RAFT] apply error at index {idx}: {e}");
            } else if !matches!(op, RaftOp::Noop) {
                self.stats_writes.fetch_add(1, Ordering::Relaxed);
            }
            {
                let mut core = self.core.lock().unwrap();
                core.last_applied = idx;
            }
            if let Some(waiter) = waiter {
                let _ = waiter.send(send_result);
            }
        }
    }

    // ----- client surface -----

    /// Submit a write; blocks until committed by a majority (or fails).
    pub fn client_write(&self, op: RaftOp) -> Result<(), String> {
        let (term, index, rx) = {
            let mut core = self.core.lock().unwrap();
            if core.role != Role::Leader {
                return Err(format!(
                    "NOT_LEADER:{:?}",
                    core.leader_id.map(|i| i.to_string()).unwrap_or_default()
                ));
            }
            let index = core.last_log_index() + 1;
            let term = core.state.current_term;
            core.state.log.push(LogEntry {
                term,
                index,
                op,
            });
            let _ = core.persist();
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            core.pending_client = Some((index, tx));
            (core.state.current_term, index, rx)
        };
        let _ = term;
        // Kick replication immediately (do not wait for the ticker).
        self.replicate_all();
        self.advance_commit();
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("raft: commit timeout (no majority ack within 5s)".into()),
        }
    }

    /// Local (follower-safe) read of the applied state machine. Committed
    /// entries are durable on every node, so this is the replication check
    /// used by tests and `cluster status`.
    pub fn read_local(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        self.engine.get(key).map_err(|e| e.to_string())
    }

    pub fn client_read(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        let is_leader = self.core.lock().unwrap().role == Role::Leader;
        if !is_leader {
            return Err("NOT_LEADER:".into());
        }
        self.engine.get(key).map_err(|e| e.to_string())
    }

    pub fn status_json(&self) -> serde_json::Value {
        let core = self.core.lock().unwrap();
        serde_json::json!({
            "id": core.id,
            "role": format!("{:?}", core.role),
            "term": core.state.current_term,
            "leader": core.leader_id,
            "commit_index": core.commit_index,
            "last_applied": core.last_applied,
            "log_len": core.state.log.len(),
            "writes": self.stats_writes.load(Ordering::Relaxed),
        })
    }

    // ----- small helpers -----

    fn node_id(&self) -> u64 {
        self.core.lock().unwrap().id
    }

    fn id_str(&self) -> String {
        self.core.lock().unwrap().id.to_string()
    }

    fn peer_list(&self) -> Vec<(u64, String)> {
        self.core.lock().unwrap().peers.clone()
    }
}

// ---------------------------------------------------------------------------
// Networking
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize)]
struct RpcEnvelope {
    method: String,
    payload: serde_json::Value,
}

fn write_frame(stream: &mut TcpStream, kind: u8, bytes: &[u8]) -> std::io::Result<()> {
    let mut framed = vec![kind];
    framed.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    framed.extend_from_slice(bytes);
    stream.write_all(&framed)
}

fn read_frame(stream: &mut TcpStream) -> std::io::Result<(u8, Vec<u8>)> {
    let mut kind = [0u8; 1];
    stream.read_exact(&mut kind)?;
    let mut len = [0u8; 4];
    stream.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > 64 * 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    Ok((kind[0], buf))
}

fn rpc_call<A: Serialize, R: for<'de> Deserialize<'de>>(
    addr: &str,
    method: &str,
    args: &A,
    timeout: Duration,
) -> Option<R> {
    let mut stream = TcpStream::connect(addr).ok()?;
    stream.set_nodelay(true).ok();
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();
    let envelope = RpcEnvelope {
        method: method.to_string(),
        payload: serde_json::to_value(args).ok()?,
    };
    write_frame(&mut stream, 0x01, &serde_json::to_vec(&envelope).ok()?).ok()?;
    let (_, reply) = read_frame(&mut stream).ok()?;
    serde_json::from_slice(&reply).ok()
}

/// Accept loop for a cluster node: muxes peer RPC (0x01) and client
/// commands (0x02) on one port.
pub fn serve_node(node: Arc<RaftNode>, bind: &str, port: u16) -> std::io::Result<()> {
    let listener = TcpListener::bind((bind, port))?;
    eprintln!("[RAFT] node {} listening on {bind}:{port}", node.node_id());
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let node = node.clone();
        std::thread::spawn(move || {
            stream.set_nodelay(true).ok();
            stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
            let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
            loop {
                let (kind, buf) = match read_frame(&mut stream) {
                    Ok(f) => f,
                    Err(_) => return,
                };
                match kind {
                    0x01 => {
                        let Ok(env) = serde_json::from_slice::<RpcEnvelope>(&buf) else {
                            return;
                        };
                        let reply: serde_json::Value = match env.method.as_str() {
                            "request_vote" => serde_json::to_value(node.handle_request_vote(
                                serde_json::from_value(env.payload).unwrap(),
                            ))
                            .unwrap(),
                            "append_entries" => serde_json::to_value(node.handle_append_entries(
                                serde_json::from_value(env.payload).unwrap(),
                            ))
                            .unwrap(),
                            other => {
                                serde_json::json!({ "error": format!("unknown method {other}") })
                            }
                        };
                        if write_frame(&mut stream, kind, &serde_json::to_vec(&reply).unwrap())
                            .is_err()
                        {
                            return;
                        }
                    }
                    0x02 => {
                        let reply = match serde_json::from_slice::<ClientRequest>(&buf) {
                            Ok(req) => handle_client(&node, req),
                            Err(e) => ClientReply {
                                ok: false,
                                leader: None,
                                value: None,
                                error: Some(format!("bad request: {e}")),
                                status: None,
                            },
                        };
                        if write_frame(&mut stream, kind, &serde_json::to_vec(&reply).unwrap())
                            .is_err()
                        {
                            return;
                        }
                    }
                    _ => return,
                }
            }
        });
    }
    Ok(())
}

fn handle_client(node: &Arc<RaftNode>, req: ClientRequest) -> ClientReply {
    match req.op {
        ClientOp::Put { key, value } => match node.client_write(RaftOp::Put { key, value }) {
            Ok(()) => ClientReply {
                ok: true,
                leader: Some(node.node_id()),
                value: None,
                error: None,
                status: None,
            },
            Err(e) if e.starts_with("NOT_LEADER") => ClientReply {
                ok: false,
                leader: e
                    .strip_prefix("NOT_LEADER:")
                    .and_then(|s| s.parse::<u64>().ok()),
                value: None,
                error: Some("not leader".into()),
                status: None,
            },
            Err(e) => ClientReply {
                ok: false,
                leader: Some(node.node_id()),
                value: None,
                error: Some(e),
                status: None,
            },
        },
        ClientOp::Delete { key } => match node.client_write(RaftOp::Delete { key }) {
            Ok(()) => ClientReply {
                ok: true,
                leader: Some(node.node_id()),
                value: None,
                error: None,
                status: None,
            },
            Err(e) => ClientReply {
                ok: false,
                leader: None,
                value: None,
                error: Some(e),
                status: None,
            },
        },
        ClientOp::Get { key } => match node.client_read(&key) {
            Ok(v) => ClientReply {
                ok: true,
                leader: Some(node.node_id()),
                value: v,
                error: None,
                status: None,
            },
            Err(e) => ClientReply {
                ok: false,
                leader: None,
                value: None,
                error: Some(e),
                status: None,
            },
        },
        ClientOp::Status => ClientReply {
            ok: true,
            leader: None,
            value: None,
            error: None,
            status: Some(node.status_json()),
        },
    }
}

/// Convenience: send a client command to a node address.
pub fn client_call(addr: &str, op: ClientOp, timeout: Duration) -> Option<ClientReply> {
    let mut stream = TcpStream::connect(addr).ok()?;
    stream.set_nodelay(true).ok();
    stream.set_read_timeout(Some(timeout)).ok();
    let req = serde_json::to_vec(&ClientRequest { op }).ok()?;
    write_frame(&mut stream, 0x02, &req).ok()?;
    let (_, reply) = read_frame(&mut stream).ok()?;
    serde_json::from_slice(&reply).ok()
}
