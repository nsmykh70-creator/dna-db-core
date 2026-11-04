// SPDX-License-Identifier: AGPL-3.0-only
//! One-time bootstrap seeding performed on first gate use (WordPress
//! options/version baseline).

use super::*;
use crate::cms::ddl_meta::CmsTableMeta;
use crate::cms::wordpress;

impl CmsSqlGate {
    /// Creates the WordPress core schema: overlay metadata for all 12 core
    /// tables + engine tables where the engine has none yet. Single-column
    /// secondary indexes are created in the engine; composite keys stay
    /// overlay-only (engine single-PK limitation, CMS_EDITION_RU.md §4).
    /// Returns the number of engine tables actually created.
    pub fn bootstrap_wordpress(&self) -> Result<usize, String> {
        let mut created = 0usize;
        for (name, ddl) in wordpress::WP_CORE_TABLES {
            let meta = CmsTableMeta::parse_create_table(ddl)?;
            if self.engine.get_schema(name).is_none() {
                self.engine.execute(&meta.normalized_engine_ddl())?;
                created += 1;
                // Engine-level indexes for single-column keys only.
                for idx in &meta.indexes {
                    if idx.columns.len() == 1 {
                        let col = &idx.columns[0].0;
                        let kind = if idx.unique { "UNIQUE INDEX" } else { "INDEX" };
                        let stmt = format!(
                            "CREATE {} IF NOT EXISTS {} ON {} ({})",
                            kind, idx.name, name, col
                        );
                        if let Err(e) = self.engine.execute(&stmt) {
                            // Index creation is best-effort at bootstrap;
                            // overlays keep the catalog truth either way.
                            eprintln!("[cms] index {} on {}: {}", idx.name, name, e);
                        }
                    }
                }
            }
            self.register_table_meta(meta);
        }
        Ok(created)
    }
}
