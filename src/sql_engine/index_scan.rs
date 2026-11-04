// SPDX-License-Identifier: AGPL-3.0-only
//! Secondary-index planning: extract indexable predicates (`=`, `IN`,
//! ranges) from WHERE, plan access per indexed column, encode keys.

use super::*;
use crate::expr::{ArithOp, CmpOp, Expr};
use crate::relational::{encode_index_value, ColumnType, Value};

/// Access descriptor for one indexed column, extracted from WHERE.
pub(super) enum IndexAccess {
    /// `col = v` or `col IN (...)`: exact value set.
    Eq(Vec<Value>),
    /// Range with inclusive/exclusive bounds: `col > a AND col <= b`.
    Range {
        lo: Option<(Value, bool)>,
        hi: Option<(Value, bool)>,
    },
}

impl IndexAccess {
    /// CMS edition: coerce lookup literals to the column's declared type
    /// before key encoding. Writes already store with the column type
    /// (Value::coerce_for_column); the same conversion must apply on the
    /// read side, or `WHERE order_id = '15'` encodes a Text key against
    /// Int-written entries (and vice versa) and IndexScan finds nothing.
    #[cfg(feature = "cms")]
    pub(super) fn coerce_values(&mut self, ct: &ColumnType) {
        match self {
            IndexAccess::Eq(values) => {
                for v in values.iter_mut() {
                    *v = Value::coerce_for_column(ct, v.clone());
                }
            }
            IndexAccess::Range { lo, hi } => {
                for bound in [lo, hi].into_iter().flatten() {
                    bound.0 = Value::coerce_for_column(ct, bound.0.clone());
                }
            }
        }
    }
}

impl std::fmt::Display for IndexAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndexAccess::Eq(values) => write!(
                f,
                "IN ({})",
                values
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            IndexAccess::Range { lo, hi } => {
                let mut parts = Vec::new();
                if let Some((v, incl)) = lo {
                    parts.push(format!("{}{}", if *incl { ">=" } else { ">" }, v));
                }
                if let Some((v, incl)) = hi {
                    parts.push(format!("{}{}", if *incl { "<=" } else { "<" }, v));
                }
                if parts.is_empty() {
                    write!(f, "full index scan")
                } else {
                    write!(f, "{}", parts.join(" AND "))
                }
            }
        }
    }
}

/// Key prefix of one column's index entries.
pub(super) fn index_prefix(table: &str, column: &str) -> Vec<u8> {
    format!("__idx:{}:{}:", table, column).into_bytes()
}

/// Key prefix of every index entry of a whole table.
pub(super) fn index_table_prefix(table: &str) -> Vec<u8> {
    format!("__idx:{}:", table).into_bytes()
}

/// Order-preserving, self-delimiting encoding of an index value part:
/// internal 0x00 bytes are escaped as 0x00 0xFF, the part ends with 0x00 0x00.
pub(super) fn escape_index_part(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 2);
    for &b in bytes {
        out.push(b);
        if b == 0 {
            out.push(0xFF);
        }
    }
    out.extend_from_slice(&[0, 0]);
    out
}

/// Smallest byte string strictly greater than every string starting with
/// `prefix` (used as an exclusive range end).
pub(super) fn next_prefix(prefix: &[u8]) -> Vec<u8> {
    let mut out = prefix.to_vec();
    while let Some(last) = out.last_mut() {
        if *last == 0xFF {
            out.pop();
        } else {
            *last += 1;
            return out;
        }
    }
    vec![0xFF] // degenerate: all-0xFF prefix
}

/// Extract an index-backed access from the WHERE conjunction tree.
/// Returns the first indexed column usable as a candidate filter; the full
/// WHERE is re-applied later, so this only needs to be conservative.
pub(super) fn index_access_for(
    schema: &crate::relational::ChromosomeTable,
    expr: &Expr,
) -> Option<(String, IndexAccess)> {
    // Flatten the AND tree into conjuncts.
    let mut conjuncts = Vec::new();
    fn flatten<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
        match e {
            Expr::And(a, b) => {
                flatten(a, out);
                flatten(b, out);
            }
            other => out.push(other),
        }
    }
    flatten(expr, &mut conjuncts);

    // Pass 1: equality / IN-list on an indexed column.
    for c in &conjuncts {
        match c {
            Expr::Cmp {
                op: CmpOp::Eq,
                left,
                right,
            } => {
                if let (Some(col), Some(v)) = (as_column(left), as_literal(right)) {
                    if schema.secondary_index_on(&col).is_some() && !v.is_null() {
                        return Some((col, IndexAccess::Eq(vec![v])));
                    }
                }
                if let (Some(v), Some(col)) = (as_literal(left), as_column(right)) {
                    if schema.secondary_index_on(&col).is_some() && !v.is_null() {
                        return Some((col, IndexAccess::Eq(vec![v])));
                    }
                }
            }
            Expr::InList { column, values, negated } => {
                if !negated
                    && schema.secondary_index_on(column).is_some()
                    && !values.is_empty()
                    && values.iter().all(|v| !v.is_null())
                {
                    return Some((column.clone(), IndexAccess::Eq(values.clone())));
                }
            }
            _ => {}
        }
    }

    // Pass 2: range bounds on an indexed column (tightest wins per side).
    let mut best: Option<(String, Option<(Value, bool)>, Option<(Value, bool)>)> = None;
    for c in &conjuncts {
        let Expr::Cmp { op, left, right } = c else {
            continue;
        };
        let (col, value, col_on_left) = match (as_column(left), as_literal(right)) {
            (Some(col), Some(v)) => (col, v, true),
            _ => match (as_literal(left), as_column(right)) {
                (Some(v), Some(col)) => (col, v, false),
                _ => continue,
            },
        };
        if schema.secondary_index_on(&col).is_none() || value.is_null() {
            continue;
        }
        // Normalise so the column is on the left (flip operator).
        let (op, col_on_left) = if col_on_left {
            (*op, true)
        } else {
            (
                match op {
                    CmpOp::Lt => CmpOp::Gt,
                    CmpOp::Le => CmpOp::Ge,
                    CmpOp::Gt => CmpOp::Lt,
                    CmpOp::Ge => CmpOp::Le,
                    other => *other,
                },
                true,
            )
        };
        let _ = col_on_left;
        let bound = match op {
            CmpOp::Gt => Some((value.clone(), false)),
            CmpOp::Ge => Some((value.clone(), true)),
            CmpOp::Lt => Some((value.clone(), false)),
            CmpOp::Le => Some((value.clone(), true)),
            _ => None,
        };
        let Some(bound) = bound else { continue };
        let entry = best.get_or_insert_with(|| (col.clone(), None, None));
        if entry.0 != col {
            continue; // one indexed column per scan; others stay as filters
        }
        let is_lower = matches!(op, CmpOp::Gt | CmpOp::Ge);
        if is_lower {
            let replace = match &entry.1 {
                None => true,
                Some((cur, cur_incl)) => {
                    bound.0.compare(cur) == std::cmp::Ordering::Greater
                        || (bound.0.compare(cur) == std::cmp::Ordering::Equal
                            && !bound.1
                            && *cur_incl)
                }
            };
            if replace {
                entry.1 = Some(bound);
            }
        } else {
            let replace = match &entry.2 {
                None => true,
                Some((cur, cur_incl)) => {
                    bound.0.compare(cur) == std::cmp::Ordering::Less
                        || (bound.0.compare(cur) == std::cmp::Ordering::Equal
                            && !bound.1
                            && *cur_incl)
                }
            };
            if replace {
                entry.2 = Some(bound);
            }
        }
    }
    best.map(|(col, lo, hi)| (col, IndexAccess::Range { lo, hi }))
}

pub(super) fn as_column(e: &Expr) -> Option<String> {
    match e {
        Expr::Column(c) => Some(c.clone()),
        _ => None,
    }
}

pub(super) fn as_literal(e: &Expr) -> Option<Value> {
    match e {
        Expr::Literal(v) => Some(v.clone()),
        _ => None,
    }
}

/// Aggregate items referenced by the projection or HAVING, as
/// (canonical name, output name, function, argument column) — deduplicated
/// by canonical name.
pub(super) fn collect_agg_items(
    projection: &[SelectItem],
    having: Option<&Expr>,
) -> Vec<(String, String, AggFunc, Option<String>)> {
    let mut items: Vec<(String, String, AggFunc, Option<String>)> = Vec::new();
    let mut push = |name: String, out: String, func: AggFunc, arg: Option<String>| {
        if !items.iter().any(|(n, _, _, _)| *n == name) {
            items.push((name, out, func, arg));
        }
    };
    for item in projection {
        if let SelectItem::Agg {
            func,
            arg,
            rendered,
            canonical,
        } = item
        {
            push(canonical.clone(), rendered.clone(), *func, arg.clone());
        }
    }
    // Aggregates appearing only inside HAVING still need computing (they
    // stay internal: output name = canonical, stripped after filtering if
    // not projected).
    if let Some(h) = having {
        let mut cols = Vec::new();
        h.collect_columns(&mut cols);
        for c in cols {
            if let Some((func, arg)) = parse_rendered_agg(&c) {
                push(c.clone(), c, func, arg);
            }
        }
    }
    items
}

/// Inverse of `render_agg`: "COUNT(*)" -> (Count, None).
pub(super) fn parse_rendered_agg(name: &str) -> Option<(AggFunc, Option<String>)> {
    let (func, rest) = name.split_once('(')?;
    let arg = rest.strip_suffix(')')?;
    let func = AggFunc::from_name(func).ok()?;
    let arg = if arg == "*" || arg.is_empty() {
        None
    } else {
        // COUNT(DISTINCT col): HAVING/ORDER BY references render the
        // canonical form; the aggregation itself runs on the bare column.
        Some(arg.strip_prefix("DISTINCT ").unwrap_or(arg).to_string())
    };
    Some((func, arg))
}

pub(super) fn mk_row(fields: &[(&str, Value)]) -> BTreeMap<String, Value> {
    fields
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

/// Literal typing rule shared by INSERT values and WHERE operands:
/// non-empty pure-ACGT strings become `Value::Dna` (uppercased), everything
/// else stays `Value::Text`.
pub(super) fn row_maps_equal(
    a: &BTreeMap<String, Value>,
    b: &BTreeMap<String, Value>,
) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().all(|(k, v)| {
        b.get(k)
            .map(|w| v.compare(w) == std::cmp::Ordering::Equal)
            .unwrap_or(false)
    })
}

pub(super) fn typed_string_literal(s: &str) -> Value {
    let pure_acgt = !s.is_empty()
        && s.chars()
            .all(|c| matches!(c, 'A' | 'C' | 'G' | 'T' | 'a' | 'c' | 'g' | 't'));
    if !pure_acgt {
        return Value::Text(s.to_string());
    }
    // CMS edition (branch cms-edition): only literals that are ALREADY
    // uppercase ACGT keep DNA typing. WordPress content is full of
    // lowercase pure-ACGT words ('cat', 'tag', 'gattaca') that the
    // uppercase-normalising sniff would corrupt into pseudo-DNA.
    #[cfg(feature = "cms")]
    if s.chars().all(|c| c.is_ascii_uppercase()) {
        return Value::Dna(s.to_string());
    }
    #[cfg(feature = "cms")]
    return Value::Text(s.to_string());
    #[cfg(not(feature = "cms"))]
    return Value::Dna(s.to_uppercase());
}

/// Canonical output name of an aggregate call: `COUNT(*)`, `SUM(balance)`.
pub(super) fn render_agg(func: &str, arg: Option<&str>) -> String {
    format!("{}({})", func.to_ascii_uppercase(), arg.unwrap_or("*"))
}

/// Like render_agg, but emits the MySQL DISTINCT form `COUNT(DISTINCT col)`.
pub(super) fn render_agg_distinct(func: &str, arg: Option<&str>, distinct: bool) -> String {
    if distinct {
        format!(
            "{}(DISTINCT {})",
            func.to_ascii_uppercase(),
            arg.unwrap_or("*")
        )
    } else {
        render_agg(func, arg)
    }
}

/// SQL rendering of a parsed expression (string literals re-quoted) — used
/// to serialise IN-subqueries for re-execution.
pub(super) fn expr_to_sql(e: &Expr) -> String {
    fn quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', "''"))
    }
    // Arithmetic precedence: Add/Sub bind looser than Mul/Div. A child
    // arithmetic node whose precedence is <= the parent's must be
    // parenthesised, otherwise `a * (b + c)` rendered as `a * b + c`
    // re-parses (and re-executes) to a DIFFERENT result — the silently
    // wrong subquery results this renderer used to produce.
    fn arith_prec(op: &ArithOp) -> u8 {
        match op {
            ArithOp::Add | ArithOp::Sub => 1,
            ArithOp::Mul | ArithOp::Div => 2,
        }
    }
    fn render_arith(op: &ArithOp, left: &Expr, right: &Expr) -> String {
        let prec = arith_prec(op);
        let side = |e: &Expr| match e {
            Expr::Arith { op: child, .. } if arith_prec(child) <= prec => {
                format!("({})", expr_to_sql(e))
            }
            other => expr_to_sql(other),
        };
        format!("{} {} {}", side(left), op.as_str(), side(right))
    }
    match e {
        Expr::Like { operand, pattern, negated } => format!(
            "{} {}LIKE '{}'",
            expr_to_sql(operand),
            if *negated { "NOT " } else { "" },
            pattern
        ),
        Expr::Fn { name, args } => format!(
            "{}({})",
            name,
            args.iter()
                .map(|a| expr_to_sql(a))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Expr::Literal(Value::Text(s)) | Expr::Literal(Value::Dna(s)) => quote(s),
        Expr::Literal(v) => v.to_string(),
        Expr::Column(c) => c.clone(),
        Expr::Not(inner) => format!("NOT {}", expr_to_sql(inner)),
        Expr::And(a, b) => format!("({} AND {})", expr_to_sql(a), expr_to_sql(b)),
        Expr::Or(a, b) => format!("({} OR {})", expr_to_sql(a), expr_to_sql(b)),
        Expr::IsNull(inner) => format!("{} IS NULL", expr_to_sql(inner)),
        Expr::IsNotNull(inner) => format!("{} IS NOT NULL", expr_to_sql(inner)),
        Expr::Arith { op, left, right } => render_arith(op, left, right),
        Expr::Cmp { op, left, right } => {
            format!(
                "{} {} {}",
                expr_to_sql(left),
                op.as_str(),
                expr_to_sql(right)
            )
        }
        Expr::Cast { inner, ty } => {
            format!("CAST({} AS {})", expr_to_sql(inner), ty.as_str())
        }
        Expr::InList { column, values, negated } => format!(
            "{}{} IN ({})",
            if *negated { "NOT " } else { "" },
            column,
            values
                .iter()
                .map(|v| match v {
                    Value::Text(s) | Value::Dna(s) => quote(s),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Expr::InSubquery { column, query } => format!("{} IN ({})", column, query),
        Expr::ScalarSubquery { stmt } => format!("({})", render_select_sql(stmt)),
        Expr::Exists { stmt, negated } => format!(
            "{}EXISTS ({})",
            if *negated { "NOT " } else { "" },
            render_select_sql(stmt)
        ),
    }
}

/// Render a parsed SELECT back to SQL text (for IN-subquery materialisation).
pub(super) fn render_select_sql(stmt: &Stmt) -> String {
    let Stmt::Select {
        projection,
        from,
        from_alias: _,
        joins,
        where_clause,
        group_by,
        having,
        order_by,
        limit,
        offset,
    } = stmt
    else {
        return "<non-select>".into();
    };
    let proj = if projection.is_empty() || projection.iter().any(|p| matches!(p, SelectItem::Star))
    {
        "*".to_string()
    } else {
        projection
            .iter()
            .filter_map(|item| match item {
                SelectItem::Star => Some("*".to_string()),
                SelectItem::AliasStar(qual) => Some(format!("{}.*", qual)),
                SelectItem::Column { source, alias } => match alias {
                    Some(a) if a != source => Some(format!("{} AS {}", source, a)),
                    _ => Some(source.clone()),
                },
                SelectItem::Computed { expr, rendered } => {
                    let base = expr_to_sql(expr);
                    if *rendered == expr.describe() {
                        Some(base)
                    } else {
                        Some(format!("{} AS {}", base, rendered))
                    }
                }
                SelectItem::Agg { canonical, .. } => Some(canonical.clone()),
                SelectItem::ScalarSub { canonical, .. } => Some(canonical.clone()),
                SelectItem::Window { canonical, .. } => Some(canonical.clone()),
                // CMS edition: sort keys are not part of the SQL text.
                SelectItem::OrderKeyOnly { .. } => None,
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut out = format!("SELECT {} FROM {}", proj, from);
    for j in joins {
        out.push_str(&format!(
            " {} JOIN {} ON {} = {}",
            match j.join_type {
                JoinType::Inner => "INNER",
                JoinType::Left => "LEFT",
                JoinType::Right => "RIGHT",
                JoinType::Full => "FULL",
            },
            j.table,
            j.left_col,
            j.right_col
        ));
    }
    if let Some(w) = where_clause {
        out.push_str(&format!(" WHERE {}", expr_to_sql(w)));
    }
    if !group_by.is_empty() {
        out.push_str(&format!(" GROUP BY {}", group_by.join(", ")));
    }
    if let Some(h) = having {
        out.push_str(&format!(" HAVING {}", expr_to_sql(h)));
    }
    for (key, asc) in order_by {
        out.push_str(&format!(
            " ORDER BY {} {}",
            key,
            if *asc { "ASC" } else { "DESC" }
        ));
    }
    if let Some(n) = limit {
        out.push_str(&format!(" LIMIT {}", n));
    }
    if *offset > 0 {
        out.push_str(&format!(" OFFSET {}", offset));
    }
    out
}

/// Join-key encoding with SQL comparison semantics:
///  * NULL never matches anything, not even another NULL (three-valued
///    logic) — previously NULL keys were hashed together and joined;
///  * Int and Float compare numerically, so Int(5) and Float(5.0) must
///    land in the same bucket (the raw index encoding is per-type, which
///    silently dropped cross-type matches).
pub(super) fn join_key(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Null => None,
        Value::Float(f)
            if f.is_finite() && f.fract() == 0.0 && *f >= i64::MIN as f64 && *f < i64::MAX as f64 =>
        {
            Some(crate::relational::encode_index_value(&Value::Int(*f as i64)))
        }
        other => Some(crate::relational::encode_index_value(other)),
    }
}

/// Inner hash join on equi-condition left_col = right_col.
/// Column collisions: right-side columns are prefixed '<table>.'.
pub(super) fn hash_join(
    left: &[BTreeMap<String, Value>],
    right: &[BTreeMap<String, Value>],
    left_col: &str,
    right_col: &str,
) -> Vec<BTreeMap<String, Value>> {
    use std::collections::HashMap;
    let mut index: HashMap<Vec<u8>, Vec<&BTreeMap<String, Value>>> = HashMap::new();
    for r in right {
        if let Some(v) = r.get(right_col) {
            if let Some(key) = join_key(v) {
                index.entry(key).or_default().push(r);
            }
        }
    }
    let mut out = Vec::new();
    for l in left {
        if let Some(v) = l.get(left_col) {
            if let Some(key) = join_key(v) {
                if let Some(matches) = index.get(&key) {
                    for r in matches {
                        let mut merged = l.clone();
                        for (k, val) in r.iter() {
                            let name = if merged.contains_key(k) {
                                format!("joined_{}", k)
                            } else {
                                k.clone()
                            };
                            merged.insert(name, val.clone());
                        }
                        out.push(merged);
                    }
                }
            }
        }
    }
    out
}

/// CMS edition (branch `cms-edition`): pure cross join — the executor form
/// of `INNER JOIN … ON 1=1`, which the compat gate produces when rewriting
/// MySQL comma-FROM lists (`FROM a, b WHERE …`). Same merge rules as the
/// equi-joins: right-side colliding columns get the `joined_` prefix.
pub(super) fn cross_join(
    left: &[BTreeMap<String, Value>],
    right: &[BTreeMap<String, Value>],
) -> Vec<BTreeMap<String, Value>> {
    let mut out = Vec::with_capacity(left.len() * right.len());
    for l in left {
        for r in right {
            let mut merged = l.clone();
            for (k, val) in r.iter() {
                let name = if merged.contains_key(k) {
                    format!("joined_{}", k)
                } else {
                    k.clone()
                };
                merged.insert(name, val.clone());
            }
            out.push(merged);
        }
    }
    out
}

/// LEFT hash join: unmatched left rows are kept with NULLs for right columns.
pub(super) fn hash_join_left(
    left: &[BTreeMap<String, Value>],
    right: &[BTreeMap<String, Value>],
    left_col: &str,
    right_col: &str,
) -> Vec<BTreeMap<String, Value>> {
    let mut out = hash_join(left, right, left_col, right_col);
    // find which left rows got no match: recompute matched set
    use std::collections::HashSet;
    let matched_keys: HashSet<Vec<u8>> = out
        .iter()
        .filter_map(|r| r.get(left_col))
        .filter_map(join_key)
        .collect();
    let right_cols: Vec<String> = right
        .first()
        .map(|r| r.keys().cloned().collect())
        .unwrap_or_default();
    for l in left {
        let key = l.get(left_col).and_then(join_key);
        let matched = key
            .as_ref()
            .map(|k| matched_keys.contains(k))
            .unwrap_or(false);
        if !matched {
            let mut row = l.clone();
            for c in &right_cols {
                if !row.contains_key(c) {
                    row.insert(c.clone(), Value::Null);
                }
            }
            out.push(row);
        }
    }
    out
}

/// v3.6 FULL OUTER JOIN: every matched pair, plus unmatched LEFT rows (right
/// columns NULL) and unmatched RIGHT rows (left columns NULL). NULL keys
/// never match (three-valued logic, same as hash_join/hash_join_left).
pub(super) fn hash_join_full(
    left_rows: &[BTreeMap<String, Value>],
    right_rows: &[BTreeMap<String, Value>],
    left_col: &str,
    right_col: &str,
) -> Vec<BTreeMap<String, Value>> {
    use std::collections::HashMap;
    // key -> indices into right_rows
    let mut index: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
    for (i, r) in right_rows.iter().enumerate() {
        if let Some(k) = join_key(r.get(right_col).unwrap_or(&Value::Null)) {
            index.entry(k).or_default().push(i);
        }
    }
    let right_cols: Vec<String> = right_rows
        .first()
        .map(|r| r.keys().cloned().collect())
        .unwrap_or_default();
    let mut matched_right: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for l in left_rows {
        let key = join_key(l.get(left_col).unwrap_or(&Value::Null));
        let matches: &[usize] = key
            .as_ref()
            .and_then(|k| index.get(k))
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        if matches.is_empty() {
            let mut row = l.clone();
            for c in &right_cols {
                if !row.contains_key(c) {
                    row.insert(c.clone(), Value::Null);
                }
            }
            out.push(row);
            continue;
        }
        for &ri in matches {
            matched_right.insert(ri);
            let r = &right_rows[ri];
            let mut row = l.clone();
            for (k, v) in r.iter() {
                row.insert(k.clone(), v.clone());
            }
            out.push(row);
        }
    }
    // Right rows that never matched: keep the right columns, NULL-pad the
    // left side (any column the right row does not carry becomes NULL —
    // the single-namespace model unifies both sides).
    for (i, r) in right_rows.iter().enumerate() {
        if matched_right.contains(&i) {
            continue;
        }
        let mut row = r.clone();
        for (k, v) in row.iter_mut() {
            if !r.contains_key(k) {
                *v = Value::Null;
            }
        }
        out.push(row);
    }
    out
}

/// Does this expression contain SQL subquery nodes (InSubquery that may be
/// correlated, ScalarSubquery, Exists)? Used to switch the WHERE filter to
/// the per-row resolution path.
pub(super) fn expr_has_subquery(e: &Expr) -> bool {
    match e {
        Expr::ScalarSubquery { .. } | Expr::Exists { .. } | Expr::InSubquery { .. } => true,
        Expr::Not(inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            expr_has_subquery(inner)
        }
        Expr::And(a, b) | Expr::Or(a, b) => expr_has_subquery(a) || expr_has_subquery(b),
        Expr::Cmp { left, right, .. } | Expr::Arith { left, right, .. } => {
            expr_has_subquery(left) || expr_has_subquery(right)
        }
        Expr::Like { operand, .. } => expr_has_subquery(operand),
        Expr::Cast { inner, .. } => expr_has_subquery(inner),
        Expr::Fn { args, .. } => args.iter().any(expr_has_subquery),
        Expr::Literal(_) | Expr::Column(_) | Expr::InList { .. } => false,
    }
}

/// Canonical rendering of a window function expression.
pub(super) fn render_window(func: &WindowFunc, partition: &[String], order: &[(String, bool)]) -> String {
    let mut s = format!("{}() OVER (", func.name());
    if !partition.is_empty() {
        s.push_str(&format!("PARTITION BY {} ", partition.join(", ")));
    }
    if !order.is_empty() {
        s.push_str(&format!(
            "ORDER BY {}",
            order
                .iter()
                .map(|(c, asc)| format!("{} {}", c, if *asc { "ASC" } else { "DESC" }))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    s.push(')');
    s
}

/// Render any SELECT-shaped statement back to SQL text (subquery execution).
pub(crate) fn render_stmt_sql(stmt: &Stmt) -> String {
    render_select_sql(stmt)
}

/// Compute a ranking window function over `rows` in place, inserting the
/// result under `canonical`. Rows are grouped by the partition key (encoded
/// values), sorted by the window ORDER BY inside each partition, then
/// numbered: ROW_NUMBER = position, RANK = position of first peer,
/// DENSE_RANK = consecutive peer groups.
pub(super) fn compute_window(
    rows: &mut [BTreeMap<String, Value>],
    func: WindowFunc,
    partition: &[String],
    order: &[(String, bool)],
    canonical: &str,
) {
    // Index rows by partition key.
    let mut partitions: std::collections::BTreeMap<Vec<u8>, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (i, row) in rows.iter().enumerate() {
        let mut key = Vec::new();
        for col in partition {
            let v = row.get(col).cloned().unwrap_or(Value::Null);
            key.extend_from_slice(&encode_index_value(&v));
            key.push(0);
        }
        partitions.entry(key).or_default().push(i);
    }
    for (_, mut idxs) in partitions {
        if !order.is_empty() {
            idxs.sort_by(|&a, &b| {
                for (col, asc) in order {
                    let va = rows[a].get(col).cloned().unwrap_or(Value::Null);
                    let vb = rows[b].get(col).cloned().unwrap_or(Value::Null);
                    let ord = va.compare(&vb);
                    if ord != std::cmp::Ordering::Equal {
                        return if *asc { ord } else { ord.reverse() };
                    }
                }
                std::cmp::Ordering::Equal
            });
        }
        let mut rank = 1usize;
        let mut dense = 1usize;
        for (pos, &i) in idxs.iter().enumerate() {
            let value = match func {
                WindowFunc::RowNumber => (pos + 1) as i64,
                WindowFunc::Rank => rank as i64,
                WindowFunc::DenseRank => dense as i64,
            };
            rows[i].insert(canonical.to_string(), Value::Int(value));
            let is_last = pos + 1 == idxs.len();
            if !is_last {
                // Peers share the same ORDER BY values; a new peer group
                // advances RANK to the current position and DENSE_RANK by 1.
                let a = &rows[i];
                let b = &rows[idxs[pos + 1]];
                let peer = order.iter().all(|(col, _)| {
                    let va = a.get(col).cloned().unwrap_or(Value::Null);
                    let vb = b.get(col).cloned().unwrap_or(Value::Null);
                    va.compare(&vb) == std::cmp::Ordering::Equal
                });
                if !peer {
                    rank = pos + 2;
                    dense += 1;
                }
            }
        }
    }
}

