// SPDX-License-Identifier: AGPL-3.0-only
//! DnaQL executor: `DnaQlEngine` executes parsed ASTs against the real
//! storage engine — pattern search, pipelines, transactions, bio analysis,
//! VERIFY/REPAIR CHROMOSOME and schema DDL forwarded to the SQL engine.

use super::*;

// ---------------------------------------------------------------------------
// Engine (parser + executor)
// ---------------------------------------------------------------------------

pub struct DnaQlEngine {
    pub engine: Arc<Engine>,
    pub tx: TransactionCoordinator,
    /// Shared SQL engine — schema DDL (CREATE/DROP CHROMOSOME) goes through
    /// it so the SQL side's in-memory schema cache stays consistent.
    pub sql: Arc<SqlEngine>,
}

impl DnaQlEngine {
    pub fn new(engine: Arc<Engine>, sql: Arc<SqlEngine>) -> Self {
        let tx = TransactionCoordinator::new(engine.clone());
        Self { engine, tx, sql }
    }

    pub fn parse(&self, query: &str) -> Result<DnaQlAst, String> {
        let toks = tokenize(query)?;
        let mut p = Parser::new(toks);
        p.parse_query()
    }

    pub fn execute(&self, query: &str) -> Result<DnaQlResult, String> {
        let start = std::time::Instant::now();
        let ast = self.parse(query)?;
        let result = match &ast {
            DnaQlAst::PatternSearch {
                chromosome,
                pattern,
                mode,
                max_distance,
                link,
                projection,
            } => self.exec_pattern_search(
                chromosome,
                pattern,
                mode.clone(),
                *max_distance,
                link.clone(),
                projection.clone(),
            )?,
            DnaQlAst::PipelineQuery { source, stages } => self.exec_pipeline(source, stages)?,
            DnaQlAst::Tx { statements } => self.exec_tx(statements)?,
            DnaQlAst::BioAnalysis {
                sequence,
                locus,
                functions,
            } => self.exec_bio(sequence.clone(), locus.clone(), functions)?,
            DnaQlAst::HealthCheck { chromosome, fix } => {
                self.exec_health_check(chromosome, *fix)?
            }
            DnaQlAst::CreateChromosome {
                table,
                definitions,
                if_not_exists,
            } => self.exec_create_chromosome(table, definitions, *if_not_exists)?,
            DnaQlAst::DropChromosome { table, if_exists } => {
                self.exec_drop_chromosome(table, *if_exists)?
            }
        };

        Ok(DnaQlResult {
            ast_summary: format!("{:?}", ast_summary(&ast)),
            rows: result.0,
            engine_note: result.1,
            execution_time_us: start.elapsed().as_micros() as u64,
        })
    }

    fn exec_pattern_search(
        &self,
        chromosome: &str,
        pattern: &str,
        mode: DistanceMode,
        max_distance: usize,
        link: Option<LinkSpec>,
        projection: Vec<String>,
    ) -> Result<(Vec<DnaQlRow>, String), String> {
        if pattern.is_empty() || pattern.len() > 64 {
            return Err(format!(
                "DnaQL: pattern length must be 1..=64, got {}",
                pattern.len()
            ));
        }
        PackedDna::from_ascii(pattern).map_err(|e| format!("DnaQL: invalid pattern: {}", e))?;

        let prefix = format!("{}:", chromosome);
        let raw_rows = self
            .engine
            .scan_prefix(prefix.as_bytes())
            .map_err(|e| format!("DnaQL storage error: {}", e))?;

        let mut out = Vec::new();
        let mut scanned = 0usize;
        for (key, payload) in raw_rows {
            scanned += 1;
            let fields = RowCodec::decode(&payload).map_err(|e| {
                format!(
                    "DnaQL: row {} unreadable: {}",
                    String::from_utf8_lossy(&key),
                    e
                )
            })?;
            let row_map: BTreeMap<String, Value> = fields.into_iter().collect();

            let seq_str = match row_map.get("sequence") {
                Some(Value::Dna(s)) | Some(Value::Text(s)) => s.clone(),
                _ => continue, // rows without sequence can't be pattern-searched
            };
            let packed = PackedDna::from_ascii(&seq_str)
                .map_err(|e| format!("DnaQL: stored sequence invalid: {}", e))?;

            let matches = match mode {
                DistanceMode::Levenshtein => BitapMatcher::search(&packed, pattern, max_distance),
                DistanceMode::Hamming => {
                    BitapMatcher::search_hamming(&packed, pattern, max_distance)
                }
            };

            for m in matches {
                let mut result_row = BTreeMap::new();
                result_row.insert(
                    "locus".to_string(),
                    Value::Text(String::from_utf8_lossy(&key).to_string()),
                );
                result_row.insert("offset_bp".to_string(), Value::Int(m.offset_bp as i64));
                result_row.insert("mismatches".to_string(), Value::Int(m.mismatches as i64));
                result_row.insert(
                    "matched_seq".to_string(),
                    Value::Dna(m.matched_sequence.clone()),
                );
                // merge source row fields (locus_id, variant_name, ...)
                for (k, v) in &row_map {
                    result_row.entry(k.clone()).or_insert_with(|| v.clone());
                }

                // Complementary linkage: O(1) direct key seek
                if let Some(spec) = &link {
                    let fk_val = row_map.get(&spec.column).cloned().unwrap_or(Value::Null);
                    let target_key = format!("{}:{}", spec.chromosome, fk_val);
                    if let Some(target_bytes) = self
                        .engine
                        .get(target_key.as_bytes())
                        .map_err(|e| format!("DnaQL storage error: {}", e))?
                    {
                        let target_fields = RowCodec::decode(&target_bytes)
                            .map_err(|e| format!("DnaQL: link target unreadable: {}", e))?;
                        for (k, v) in target_fields {
                            // prefer the plain field name, then namespace, then link_
                            let name = if !result_row.contains_key(&k) {
                                k.clone()
                            } else {
                                let ns = format!("{}.{}", spec.chromosome, k);
                                if result_row.contains_key(&ns) {
                                    format!("link_{}", k)
                                } else {
                                    ns
                                }
                            };
                            result_row.insert(name, v);
                        }
                    } else {
                        result_row.insert("link_missing".to_string(), Value::Text(target_key));
                    }
                }

                let row = if projection.is_empty() {
                    DnaQlRow { fields: result_row }
                } else {
                    let mut filtered = BTreeMap::new();
                    for p in &projection {
                        if let Some(v) = result_row.get(p) {
                            filtered.insert(p.clone(), v.clone());
                        }
                    }
                    DnaQlRow { fields: filtered }
                };
                out.push(row);
            }
        }

        let note = format!(
            "real scan: {} loci, bitap {} k={}, {} matches",
            scanned,
            match mode {
                DistanceMode::Hamming => "Hamming",
                DistanceMode::Levenshtein => "Levenshtein",
            },
            max_distance,
            out.len()
        );
        Ok((out, note))
    }

    fn exec_create_chromosome(
        &self,
        table: &str,
        definitions: &[String],
        if_not_exists: bool,
    ) -> Result<(Vec<DnaQlRow>, String), String> {
        if definitions.is_empty() {
            return Err("DnaQL: CREATE CHROMOSOME needs at least one column".into());
        }
        let mut clauses: Vec<String> = definitions.to_vec();
        // No explicit PRIMARY KEY clause: the first column becomes the locus,
        // mirroring SQL CREATE TABLE's fallback (first column as PK).
        let has_pk = definitions.iter().any(|d| {
            d.split_whitespace()
                .next()
                .map(|w| w.eq_ignore_ascii_case("PRIMARY"))
                .unwrap_or(false)
        });
        if !has_pk {
            let first = definitions[0]
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
            if first.is_empty() {
                return Err("DnaQL: CREATE CHROMOSOME first clause names no column".into());
            }
            clauses.push(format!("PRIMARY KEY ({})", first));
        }
        let nie = if if_not_exists { "IF NOT EXISTS " } else { "" };
        let sql = format!("CREATE TABLE {}{} ({})", nie, table, clauses.join(", "));
        let res = self
            .sql
            .execute(&sql)
            .map_err(|e| format!("DnaQL DDL: {}", e))?;
        Ok((
            vec![],
            res.message
                .unwrap_or_else(|| format!("chromosome '{}' created", table)),
        ))
    }

    fn exec_drop_chromosome(
        &self,
        table: &str,
        if_exists: bool,
    ) -> Result<(Vec<DnaQlRow>, String), String> {
        let sql = if if_exists {
            format!("DROP TABLE IF EXISTS {}", table)
        } else {
            format!("DROP TABLE {}", table)
        };
        let res = self
            .sql
            .execute(&sql)
            .map_err(|e| format!("DnaQL DDL: {}", e))?;
        Ok((
            vec![],
            res.message
                .unwrap_or_else(|| format!("chromosome '{}' dropped", table)),
        ))
    }

    fn exec_pipeline(
        &self,
        source: &str,
        stages: &[PipelineStage],
    ) -> Result<(Vec<DnaQlRow>, String), String> {
        let prefix = format!("{}:", source);
        let raw_rows = self
            .engine
            .scan_prefix(prefix.as_bytes())
            .map_err(|e| format!("DnaQL storage error: {}", e))?;

        let mut rows: Vec<DnaQlRow> = Vec::new();
        for (key, payload) in raw_rows {
            let fields =
                RowCodec::decode(&payload).map_err(|e| format!("DnaQL: row unreadable: {}", e))?;
            let mut row_map: BTreeMap<String, Value> = fields.into_iter().collect();
            row_map.insert(
                "locus".to_string(),
                Value::Text(String::from_utf8_lossy(&key).to_string()),
            );
            rows.push(DnaQlRow { fields: row_map });
        }

        let mut plan = vec![format!("SeqScan({})", source)];
        for stage in stages {
            match stage {
                PipelineStage::Link(spec) => {
                    let spec = spec.clone().ok_or("DnaQL: LINK requires a target")?;
                    plan.push(format!("KeySeek({}.{})", spec.chromosome, spec.column));
                    for row in rows.iter_mut() {
                        let fk_val = row.fields.get(&spec.column).cloned().unwrap_or(Value::Null);
                        let target_key = format!("{}:{}", spec.chromosome, fk_val);
                        if let Some(target_bytes) = self
                            .engine
                            .get(target_key.as_bytes())
                            .map_err(|e| format!("DnaQL storage error: {}", e))?
                        {
                            let target_fields = RowCodec::decode(&target_bytes)
                                .map_err(|e| format!("DnaQL: link target unreadable: {}", e))?;
                            for (k, v) in target_fields {
                                let mut name = format!("{}.{}", spec.chromosome, k);
                                if row.fields.contains_key(&name) {
                                    name = format!("link_{}", k);
                                }
                                row.fields.insert(name, v);
                            }
                        }
                    }
                }
                PipelineStage::Filter(e) => {
                    plan.push(format!("Filter({})", e.describe()));
                    rows.retain(|r| e.eval_bool(&r.fields));
                }
                PipelineStage::Emit(projection) => {
                    plan.push(format!("Project({:?})", projection));
                    for row in rows.iter_mut() {
                        let mut filtered = BTreeMap::new();
                        for p in projection {
                            if let Some(v) = row.fields.get(p.as_str()) {
                                filtered.insert(p.clone(), v.clone());
                            }
                        }
                        row.fields = filtered;
                    }
                }
                PipelineStage::Sort { column, asc } => {
                    plan.push(format!(
                        "Sort({} {})",
                        column,
                        if *asc { "ASC" } else { "DESC" }
                    ));
                    rows.sort_by(|a, b| {
                        let va = a.fields.get(column).cloned().unwrap_or(Value::Null);
                        let vb = b.fields.get(column).cloned().unwrap_or(Value::Null);
                        let ord = va.compare(&vb);
                        if *asc {
                            ord
                        } else {
                            ord.reverse()
                        }
                    });
                }
                PipelineStage::Limit(n) => {
                    plan.push(format!("Limit({})", n));
                    rows.truncate(*n);
                }
            }
        }

        let note = format!("real pipeline: {}", plan.join(" -> "));
        Ok((rows, note))
    }

    fn exec_tx(&self, statements: &[TxStmt]) -> Result<(Vec<DnaQlRow>, String), String> {
        // 1. Buffer mutations into a transaction session. Reads go THROUGH
        // the session (read-your-writes: a statement sees earlier statements'
        // buffered effects) and record Serializable validation baselines.
        let mut session = self.tx.begin(crate::tx::TxIsolationLevel::Serializable);

        for stmt in statements {
            match stmt {
                TxStmt::Mutate {
                    table,
                    locus,
                    field,
                    op,
                    value,
                } => {
                    let key = format!("{}:{}", table, locus);
                    let existing = session.read_row(&key)?;
                    let mut row = existing.unwrap_or_default();
                    let old = row.get(field).cloned().unwrap_or(Value::Null);
                    let new_val = apply_assign(&old, *op, value)?;
                    row.insert(field.clone(), new_val);
                    session.put_row(key.into_bytes(), row);
                }
                TxStmt::Append {
                    table,
                    locus,
                    fields,
                } => {
                    let key = format!("{}:{}", table, locus);
                    let existing = session.read_row(&key)?;
                    let mut row = existing.unwrap_or_default();
                    for (k, v) in fields {
                        row.insert(k.clone(), v.clone());
                    }
                    session.put_row(key.into_bytes(), row);
                }
                TxStmt::DropLocus {
                    table,
                    locus,
                    cascade: _,
                } => {
                    let key = format!("{}:{}", table, locus);
                    session.delete(key.into_bytes());
                }
            }
        }

        // 2. Atomic commit: baseline validation (Serializable) + one WAL batch
        // frame, one fsync. Conflicts surface as retryable errors here.
        let lsn = self.tx.commit(&mut session)?;

        // 3. Report resulting rows
        let mut out = Vec::new();
        for stmt in statements {
            match stmt {
                TxStmt::Mutate { table, locus, .. } | TxStmt::Append { table, locus, .. } => {
                    let key = format!("{}:{}", table, locus);
                    let mut fields = self.read_row(&key)?.unwrap_or_default();
                    fields.insert("locus".to_string(), Value::Text(key.clone()));
                    out.push(DnaQlRow { fields });
                }
                TxStmt::DropLocus { table, locus, .. } => {
                    let mut f = BTreeMap::new();
                    f.insert(
                        "dropped".to_string(),
                        Value::Text(format!("{}:{}", table, locus)),
                    );
                    out.push(DnaQlRow { fields: f });
                }
            }
        }

        Ok((
            out,
            format!(
                "real TX: {} statements committed atomically at LSN {}",
                statements.len(),
                lsn
            ),
        ))
    }

    fn read_row(&self, key: &str) -> Result<Option<BTreeMap<String, Value>>, String> {
        match self
            .engine
            .get(key.as_bytes())
            .map_err(|e| format!("storage: {}", e))?
        {
            Some(bytes) => {
                let fields = RowCodec::decode(&bytes)
                    .map_err(|e| format!("row {} unreadable: {}", key, e))?;
                Ok(Some(fields.into_iter().collect()))
            }
            None => Ok(None),
        }
    }

    /// VERIFY / REPAIR CHROMOSOME (P5.3): block-level integrity scan of
    /// every segment that holds keys of the chromosome.
    fn exec_health_check(
        &self,
        chromosome: &str,
        fix: bool,
    ) -> Result<(Vec<DnaQlRow>, String), String> {
        let prefix = format!("{}:", chromosome);
        let segments = self
            .engine
            .segments_with_prefix(prefix.as_bytes())
            .map_err(|e| format!("DnaQL storage error: {}", e))?;
        if segments.is_empty() {
            return Ok((
                Vec::new(),
                format!("chromosome '{}' has no stored segments", chromosome),
            ));
        }
        let (blocks, lesions, repaired, unrecoverable) = self
            .engine
            .verify_segments(&segments, fix)
            .map_err(|e| format!("DnaQL verify error: {}", e))?;

        let mut rows = Vec::new();
        for seg in &segments {
            let mut row = std::collections::BTreeMap::new();
            row.insert(
                "segment".to_string(),
                Value::Text(
                    seg.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("?")
                        .to_string(),
                ),
            );
            row.insert("format".to_string(), Value::Text("DNASST02".into()));
            rows.push(DnaQlRow { fields: row });
        }
        let note = format!(
            "{} chromosome '{}': {} segment(s), {} blocks scanned, {} lesion(s), {} repaired, {} unrecoverable stripe(s){}",
            if fix { "REPAIR" } else { "VERIFY" },
            chromosome,
            segments.len(),
            blocks,
            lesions,
            repaired,
            unrecoverable,
            if !fix && lesions > 0 {
                " (run REPAIR CHROMOSOME to fix)"
            } else {
                ""
            }
        );
        // Attach summary as the final row for tabular consumers.
        let mut summary = std::collections::BTreeMap::new();
        summary.insert("segment".to_string(), Value::Text("SUMMARY".into()));
        summary.insert("blocks_scanned".to_string(), Value::Int(blocks as i64));
        summary.insert("lesions_found".to_string(), Value::Int(lesions as i64));
        summary.insert("lesions_repaired".to_string(), Value::Int(repaired as i64));
        summary.insert(
            "unrecoverable_stripes".to_string(),
            Value::Int(unrecoverable as i64),
        );
        rows.push(DnaQlRow { fields: summary });
        Ok((rows, note))
    }

    fn exec_bio(
        &self,
        sequence: Option<String>,
        locus: Option<(String, i64)>,
        functions: &[String],
    ) -> Result<(Vec<DnaQlRow>, String), String> {
        // v4.1: STORAGE_MAP() — the per-column storage map of one locus.
        // Works on ANY row (no sequence field required) and decodes every
        // value back to the human-readable form — the DnaQL counterpart of
        // the SQL statement SHOW STORAGE ... WHERE <pk> = <literal>.
        if functions.iter().any(|f| f == "STORAGE_MAP") {
            if functions.len() != 1 {
                return Err(
                    "DnaQL: STORAGE_MAP() must be the only pipeline function".into(),
                );
            }
            let Some((table, id)) = locus else {
                return Err(
                    "DnaQL: STORAGE_MAP() needs ANALYZE LOCUS <table>:<locus>".into(),
                );
            };
            return self.exec_storage_map(&table, id);
        }
        let seq_str = match (sequence, locus) {
            (Some(s), _) => s,
            (None, Some((table, id))) => {
                let key = format!("{}:{}", table, id);
                let row = self
                    .read_row(&key)?
                    .ok_or_else(|| format!("DnaQL: locus {} not found", key))?;
                match row.get("sequence") {
                    Some(Value::Dna(s)) | Some(Value::Text(s)) => s.clone(),
                    _ => return Err(format!("DnaQL: locus {} has no sequence field", key)),
                }
            }
            (None, None) => return Err("DnaQL: ANALYZE requires SEQUENCE or LOCUS".into()),
        };
        let packed = PackedDna::from_ascii(&seq_str)
            .map_err(|e| format!("DnaQL: invalid sequence: {}", e))?;

        let mut out = Vec::new();
        for fname in functions {
            let mut fields = BTreeMap::new();
            fields.insert("function".to_string(), Value::Text(fname.clone()));
            match fname.as_str() {
                "GC_CONTENT" => {
                    fields.insert(
                        "result".to_string(),
                        Value::Text(format!("{:.2}%", packed.gc_content() * 100.0)),
                    );
                }
                "MELTING_TEMP" => {
                    fields.insert(
                        "result".to_string(),
                        Value::Text(format!("{:.1} C", packed.melting_temp())),
                    );
                }
                "REV_COMP" => {
                    fields.insert(
                        "result".to_string(),
                        Value::Dna(packed.reverse_complement().to_ascii()),
                    );
                }
                "TRANSCRIBE" => {
                    fields.insert("result".to_string(), Value::Text(packed.transcribe()));
                }
                "TRANSLATE" => {
                    fields.insert("result".to_string(), Value::Text(packed.translate()));
                }
                other => return Err(format!("DnaQL: unknown bio function '{}'", other)),
            }
            out.push(DnaQlRow { fields });
        }
        Ok((out, format!("real bio-analysis of {} bp", packed.len)))
    }

    /// v4.1: ANALYZE LOCUS <table>:<id> |> STORAGE_MAP() — per-column map of
    /// one stored row: chromosome, locus, exact storage key, SQL type, the
    /// decoded human-readable value and the physical RowCodec representation.
    /// Same report as SQL `SHOW STORAGE ... WHERE <pk> = <literal>`.
    fn exec_storage_map(
        &self,
        table: &str,
        id: i64,
    ) -> Result<(Vec<DnaQlRow>, String), String> {
        let key = format!("{}:{}", table, id);
        let row = self
            .read_row(&key)?
            .ok_or_else(|| format!("DnaQL: locus {} not found", key))?;
        let schema = self.sql.get_schema(table);
        let mut out = Vec::new();
        let mut payload_bytes = 0usize;
        for (fname, value) in row {
            // approximate the per-field wire size the way RowCodec does:
            // 4 B key length + key + 1 B tag + payload
            payload_bytes += 4 + fname.len() + 1
                + match &value {
                    Value::Null => 0,
                    Value::Int(_) | Value::Float(_) => 8,
                    Value::Text(s) | Value::Dna(s) => 4 + s.len(),
                };
            let mut fields = BTreeMap::new();
            fields.insert(
                "function".to_string(),
                Value::Text("STORAGE_MAP".to_string()),
            );
            fields.insert(
                "chromosome".to_string(),
                Value::Text(table.to_string()),
            );
            fields.insert("locus".to_string(), Value::Int(id));
            fields.insert("storage_key".to_string(), Value::Text(key.clone()));
            fields.insert("column".to_string(), Value::Text(fname.clone()));
            fields.insert(
                "sql_type".to_string(),
                Value::Text(match &schema {
                    Some(s) => s.column_type_name(&fname),
                    None => value.type_name().to_string(),
                }),
            );
            fields.insert("value".to_string(), Value::Text(value.to_string()));
            fields.insert(
                "stored_as".to_string(),
                Value::Text(value.stored_as()),
            );
            out.push(DnaQlRow { fields });
        }
        let note = format!(
            "storage map of locus {}:{} ({} column(s), ~{} B payload); values are \
             decoded for the operator, stored_as describes the physical bytes",
            table, id, out.len(), payload_bytes
        );
        Ok((out, note))
    }
}

pub(crate) fn apply_assign(old: &Value, op: AssignOp, v: &Value) -> Result<Value, String> {
    match op {
        AssignOp::Set => Ok(v.clone()),
        AssignOp::Add => match (old, v) {
            // Checked arithmetic: TX mutations are durable, so a silently
            // wrapped i64 (e.g. i64::MAX + 1 becoming negative) would
            // corrupt persisted balances/counts. Overflow aborts the
            // mutation instead.
            (Value::Int(a), Value::Int(b)) => a
                .checked_add(*b)
                .map(Value::Int)
                .ok_or_else(|| "DnaQL: integer overflow in +=".to_string()),
            (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a + b)),
            (Value::Int(a), Value::Float(b)) => Ok(Value::Float(*a as f64 + b)),
            (Value::Float(a), Value::Int(b)) => Ok(Value::Float(a + *b as f64)),
            _ => Err("DnaQL: += requires numeric operands".into()),
        },
        AssignOp::Sub => match (old, v) {
            (Value::Int(a), Value::Int(b)) => a
                .checked_sub(*b)
                .map(Value::Int)
                .ok_or_else(|| "DnaQL: integer overflow in -=".to_string()),
            (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a - b)),
            (Value::Int(a), Value::Float(b)) => Ok(Value::Float(*a as f64 - b)),
            (Value::Float(a), Value::Int(b)) => Ok(Value::Float(a - *b as f64)),
            _ => Err("DnaQL: -= requires numeric operands".into()),
        },
    }
}

fn ast_summary(ast: &DnaQlAst) -> String {
    match ast {
        DnaQlAst::PatternSearch {
            chromosome,
            pattern,
            mode,
            max_distance,
            link,
            ..
        } => {
            format!(
                "PatternSearch {{ chr: '{}', pattern: '{}', mode: {:?}, k: {}, link: {} }}",
                chromosome,
                pattern,
                mode,
                max_distance,
                link.as_ref()
                    .map(|l| format!("{}({})", l.chromosome, l.column))
                    .unwrap_or_else(|| "none".into())
            )
        }
        DnaQlAst::PipelineQuery { source, stages } => {
            format!(
                "PipelineQuery {{ source: '{}', stages: {} }}",
                source,
                stages.len()
            )
        }
        DnaQlAst::Tx { statements } => {
            format!("Tx {{ statements: {} }}", statements.len())
        }
        DnaQlAst::BioAnalysis { functions, .. } => {
            format!("BioAnalysis {{ functions: {:?} }}", functions)
        }
        DnaQlAst::HealthCheck { chromosome, fix } => {
            format!("HealthCheck {{ chr: '{}', fix: {} }}", chromosome, fix)
        }
        DnaQlAst::CreateChromosome {
            table,
            definitions,
            if_not_exists,
        } => {
            format!(
                "CreateChromosome {{ table: '{}', definitions: {}, if_not_exists: {} }}",
                table,
                definitions.len(),
                if_not_exists
            )
        }
        DnaQlAst::DropChromosome { table, if_exists } => {
            format!(
                "DropChromosome {{ table: '{}', if_exists: {} }}",
                table, if_exists
            )
        }
    }
}
