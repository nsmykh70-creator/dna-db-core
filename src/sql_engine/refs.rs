// SPDX-License-Identifier: AGPL-3.0-only
//! ACL support: which tables (and columns) does a statement touch?
//! Parsed with the same `SParser`, but never touches storage.

use super::lexer::STok;
use super::parser::SParser;
use super::stmt::{OnConflict, SelectItem, Stmt};
use crate::expr::Expr;


/// Parse a SQL statement and return every table it references, including
/// tables in JOINs, foreign-key definitions and `IN (SELECT ...)` subqueries.
/// This is used by the per-user ACL before execution; it never touches storage.
pub(crate) fn referenced_tables(sql: &str) -> Result<Vec<String>, String> {
    let mut parser = SParser::new(sql)?;
    let stmt = parser.parse_stmt()?;
    if parser.peek() != &STok::Eof {
        return Err(format!(
            "SQL: unexpected trailing tokens starting at {:?} (unsupported syntax?)",
            parser.peek()
        ));
    }
    let mut tables = Vec::new();
    collect_stmt_tables(&stmt, &mut tables)?;
    tables.sort();
    tables.dedup();
    Ok(tables)
}

pub(super) fn collect_stmt_tables(stmt: &Stmt, out: &mut Vec<String>) -> Result<(), String> {
    match stmt {
        Stmt::DropColumn { table, .. } => {
            out.push(table.clone());
        }
        Stmt::Select {
            projection,
            from,
            joins,
            where_clause,
            having,
            ..
        } => {
            out.push(from.clone());
            for join in joins {
                out.push(join.table.clone());
            }
            for item in projection {
                match item {
                    SelectItem::Computed { expr, .. } => collect_expr_tables(expr, out)?,
                    // Scalar subqueries contribute their own tables.
                    SelectItem::ScalarSub { stmt, .. } => {
                        collect_stmt_tables(stmt, out)?;
                    }
                    SelectItem::Window { .. } => {}
                    _ => {}
                }
            }
            if let Some(expr) = where_clause {
                collect_expr_tables(expr, out)?;
            }
            if let Some(expr) = having {
                collect_expr_tables(expr, out)?;
            }
        }
        Stmt::Union { left, right, .. } => {
            collect_stmt_tables(left, out)?;
            collect_stmt_tables(right, out)?;
        }
        Stmt::Cte { ctes, body } => {
            for (_, def) in ctes {
                collect_stmt_tables(def, out)?;
            }
            collect_stmt_tables(body, out)?;
        }
        Stmt::ShowTables | Stmt::Tx { .. } => {}
        Stmt::ShowStorage { table, .. } => out.push(table.clone()),
        Stmt::AddColumn { table, .. } => out.push(table.clone()),
        Stmt::Insert { table, rows, on_conflict, ignore: _ } => {
            out.push(table.clone());
            // Column values are literals; no table refs beyond the target.
            if let Some(OnConflict::DoUpdate { assignments }) = on_conflict {
                for (col, val) in assignments {
                    let _ = (col, val);
                }
            }
        }
        Stmt::InsertSelect { table, select, .. } => {
            out.push(table.clone());
            collect_stmt_tables(select, out)?;
        }
        Stmt::DropTable { table, .. } => out.push(table.clone()),
        Stmt::Update {
            table,
            where_clause,
            ..
        }
        | Stmt::Delete {
            table,
            where_clause,
            ..
        } => {
            out.push(table.clone());
            if let Some(expr) = where_clause {
                collect_expr_tables(expr, out)?;
            }
        }
        Stmt::CreateTable { table, .. } => {
            out.push(table.name.clone());
            for fk in &table.foreign_keys {
                out.push(fk.target_chromosome.clone());
            }
        }
        Stmt::CreateIndex { table, .. } => out.push(table.clone()),
        Stmt::DropIndex { table, .. } => {
            // The bare form (`DROP INDEX name`) does not name its table, and
            // this function must not touch storage to resolve the name — so a
            // table-restricted user cannot be permission-checked here.
            // Reject it for the ACL path instead of silently authorising an
            // index drop on ANY table (the ACL bypass this used to be).
            // Administrators never reach this code (they skip the ACL), and
            // everybody else can use the documented ON form.
            if table.is_none() {
                return Err(
                    "SQL: DROP INDEX without ON <table> cannot be permission-checked; \
                     use DROP INDEX <name> ON <table>"
                        .to_string(),
                );
            }
            out.push(table.clone().unwrap());
        }
        Stmt::Explain(inner) => collect_stmt_tables(inner, out)?,
    }
    Ok(())
}

/// Every column a statement references (WHERE, HAVING, projections,
/// computed items). Used to distinguish inner columns from correlated
/// outer references of a subquery.
pub(super) fn collect_stmt_columns(stmt: &Stmt, out: &mut Vec<String>) -> Result<(), String> {
    match stmt {
        Stmt::Select {
            projection,
            where_clause,
            having,
            ..
        } => {
            for item in projection {
                match item {
                    SelectItem::Column { source, .. } => {
                        if !out.contains(source) {
                            out.push(source.clone());
                        }
                    }
                    SelectItem::Computed { expr, .. } => {
                        expr.collect_columns(out);
                    }
                    SelectItem::Agg { arg, .. } => {
                        if let Some(c) = arg {
                            if !out.contains(c) {
                                out.push(c.clone());
                            }
                        }
                    }
                    SelectItem::ScalarSub { stmt, .. } => collect_stmt_columns(stmt, out)?,
                    SelectItem::Window { partition, order, .. } => {
                        for c in partition {
                            if !out.contains(c) {
                                out.push(c.clone());
                            }
                        }
                        for (c, _) in order {
                            if !out.contains(c) {
                                out.push(c.clone());
                            }
                        }
                    }
                    // CMS edition: ORDER BY sort keys are internal — they
                    // create no outer references of their own.
                    SelectItem::OrderKeyOnly { .. }
                    | SelectItem::AliasStar(_)
                    | SelectItem::Star => {}
                }
            }
            if let Some(e) = where_clause {
                e.collect_columns(out);
            }
            if let Some(e) = having {
                e.collect_columns(out);
            }
        }
        Stmt::Union { left, right, order_by, .. } => {
            collect_stmt_columns(left, out)?;
            collect_stmt_columns(right, out)?;
            for (c, _) in order_by {
                if !out.contains(c) {
                    out.push(c.clone());
                }
            }
        }
        Stmt::Cte { ctes, body } => {
            for (_, def) in ctes {
                collect_stmt_columns(def, out)?;
            }
            collect_stmt_columns(body, out)?;
        }
        Stmt::Update { assignments, where_clause, .. } => {
            for (c, _) in assignments {
                if !out.contains(c) {
                    out.push(c.clone());
                }
            }
            if let Some(e) = where_clause {
                e.collect_columns(out);
            }
        }
        Stmt::Delete { where_clause, .. } => {
            if let Some(e) = where_clause {
                e.collect_columns(out);
            }
        }
        Stmt::Insert { rows, .. } => {
            for row in rows {
                for (c, _) in row {
                    if !out.contains(c) {
                        out.push(c.clone());
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn collect_expr_tables(expr: &Expr, out: &mut Vec<String>) -> Result<(), String> {
    match expr {
        Expr::Like { operand, .. } => collect_expr_tables(operand, out)?,
        Expr::InSubquery { query, .. } => {
            let nested = referenced_tables(query)?;
            out.extend(nested);
        }
        Expr::ScalarSubquery { stmt } | Expr::Exists { stmt, .. } => {
            collect_stmt_tables(stmt, out)?;
        }
        Expr::Not(inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            collect_expr_tables(inner, out)?;
        }
        Expr::And(left, right)
        | Expr::Or(left, right)
        | Expr::Cmp { left, right, .. }
        | Expr::Arith { left, right, .. } => {
            collect_expr_tables(left, out)?;
            collect_expr_tables(right, out)?;
        }
        Expr::Cast { inner, .. } => collect_expr_tables(inner, out)?,
        Expr::Literal(_) | Expr::Column(_) | Expr::InList { .. } => {}
        Expr::Fn { args, .. } => {
            for a in args {
                collect_expr_tables(a, out)?;
            }
        }
    }
    Ok(())
}
