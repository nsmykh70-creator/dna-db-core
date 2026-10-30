// SPDX-License-Identifier: AGPL-3.0-only
//! Per-user table allowlists (a deliberately small, auditable ACL).
//!
//! `None` means "no restriction" and preserves the pre-v3.4 behaviour of
//! existing `users.json` files. `Some(list)` restricts a non-admin user to the
//! named tables; an empty list denies every table. Administrators always pass
//! unrestricted so a locked-out ACL cannot make the instance unmanageable.
//!
//! The ACL is enforced at both query surfaces (SQL and DnaQL) and on the web
//! panel. Raw key/value access is disabled for restricted users because the
//! protocol has no table namespace and would otherwise bypass the ACL.

/// Normalise a user-supplied allowlist. `None` and `["*"]` mean unrestricted.
pub fn normalize_tables(input: Option<Vec<String>>) -> Result<Option<Vec<String>>, String> {
    let Some(values) = input else {
        return Ok(None);
    };
    let mut out: Vec<String> = Vec::new();
    for raw in values {
        let value = raw.trim();
        if value.is_empty() {
            continue;
        }
        if value == "*" {
            return Ok(None);
        }
        if value.len() > 128 {
            return Err(format!("table name is too long: {value:.32}..."));
        }
        if !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        {
            return Err(format!(
                "invalid table name '{value}': use letters, digits, '_', '-' or '.'"
            ));
        }
        let lower = value.to_ascii_lowercase();
        if !out.iter().any(|existing| existing == &lower) {
            out.push(lower);
        }
    }
    Ok(Some(out))
}

/// Parse the comma-separated form used by the admin panel. `*` or an empty
/// string means unrestricted; this is intentionally separate from an explicit
/// empty JSON array, which denies all tables.
pub fn normalize_csv(input: &str) -> Result<Option<Vec<String>>, String> {
    let trimmed = input.trim();
    if trimmed == "-" {
        return Ok(Some(Vec::new()));
    }
    if trimmed.is_empty() || trimmed == "*" {
        return Ok(None);
    }
    normalize_tables(Some(
        trimmed
            .split(',')
            .map(|part| part.trim().to_string())
            .collect(),
    ))
}

fn table_key(name: &str) -> String {
    name.trim()
        .trim_matches(|c| matches!(c, '"' | '\'' | '`' | '[' | ']'))
        .to_ascii_lowercase()
}

/// Return the first table that the user is not allowed to touch, if any.
pub fn denied_table(role: &str, allowed: &Option<Vec<String>>, table: &str) -> Option<String> {
    if role.eq_ignore_ascii_case("admin") {
        return None;
    }
    let Some(list) = allowed else {
        return None;
    };
    let key = table_key(table);
    if list.iter().any(|entry| table_key(entry) == key) {
        None
    } else {
        Some(table.to_string())
    }
}

/// Check every table referenced by a query. Returns a client-safe error.
pub fn authorize_tables(
    role: &str,
    allowed: &Option<Vec<String>>,
    tables: &[String],
) -> Result<(), String> {
    for table in tables {
        if let Some(denied) = denied_table(role, allowed, table) {
            return Err(format!(
                "access denied for table '{denied}' (ask an administrator to grant it)"
            ));
        }
    }
    Ok(())
}

/// Raw KV commands have no table namespace, so a restricted account must not
/// be able to reach them. Unrestricted legacy users and admins keep working.
pub fn raw_kv_allowed(role: &str, allowed: &Option<Vec<String>>) -> bool {
    role.eq_ignore_ascii_case("admin") || allowed.is_none()
}

/// Keep only the table names visible to this account.
pub fn filter_tables<'a>(
    role: &str,
    allowed: &Option<Vec<String>>,
    names: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    names
        .into_iter()
        .filter(|name| denied_table(role, allowed, name).is_none())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_means_unrestricted_and_star_resets() {
        assert_eq!(normalize_tables(None).unwrap(), None);
        assert_eq!(
            normalize_tables(Some(vec!["*".into()])).unwrap(),
            None,
            "* means all tables"
        );
    }

    #[test]
    fn normalizes_and_deduplicates() {
        assert_eq!(
            normalize_tables(Some(vec![" Chr_Users ".into(), "chr_users".into()])).unwrap(),
            Some(vec!["chr_users".into()])
        );
    }

    #[test]
    fn empty_list_denies_every_table() {
        let allowed = Some(Vec::new());
        assert!(authorize_tables("viewer", &allowed, &["chr_users".into()]).is_err());
        assert!(authorize_tables("admin", &allowed, &["chr_users".into()]).is_ok());
    }

    #[test]
    fn allows_only_listed_table_case_insensitively() {
        let allowed = Some(vec!["chr_users".into()]);
        assert!(authorize_tables("operator", &allowed, &["CHR_USERS".into()]).is_ok());
        assert!(authorize_tables("operator", &allowed, &["chr_orders".into()]).is_err());
    }

    #[test]
    fn raw_kv_is_blocked_for_restricted_users() {
        assert!(raw_kv_allowed("admin", &Some(vec![])));
        assert!(raw_kv_allowed("viewer", &None));
        assert!(!raw_kv_allowed("viewer", &Some(vec!["chr_users".into()])));
    }
}
