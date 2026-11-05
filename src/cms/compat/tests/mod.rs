// SPDX-License-Identifier: AGPL-3.0-only

//! Gate-level tests: session state, catalog overlay, dialect rewrites.

use crate::cms::wordpress;

use super::*;
use crate::engine::{Engine, EngineConfig};
use crate::wal::SyncMode;
use std::sync::Arc;

fn gate(name: &str) -> CmsSqlGate {
    let dir = std::env::temp_dir().join(format!(
        "dnadb_cms_{}_{}_{}",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let cfg = EngineConfig {
        dir_path: dir,
        sync_mode: SyncMode::Batched,
        ..EngineConfig::default()
    };
    let engine = Engine::open(cfg).unwrap();
    CmsSqlGate::new(Arc::new(SqlEngine::new(engine)))
}

fn v(row: &BTreeMap<String, Value>, k: &str) -> String {
    row.get(k).map(|v| v.to_string()).unwrap_or_default()
}

#[cfg(test)]
mod gate;
#[cfg(test)]
mod dump;
#[cfg(test)]
mod row_locks;
#[cfg(test)]
mod wire;
