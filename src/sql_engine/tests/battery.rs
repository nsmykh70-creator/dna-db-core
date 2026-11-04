// SPDX-License-Identifier: AGPL-3.0-only
//! Main sql_engine test battery (verbatim from the pre-split monolith).

use super::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::EngineConfig;
    use crate::wal::SyncMode;

    fn tempdir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "dna_sql_{}_{}",
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn setup() -> (Arc<Engine>, SqlEngine) {
        let engine = Engine::open(EngineConfig {
            dir_path: tempdir("sql"),
            memtable_capacity: 4 * 1024 * 1024,
            sync_mode: SyncMode::Strict,
            mirror_disk_path: None,
            ..Default::default()
        })
        .unwrap();
        let sql = SqlEngine::new(engine.clone());
        sql.execute("CREATE TABLE chr_users (user_id INT, username TEXT, balance FLOAT, PRIMARY KEY (user_id))").unwrap();
        sql.execute("CREATE TABLE chr_orders (order_id INT, user_id INT, total_usd FLOAT, status TEXT, PRIMARY KEY (order_id), FOREIGN KEY (user_id) REFERENCES chr_users(user_id) ON DELETE CASCADE)").unwrap();
        (engine, sql)
    }

    #[cfg(test)]
    mod aggregates_storage;
    #[cfg(test)]
    mod basics;
    #[cfg(test)]
    mod indexes_joins;
}
