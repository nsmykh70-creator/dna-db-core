// SPDX-License-Identifier: AGPL-3.0-only
//! Print the WordPress core preset DDL (cms::wordpress::WP_CORE_TABLES) as
//! runnable SQL — the single source of truth for the differential harness
//! (cms/harness/run_comparison.php) feeds the same statements to MySQL and
//! DNA-DB.
//!
//! Run: cargo run --features cms --example dump_wp_schema > cms/harness/fixtures_wp.sql

#[cfg(feature = "cms")]
fn main() {
    for (name, ddl) in dna_db_core::cms::wordpress::WP_CORE_TABLES {
        println!("-- preset table: {}", name);
        println!("{};", ddl.trim_end());
    }
}

#[cfg(not(feature = "cms"))]
fn main() {
    eprintln!("build with --features cms (the WP preset lives behind the cms feature)");
    std::process::exit(1);
}
