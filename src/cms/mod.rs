// SPDX-License-Identifier: AGPL-3.0-only
//! DNA-DB CMS Edition — compatibility primitives for WordPress / Joomla.
//!
//! This module lives behind the `cms` Cargo feature and is developed on the
//! `cms-edition` branch. The goal of the CMS edition is to let popular
//! PHP CMS platforms (WordPress 6.x, Joomla 5.x) run against DNA-DB with
//! minimal or no application changes.
//!
//! What lives here (scaffold, grows per docs/CMS_EDITION_RU.md):
//! - [`errno`] — MySQL server error codes + SQLSTATE classes that CMS code
//!   (wpdb, dbDelta, WooCommerce retry loops) branches on. The compat layer
//!   must report these, not generic engine errors.
//! - [`session`] — per-connection state every MySQL-dialect client assumes:
//!   `LAST_INSERT_ID()`, `FOUND_ROWS()`, `SET NAMES`, `SET sql_mode`,
//!   autocommit.
//! - [`locks`] — named advisory locks with `GET_LOCK()` / `RELEASE_LOCK()` /
//!   `IS_FREE_LOCK()` / `IS_USED_LOCK()` semantics (used by WooCommerce
//!   stock/coupon flows and many plugins).
//! - [`row_locks`] — row-level locks for `SELECT … FOR UPDATE` with lock
//!   waits (errno 1205), `NOWAIT` (3572) and deadlock detection (1213) via
//!   a waits-for graph — the serialization WooCommerce stock paths need.
//!
//! What intentionally does NOT live here yet: the SQL dialect translator
//! (extends `sql_to_dnaql`), AUTO_INCREMENT sequences, collations, the
//! MySQL wire protocol, and the PHP drop-in kits (WordPress `db.php`,
//! Joomla `DatabaseDriver`). See docs/CMS_EDITION_RU.md for the phases.
//!
//! Build: `cargo build --release --features cms`

pub mod compat;
pub mod ddl_meta;
pub mod errno;
pub mod locks;
pub mod mysqlwire;
pub mod panel;
pub mod row_locks;
pub mod sequence;
pub mod session;
pub mod spirals;
pub mod wordpress;

pub use compat::{CmsError, CmsQueryResult, CmsSqlGate};
pub use ddl_meta::{CmsColumnMeta, CmsIndexMeta, CmsTableMeta};
pub use locks::LockRegistry;
pub use mysqlwire::{MySqlAuth, MySqlWireServer};
pub use panel::CmsPanel;
pub use row_locks::RowLockRegistry;
pub use sequence::AutoIncrementRegistry;
pub use spirals::{SpiralCatalog, SpiralPair, StorageKind, StorageRoot};
pub use session::{CmsSession, Charset, SqlModeFlag};

/// Human-readable edition banner for `--version` output and startup logs.
pub fn edition_banner() -> String {
    format!(
        "DNA-DB CMS Edition (feature: cms) — WordPress/Joomla compatibility track, base {}",
        env!("CARGO_PKG_VERSION")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_mentions_edition() {
        let b = edition_banner();
        assert!(b.contains("CMS Edition"));
        assert!(b.contains(env!("CARGO_PKG_VERSION")));
    }
}
