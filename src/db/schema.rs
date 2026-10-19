//! 数据库结构导出：把 schema 变成适合喂给 AI/Agent 的上下文文档。
//!
//! 输出格式：
//! - `AiContext`  面向 LLM 的结构说明书（概览 + 每表详解 + 关系 + 约束 + 样例数据 + 写 SQL 提示）
//! - `Markdown`   纯结构文档
//! - `Ddl`        可执行 T-SQL 建库脚本
//! - `Json`       结构化元数据
//! - `Mermaid`    ER 图

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tiberius::Row;

use crate::db::conn::SqlClient;
use crate::db::export::TaskCtx;
use crate::db::value::row_to_cells;
use crate::model::{Cell, SchemaFormat, SchemaSettings, Summary, TableRef};

// ---------------------------------------------------------------- 快照模型

#[derive(Clone, Debug, Default)]
pub struct DbInfo {
    pub name: String,
    pub version: String,
    pub edition: String,
    pub collation: String,
    pub compat_level: i32,
    pub recovery_model: String,
}

#[derive(Clone, Debug)]
pub struct ColumnSchema {
    pub name: String,
    pub type_name: String,
    pub max_length: i32,
    pub precision: i32,
    pub scale: i32,
    pub nullable: bool,
    pub identity: bool,
    pub computed: Option<String>,
    pub default: Option<String>,
    pub collation: Option<String>,
    pub description: Option<String>,
}

impl ColumnSchema {
    /// T-SQL 类型声明
    pub fn sql_type(&self) -> String {
        let t = self.type_name.to_ascii_lowercase();
        match t.as_str() {
            "varchar" | "char" | "varbinary" | "binary" => {
                if self.max_length < 0 {
                    format!("{}(max)", t)
                } else {
                    format!("{}({})", t, self.max_length)
                }
            }
            "nvarchar" | "nchar" => {
                if self.max_length < 0 {
                    format!("{}(max)", t)
                } else {
                    format!("{}({})", t, (self.max_length / 2).max(1))
                }
            }
            "decimal" | "numeric" => format!("{}({},{})", t, self.precision, self.scale),
            "datetime2" | "time" | "datetimeoffset" => {
                format!("{}({})", t, self.scale.clamp(0, 7))
            }
            _ => t,
        }
    }
    /// 简化类型（用于 Markdown 表格与 Mermaid）
    pub fn short_type(&self) -> String {
        let t = self.type_name.to_ascii_lowercase();
        match t.as_str() {
            "varchar" | "char" | "nvarchar" | "nchar" | "varbinary" | "binary" => {
                if self.max_length < 0 {
                    format!("{}(max)", t)
                } else if t.starts_with('n') {
                    format!("{}({})", t, (self.max_length / 2).max(1))
                } else {
                    format!("{}({})", t, self.max_length)
                }
            }
            "decimal" | "numeric" => format!("{}({},{})", t, self.precision, self.scale),
            "datetime2" | "time" | "datetimeoffset" => {
                format!("{}({})", t, self.scale.clamp(0, 7))
            }
            _ => t,
        }
    }
}

#[derive(Clone, Debug)]
pub struct IndexSchema {
    pub name: String,
    pub is_unique: bool,
    pub is_primary_key: bool,
    pub is_unique_constraint: bool,
    pub kind: String,
    pub columns: Vec<String>,
    pub includes: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ForeignKey {
    pub name: String,
    pub columns: Vec<String>,
    pub ref_schema: String,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    pub on_delete: String,
    pub on_update: String,
}

#[derive(Clone, Debug)]
pub struct CheckConstraint {
    pub name: String,
    pub definition: String,
}

#[derive(Clone, Debug)]
pub struct TableSchema {
    pub schema: String,
    pub name: String,
    pub is_view: bool,
    pub row_count: i64,
    pub description: Option<String>,
    pub columns: Vec<ColumnSchema>,
    pub primary_key: Vec<String>,
    pub indexes: Vec<IndexSchema>,
    pub foreign_keys: Vec<ForeignKey>,
    pub checks: Vec<CheckConstraint>,
    pub samples: Vec<Vec<Option<String>>>,
    pub sample_headers: Vec<String>,
}

impl TableSchema {
    pub fn display(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }
    pub fn qualified(&self) -> String {
        format!("[{}].[{}]", self.schema, self.name)
    }
}

#[derive(Clone, Debug)]
pub struct ModuleSchema {
    pub schema: String,
    pub name: String,
    /// V=view P=procedure FN/TF/IF=function TR=trigger
    pub kind: String,
    pub definition: String,
}

#[derive(Clone, Debug)]
pub struct SequenceSchema {
    pub schema: String,
    pub name: String,
    pub type_name: String,
    pub start: String,
    pub increment: String,
    pub current: String,
}

#[derive(Clone, Debug, Default)]
pub struct SchemaSnapshot {
    pub db: DbInfo,
    pub tables: Vec<TableSchema>,
    pub views: Vec<TableSchema>,
    pub modules: Vec<ModuleSchema>,
    pub sequences: Vec<SequenceSchema>,
}

// ---------------------------------------------------------------- 取值助手

fn s(row: &Row, i: usize) -> String {
    row.try_get::<&str, _>(i)
        .ok()
        .flatten()
        .unwrap_or_default()
        .to_string()
}

fn opt_s(row: &Row, i: usize) -> Option<String> {
    let v = s(row, i);
    if v.trim().is_empty() {
        None
    } else {
        Some(v)
    }
}

fn i32v(row: &Row, i: usize) -> i32 {
    row.try_get::<i32, _>(i)
        .ok()
        .flatten()
        .or_else(|| row.try_get::<i16, _>(i).ok().flatten().map(|v| v as i32))
        .or_else(|| row.try_get::<u8, _>(i).ok().flatten().map(|v| v as i32))
        .unwrap_or(0)
}

fn i64v(row: &Row, i: usize) -> i64 {
    row.try_get::<i64, _>(i)
        .ok()
        .flatten()
        .or_else(|| row.try_get::<i32, _>(i).ok().flatten().map(|v| v as i64))
        .unwrap_or(0)
}

fn boolv(row: &Row, i: usize) -> bool {
    row.try_get::<bool, _>(i).ok().flatten().unwrap_or(false)
}

// ---------------------------------------------------------------- 采集

async fn fetch_db_info(client: &mut SqlClient) -> DbInfo {
    let q = "SELECT DB_NAME(),
                    CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)),
                    CAST(SERVERPROPERTY('Edition') AS nvarchar(128)),
                    CONVERT(nvarchar(128), DATABASEPROPERTYEX(DB_NAME(), 'Collation')),
                    compatibility_level,
                    recovery_model_desc
             FROM sys.databases WHERE name = DB_NAME()";
    match client.query(q, &[]).await {
        Ok(st) => match st.into_first_result().await {
            Ok(rows) => rows.first().map(|r| DbInfo {
                name: s(r, 0),
                version: s(r, 1),
                edition: s(r, 2),
                collation: s(r, 3),
                compat_level: i32v(r, 4),
                recovery_model: s(r, 5),
            }),
            Err(_) => None,
        },
        Err(_) => None,
    }
    .unwrap_or_default()
}

async fn fetch_objects(client: &mut SqlClient) -> Result<Vec<TableSchema>> {
    let q = "SELECT s.name, o.name, o.type,
                    ISNULL(SUM(p.rows), 0) AS row_count,
                    CAST(ep.value AS nvarchar(4000)) AS descr
             FROM sys.objects o
             JOIN sys.schemas s ON s.schema_id = o.schema_id
             LEFT JOIN sys.partitions p
                    ON p.object_id = o.object_id AND p.index_id IN (0, 1)
             LEFT JOIN sys.extended_properties ep
                    ON ep.class = 1 AND ep.major_id = o.object_id
                   AND ep.minor_id = 0 AND ep.name = 'MS_Description'
             WHERE o.type IN ('U', 'V') AND o.is_ms_shipped = 0
             GROUP BY s.name, o.name, o.type, CAST(ep.value AS nvarchar(4000))
             ORDER BY o.type, s.name, o.name";
    let rows = client.query(q, &[]).await?.into_first_result().await?;
    Ok(rows
        .iter()
        .map(|r| TableSchema {
            schema: s(r, 0),
            name: s(r, 1),
            is_view: s(r, 2).trim() == "V",
            row_count: i64v(r, 3),
            description: opt_s(r, 4),
            columns: Vec::new(),
            primary_key: Vec::new(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            checks: Vec::new(),
            samples: Vec::new(),
            sample_headers: Vec::new(),
        })
        .collect())
}

async fn fetch_columns(client: &mut SqlClient) -> Result<BTreeMap<String, Vec<ColumnSchema>>> {
    let q = "SELECT s.name, o.name, c.name, t.name,
                    c.max_length, c.precision, c.scale,
                    c.is_nullable, c.is_identity, c.is_computed,
                    cc.definition, dc.definition, c.collation_name,
                    CAST(ep.value AS nvarchar(4000))
             FROM sys.columns c
             JOIN sys.objects o ON o.object_id = c.object_id
                               AND o.type IN ('U','V') AND o.is_ms_shipped = 0
             JOIN sys.schemas s ON s.schema_id = o.schema_id
             JOIN sys.types t ON t.user_type_id = c.user_type_id
             LEFT JOIN sys.computed_columns cc
                    ON cc.object_id = c.object_id AND cc.column_id = c.column_id
             LEFT JOIN sys.default_constraints dc
                    ON dc.parent_object_id = c.object_id AND dc.parent_column_id = c.column_id
             LEFT JOIN sys.extended_properties ep
                    ON ep.class = 1 AND ep.major_id = c.object_id
                   AND ep.minor_id = c.column_id AND ep.name = 'MS_Description'
             ORDER BY s.name, o.name, c.column_id";
    let rows = client.query(q, &[]).await?.into_first_result().await?;

    let mut out: BTreeMap<String, Vec<ColumnSchema>> = BTreeMap::new();
    for r in rows.iter() {
        let key = format!("{}.{}", s(r, 0), s(r, 1));
        out.entry(key).or_default().push(ColumnSchema {
            name: s(r, 2),
            type_name: s(r, 3).to_ascii_lowercase(),
            max_length: i32v(r, 4),
            precision: i32v(r, 5),
            scale: i32v(r, 6),
            nullable: boolv(r, 7),
            identity: boolv(r, 8),
            computed: opt_s(r, 10),
            default: opt_s(r, 11),
            collation: opt_s(r, 12),
            description: opt_s(r, 13),
        });
    }
    Ok(out)
}

async fn fetch_indexes(client: &mut SqlClient) -> Result<BTreeMap<String, Vec<IndexSchema>>> {
    let q = "SELECT s.name, o.name, i.name, i.is_unique, i.is_primary_key,
                    i.is_unique_constraint, i.type_desc,
                    c.name, ic.key_ordinal, ic.is_descending_key, ic.is_included_column
             FROM sys.indexes i
             JOIN sys.objects o ON o.object_id = i.object_id
                               AND o.type = 'U' AND o.is_ms_shipped = 0
             JOIN sys.schemas s ON s.schema_id = o.schema_id
             JOIN sys.index_columns ic
                    ON ic.object_id = i.object_id AND ic.index_id = i.index_id
             JOIN sys.columns c
                    ON c.object_id = ic.object_id AND c.column_id = ic.column_id
             WHERE i.type > 0
             ORDER BY s.name, o.name, i.index_id, ic.is_included_column, ic.key_ordinal,
                      ic.index_column_id";
    let rows = client.query(q, &[]).await?.into_first_result().await?;

    // (table, index) -> IndexSchema
    let mut map: BTreeMap<(String, String), IndexSchema> = BTreeMap::new();
    for r in rows.iter() {
        let table = format!("{}.{}", s(r, 0), s(r, 1));
        let idx_name = s(r, 2);
        let key = (table.clone(), idx_name.clone());
        let entry = map.entry(key).or_insert_with(|| IndexSchema {
            name: idx_name.clone(),
            is_unique: boolv(r, 3),
            is_primary_key: boolv(r, 4),
            is_unique_constraint: boolv(r, 5),
            kind: s(r, 6),
            columns: Vec::new(),
            includes: Vec::new(),
        });
        let col = s(r, 7);
        if boolv(r, 10) {
            entry.includes.push(col);
        } else {
            let desc = boolv(r, 9);
            entry
                .columns
                .push(if desc { format!("{} DESC", col) } else { col });
        }
    }

    let mut out: BTreeMap<String, Vec<IndexSchema>> = BTreeMap::new();
    for ((table, _), idx) in map {
        out.entry(table).or_default().push(idx);
    }
    Ok(out)
}

async fn fetch_foreign_keys(client: &mut SqlClient) -> Result<BTreeMap<String, Vec<ForeignKey>>> {
    let q = "SELECT ps.name, po.name, fk.name,
                    pc.name, rs.name, ro.name, rc.name,
                    fk.delete_referential_action_desc,
                    fk.update_referential_action_desc
             FROM sys.foreign_keys fk
             JOIN sys.foreign_key_columns fkc
                    ON fkc.constraint_object_id = fk.object_id
             JOIN sys.objects po ON po.object_id = fk.parent_object_id
             JOIN sys.schemas ps ON ps.schema_id = po.schema_id
             JOIN sys.columns pc
                    ON pc.object_id = fkc.parent_object_id
                   AND pc.column_id = fkc.parent_column_id
             JOIN sys.objects ro ON ro.object_id = fk.referenced_object_id
             JOIN sys.schemas rs ON rs.schema_id = ro.schema_id
             JOIN sys.columns rc
                    ON rc.object_id = fkc.referenced_object_id
                   AND rc.column_id = fkc.referenced_column_id
             ORDER BY ps.name, po.name, fk.name, fkc.constraint_column_id";
    let rows = client.query(q, &[]).await?.into_first_result().await?;

    let mut map: BTreeMap<(String, String), ForeignKey> = BTreeMap::new();
    for r in rows.iter() {
        let table = format!("{}.{}", s(r, 0), s(r, 1));
        let fk_name = s(r, 2);
        let entry = map
            .entry((table, fk_name.clone()))
            .or_insert_with(|| ForeignKey {
                name: fk_name,
                columns: Vec::new(),
                ref_schema: s(r, 4),
                ref_table: s(r, 5),
                ref_columns: Vec::new(),
                on_delete: s(r, 7),
                on_update: s(r, 8),
            });
        entry.columns.push(s(r, 3));
        entry.ref_columns.push(s(r, 6));
    }

    let mut out: BTreeMap<String, Vec<ForeignKey>> = BTreeMap::new();
    for ((table, _), fk) in map {
        out.entry(table).or_default().push(fk);
    }
    Ok(out)
}

async fn fetch_checks(client: &mut SqlClient) -> Result<BTreeMap<String, Vec<CheckConstraint>>> {
    let q = "SELECT s.name, o.name, cc.name, cc.definition
             FROM sys.check_constraints cc
             JOIN sys.objects o ON o.object_id = cc.parent_object_id
             JOIN sys.schemas s ON s.schema_id = o.schema_id
             WHERE o.is_ms_shipped = 0
             ORDER BY s.name, o.name, cc.name";
    let rows = client.query(q, &[]).await?.into_first_result().await?;
    let mut out: BTreeMap<String, Vec<CheckConstraint>> = BTreeMap::new();
    for r in rows.iter() {
        out.entry(format!("{}.{}", s(r, 0), s(r, 1)))
            .or_default()
            .push(CheckConstraint {
                name: s(r, 2),
                definition: s(r, 3),
            });
    }
    Ok(out)
}

async fn fetch_modules(client: &mut SqlClient) -> Result<Vec<ModuleSchema>> {
    let q = "SELECT s.name, o.name, o.type, m.definition
             FROM sys.objects o
             JOIN sys.schemas s ON s.schema_id = o.schema_id
             JOIN sys.sql_modules m ON m.object_id = o.object_id
             WHERE o.is_ms_shipped = 0 AND o.type IN ('V','P','FN','IF','TF','TR')
             ORDER BY o.type, s.name, o.name";
    let rows = client.query(q, &[]).await?.into_first_result().await?;
    Ok(rows
        .iter()
        .map(|r| ModuleSchema {
            schema: s(r, 0),
            name: s(r, 1),
            kind: s(r, 2),
            definition: s(r, 3),
        })
        .collect())
}

async fn fetch_sequences(client: &mut SqlClient) -> Result<Vec<SequenceSchema>> {
    let q = "SELECT s.name, q.name, t.name,
                    CAST(q.start_value AS nvarchar(60)),
                    CAST(q.increment AS nvarchar(60)),
                    CAST(q.current_value AS nvarchar(60))
             FROM sys.sequences q
             JOIN sys.schemas s ON s.schema_id = q.schema_id
             JOIN sys.types t ON t.user_type_id = q.user_type_id
             ORDER BY s.name, q.name";
    let rows = client.query(q, &[]).await?.into_first_result().await?;
    Ok(rows
        .iter()
        .map(|r| SequenceSchema {
            schema: s(r, 0),
            name: s(r, 1),
            type_name: s(r, 2),
            start: s(r, 3),
            increment: s(r, 4),
            current: s(r, 5),
        })
        .collect())
}

async fn fetch_samples(
    client: &mut SqlClient,
    table: &TableSchema,
    limit: u32,
    max_len: usize,
) -> Result<(Vec<String>, Vec<Vec<Option<String>>>)> {
    let sql = format!("SELECT TOP ({}) * FROM {}", limit, table.qualified());
    let rows = client.query(sql, &[]).await?.into_first_result().await?;
    let mut headers: Vec<String> = Vec::new();
    let mut data: Vec<Vec<Option<String>>> = Vec::new();
    for row in rows.iter() {
        if headers.is_empty() {
            headers = row.columns().iter().map(|c| c.name().to_string()).collect();
        }
        let cells = row_to_cells(row);
        let vals = cells
            .iter()
            .map(|c| match c {
                Cell::Null => None,
                Cell::Text(t) => Some(clip(t, max_len)),
                Cell::Bytes(b) => Some(format!("<{} bytes>", b.len())),
            })
            .collect();
        data.push(vals);
    }
    Ok((headers, data))
}

fn clip(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if max == 0 || n <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// 采集完整结构快照
pub async fn collect(
    client: &mut SqlClient,
    settings: &SchemaSettings,
    selected: &[TableRef],
    ctx: &TaskCtx,
) -> Result<SchemaSnapshot> {
    let mut snap = SchemaSnapshot {
        db: fetch_db_info(client).await,
        ..Default::default()
    };
    if snap.db.name.is_empty() {
        snap.db.name = "unknown".into();
    }

    ctx.progress(0.05, "读取对象清单…", 0, 0);
    let all_objects = fetch_objects(client).await?;

    // 选择范围
    let wanted: BTreeSet<String> = if settings.use_selection && !selected.is_empty() {
        selected.iter().map(|t| t.display()).collect()
    } else {
        BTreeSet::new()
    };
    let pick = |t: &TableSchema| -> bool {
        if !settings.include_system && (t.schema == "sys" || t.name.starts_with("sys")) {
            return false;
        }
        if wanted.is_empty() {
            true
        } else {
            wanted.contains(&t.display())
        }
    };

    ctx.progress(0.12, "读取列定义…", 0, 0);
    let mut cols = fetch_columns(client).await?;
    ctx.progress(0.22, "读取索引…", 0, 0);
    let mut indexes = fetch_indexes(client).await?;
    ctx.progress(0.30, "读取外键…", 0, 0);
    let mut fks = fetch_foreign_keys(client).await?;
    ctx.progress(0.36, "读取约束…", 0, 0);
    let mut checks = fetch_checks(client).await?;

    for mut t in all_objects {
        if !pick(&t) {
            continue;
        }
        let key = t.display();
        t.columns = cols.remove(&key).unwrap_or_default();
        t.primary_key = indexes
            .get(&key)
            .and_then(|v| v.iter().find(|i| i.is_primary_key))
            .map(|i| {
                i.columns
                    .iter()
                    .map(|c| c.trim_end_matches(" DESC").to_string())
                    .collect()
            })
            .unwrap_or_default();
        t.indexes = indexes.remove(&key).unwrap_or_default();
        t.foreign_keys = fks.remove(&key).unwrap_or_default();
        t.checks = checks.remove(&key).unwrap_or_default();
        if t.is_view {
            snap.views.push(t);
        } else {
            snap.tables.push(t);
        }
    }

    // 视图定义与存储过程
    if settings.include_views || settings.include_procs || settings.include_triggers {
        ctx.progress(0.45, "读取模块定义…", 0, 0);
        let mods = fetch_modules(client).await?;
        for m in mods {
            let keep = match m.kind.as_str() {
                "V" => settings.include_views,
                "P" => settings.include_procs,
                "FN" | "IF" | "TF" => settings.include_procs,
                "TR" => settings.include_triggers,
                _ => false,
            };
            if keep {
                snap.modules.push(m);
            }
        }
    }

    if settings.include_sequences {
        ctx.progress(0.5, "读取序列…", 0, 0);
        snap.sequences = fetch_sequences(client).await.unwrap_or_default();
    }

    // 样例数据
    if settings.sample_rows > 0 {
        let total = snap.tables.len().max(1);
        let mut done = 0usize;
        for t in snap.tables.iter_mut() {
            if ctx.cancelled() {
                break;
            }
            done += 1;
            ctx.progress(
                0.5 + 0.45 * (done as f32 / total as f32),
                format!("采样 {}", t.display()),
                0,
                0,
            );
            if let Ok((h, d)) =
                fetch_samples(client, t, settings.sample_rows, settings.max_value_len).await
            {
                t.sample_headers = h;
                t.samples = d;
            }
        }
    }

    ctx.progress(0.98, "生成文档…", 0, 0);
    Ok(snap)
}

// ---------------------------------------------------------------- 渲染：通用

fn md_escape(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ").replace('\r', "")
}

fn human(n: i64) -> String {
    if n < 0 {
        return "—".into();
    }
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn type_kind(t: &str) -> &'static str {
    match t {
        "int" | "bigint" | "smallint" | "tinyint" => "整数",
        "decimal" | "numeric" | "money" | "smallmoney" => "定点小数",
        "float" | "real" => "浮点数",
        "bit" => "布尔",
        "date" | "time" | "datetime" | "datetime2" | "smalldatetime" | "datetimeoffset" => {
            "日期时间"
        }
        "uniqueidentifier" => "GUID",
        "binary" | "varbinary" | "image" | "timestamp" | "rowversion" => "二进制",
        _ => "文本",
    }
}

// ---------------------------------------------------------------- 渲染：Markdown / AI

fn column_table(cols: &[ColumnSchema], pk: &[String], show_extra: bool) -> String {
    let mut out = String::new();
    if show_extra {
        out.push_str("| 列 | 类型 | 可空 | 键 | 默认值 | 说明 |\n");
        out.push_str("|---|---|---|---|---|---|\n");
    } else {
        out.push_str("| 列 | 类型 | 可空 | 说明 |\n");
        out.push_str("|---|---|---|---|\n");
    }
    for c in cols {
        let keys = {
            let mut k = Vec::new();
            if pk.iter().any(|p| p.eq_ignore_ascii_case(&c.name)) {
                k.push("PK");
            }
            if c.identity {
                k.push("IDENTITY");
            }
            if c.computed.is_some() {
                k.push("COMPUTED");
            }
            k.join(" ")
        };
        let def = c.default.clone().unwrap_or_else(|| "—".into());
        let note = c
            .description
            .clone()
            .or_else(|| c.computed.as_ref().map(|d| format!("AS {}", d)))
            .unwrap_or_default();
        if show_extra {
            let _ = writeln!(
                out,
                "| `{}` | {} | {} | {} | {} | {} |",
                c.name,
                c.short_type(),
                if c.nullable { "是" } else { "否" },
                if keys.is_empty() {
                    "—".to_string()
                } else {
                    keys
                },
                md_escape(&def),
                md_escape(&note)
            );
        } else {
            let _ = writeln!(
                out,
                "| `{}` | {} | {} | {} |",
                c.name,
                c.short_type(),
                if c.nullable { "是" } else { "否" },
                md_escape(&note)
            );
        }
    }
    out
}

fn sample_table(headers: &[String], rows: &[Vec<Option<String>>]) -> String {
    if headers.is_empty() || rows.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push('|');
    for h in headers {
        let _ = write!(out, " {} |", md_escape(h));
    }
    out.push('\n');
    out.push('|');
    for _ in headers {
        out.push_str("---|");
    }
    out.push('\n');
    for r in rows {
        out.push('|');
        for i in 0..headers.len() {
            let v = r.get(i).cloned().flatten().unwrap_or_else(|| "NULL".into());
            let _ = write!(out, " {} |", md_escape(&v));
        }
        out.push('\n');
    }
    out
}

fn relations_section(snap: &SchemaSnapshot) -> String {
    let mut out = String::new();
    let mut any = false;
    for t in &snap.tables {
        for fk in &t.foreign_keys {
            any = true;
            let _ = writeln!(
                out,
                "- `{}`({}) → `{}.{}`({}){}{}",
                t.display(),
                fk.columns.join(", "),
                fk.ref_schema,
                fk.ref_table,
                fk.ref_columns.join(", "),
                if fk.on_delete != "NO_ACTION" {
                    format!("  ON DELETE {}", fk.on_delete)
                } else {
                    String::new()
                },
                if fk.on_update != "NO_ACTION" {
                    format!("  ON UPDATE {}", fk.on_update)
                } else {
                    String::new()
                }
            );
        }
    }
    if !any {
        out.push_str("_未定义外键约束。表之间的关联需要根据列名推断。_\n");
    }
    out
}

/// 从结构里自动推导「写 SQL 时要注意什么」，对 AI 非常关键
fn sql_notes(snap: &SchemaSnapshot) -> String {
    let mut out = String::new();

    let mut idents: Vec<String> = Vec::new();
    let mut computed: Vec<String> = Vec::new();
    let mut required: Vec<String> = Vec::new();
    let mut no_pk: Vec<String> = Vec::new();
    let mut decimals: Vec<String> = Vec::new();
    let mut maxlen: Vec<String> = Vec::new();

    for t in &snap.tables {
        if t.primary_key.is_empty() {
            no_pk.push(t.display());
        }
        for c in &t.columns {
            let q = format!("{}.{}", t.display(), c.name);
            if c.identity {
                idents.push(q.clone());
            }
            if c.computed.is_some() {
                computed.push(q.clone());
            }
            if !c.nullable && !c.identity && c.computed.is_none() && c.default.is_none() {
                required.push(q.clone());
            }
            if matches!(
                c.type_name.as_str(),
                "decimal" | "numeric" | "money" | "smallmoney"
            ) {
                decimals.push(format!("{} {}", q, c.short_type()));
            }
            if c.max_length == -1 && matches!(c.type_name.as_str(), "nvarchar" | "varchar") {
                maxlen.push(q);
            }
        }
    }

    if !idents.is_empty() {
        let _ = writeln!(
            out,
            "- **IDENTITY 列**（不要显式插入值，除非 `SET IDENTITY_INSERT ON`）：{}",
            idents.join("、")
        );
    }
    if !computed.is_empty() {
        let _ = writeln!(
            out,
            "- **计算列**（只读，INSERT/UPDATE 时不要出现在列清单中）：{}",
            computed.join("、")
        );
    }
    if !required.is_empty() {
        let _ = writeln!(
            out,
            "- **无默认值的 NOT NULL 列**（INSERT 必须提供）：{}",
            required.join("、")
        );
    }
    if !no_pk.is_empty() {
        let _ = writeln!(
            out,
            "- **无主键的表**（UPDATE/DELETE 需谨慎，务必带完整过滤条件）：{}",
            no_pk.join("、")
        );
    }
    if !decimals.is_empty() {
        let _ = writeln!(
            out,
            "- **精确小数列**（请使用精确字面量，不要经过浮点运算）：{}",
            decimals
                .iter()
                .take(20)
                .cloned()
                .collect::<Vec<_>>()
                .join("、")
        );
    }
    if !maxlen.is_empty() {
        let _ = writeln!(
            out,
            "- **大文本列**（可能非常长，查询时避免无条件 SELECT）：{}",
            maxlen
                .iter()
                .take(20)
                .cloned()
                .collect::<Vec<_>>()
                .join("、")
        );
    }

    out.push_str("- 方言为 **Microsoft SQL Server (T-SQL)**：分页用 `ORDER BY ... OFFSET n ROWS FETCH NEXT m ROWS ONLY` 或 `TOP (n)`；字符串拼接用 `+`；日期字面量写成 `'YYYY-MM-DD'`。\n");
    out.push_str(
        "- 标识符用方括号包裹（如 `[dbo].[Orders]`），字符串用单引号，`N'...'` 表示 Unicode。\n",
    );
    out.push_str("- 判断 NULL 用 `IS NULL` / `IS NOT NULL`，不要用 `= NULL`。\n");
    out
}

fn overview_block(snap: &SchemaSnapshot) -> String {
    let total_rows: i64 = snap.tables.iter().map(|t| t.row_count.max(0)).sum();
    let mut out = String::new();
    let _ = writeln!(out, "| 项 | 值 |");
    let _ = writeln!(out, "|---|---|");
    let _ = writeln!(out, "| 数据库 | `{}` |", snap.db.name);
    let _ = writeln!(
        out,
        "| SQL Server 版本 | {} {} |",
        snap.db.version, snap.db.edition
    );
    let _ = writeln!(out, "| 排序规则 | {} |", snap.db.collation);
    let _ = writeln!(out, "| 兼容级别 | {} |", snap.db.compat_level);
    let _ = writeln!(out, "| 恢复模式 | {} |", snap.db.recovery_model);
    let _ = writeln!(out, "| 表 | {} 个 |", snap.tables.len());
    let _ = writeln!(out, "| 视图 | {} 个 |", snap.views.len());
    let _ = writeln!(
        out,
        "| 存储过程 / 函数 | {} 个 |",
        snap.modules.iter().filter(|m| m.kind != "V").count()
    );
    let _ = writeln!(out, "| 数据总行数（估算） | {} |", human(total_rows));
    out
}

fn object_list_table(snap: &SchemaSnapshot) -> String {
    let mut out = String::new();
    out.push_str("| 对象 | 类型 | 行数 | 列数 | 主键 | 说明 |\n");
    out.push_str("|---|---|---|---|---|---|\n");
    for t in &snap.tables {
        let _ = writeln!(
            out,
            "| `{}` | 表 | {} | {} | {} | {} |",
            t.display(),
            human(t.row_count),
            t.columns.len(),
            if t.primary_key.is_empty() {
                "—".to_string()
            } else {
                t.primary_key.join(", ")
            },
            md_escape(t.description.as_deref().unwrap_or(""))
        );
    }
    for v in &snap.views {
        let _ = writeln!(
            out,
            "| `{}` | 视图 | — | {} | — | {} |",
            v.display(),
            v.columns.len(),
            md_escape(v.description.as_deref().unwrap_or(""))
        );
    }
    out
}

fn table_detail(t: &TableSchema, settings: &SchemaSettings, heading: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{} `{}`", heading, t.display());
    out.push('\n');

    let pk: &[String] = if settings.include_pk {
        &t.primary_key
    } else {
        &[]
    };
    let mut meta_line: Vec<String> = Vec::new();
    if settings.include_row_counts && !t.is_view {
        meta_line.push(format!("行数约 {}", human(t.row_count)));
    }
    meta_line.push(format!("{} 列", t.columns.len()));
    if !pk.is_empty() {
        meta_line.push(format!("主键 ({})", pk.join(", ")));
    }
    let _ = writeln!(out, "{}\n", meta_line.join(" · "));

    if let Some(d) = &t.description {
        let _ = writeln!(out, "{}\n", d);
    }

    out.push_str(&column_table(
        &t.columns,
        pk,
        settings.include_defaults || settings.include_descriptions,
    ));
    out.push('\n');

    if settings.include_indexes && !t.indexes.is_empty() {
        out.push_str("**索引**\n\n");
        for i in &t.indexes {
            let mut flags: Vec<&str> = Vec::new();
            if i.is_primary_key {
                flags.push("主键");
            } else if i.is_unique_constraint {
                flags.push("唯一约束");
            } else if i.is_unique {
                flags.push("唯一");
            }
            if i.kind.contains("CLUSTERED") && !i.kind.contains("NONCLUSTERED") {
                flags.push("聚集");
            }
            let flag = if flags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", flags.join(", "))
            };
            let inc = if i.includes.is_empty() {
                String::new()
            } else {
                format!(" INCLUDE ({})", i.includes.join(", "))
            };
            let _ = writeln!(
                out,
                "- `{}`{} ({}){}",
                i.name,
                flag,
                i.columns.join(", "),
                inc
            );
        }
        out.push('\n');
    }

    if settings.include_fk && !t.foreign_keys.is_empty() {
        out.push_str("**外键**\n\n");
        for fk in &t.foreign_keys {
            let _ = writeln!(
                out,
                "- `{}` ({}) → `{}.{}` ({})",
                fk.name,
                fk.columns.join(", "),
                fk.ref_schema,
                fk.ref_table,
                fk.ref_columns.join(", ")
            );
        }
        out.push('\n');
    }

    if settings.include_checks && !t.checks.is_empty() {
        out.push_str("**检查约束**\n\n");
        for c in &t.checks {
            let _ = writeln!(out, "- `{}`: {}", c.name, clip(&c.definition, 240));
        }
        out.push('\n');
    }

    if !t.samples.is_empty() {
        let _ = writeln!(out, "**样例数据（前 {} 行）**\n", t.samples.len());
        out.push_str(&sample_table(&t.sample_headers, &t.samples));
        out.push('\n');
    }
    out
}

/// 生成 Markdown 结构文档
pub fn render_markdown(snap: &SchemaSnapshot, settings: &SchemaSettings) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# 数据库结构：{}\n", snap.db.name);
    if !settings.prompt_header.trim().is_empty() {
        let _ = writeln!(out, "{}\n", settings.prompt_header.trim());
    }
    let _ = writeln!(out, "## 概览\n");
    out.push_str(&overview_block(snap));
    out.push('\n');
    let _ = writeln!(out, "## 对象清单\n");
    out.push_str(&object_list_table(snap));
    out.push('\n');

    let _ = writeln!(out, "## 表结构\n");
    for t in &snap.tables {
        out.push_str(&table_detail(t, settings, "###"));
    }
    if !snap.views.is_empty() && settings.include_views {
        let _ = writeln!(out, "## 视图\n");
        for v in &snap.views {
            out.push_str(&table_detail(v, settings, "###"));
        }
    }
    out
}

/// 生成面向 AI/Agent 的完整上下文包
pub fn render_ai_context(snap: &SchemaSnapshot, settings: &SchemaSettings) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# 数据库结构上下文：{}\n", snap.db.name);
    if !settings.prompt_header.trim().is_empty() {
        let _ = writeln!(out, "{}\n---\n", settings.prompt_header.trim());
    }
    out.push_str("> 本文档由 MSSQL Transfer Studio 生成，用于向 AI / Agent 描述数据库结构。\n");
    out.push_str("> 请基于以下结构编写 SQL，不要臆造不存在的表或列。\n\n");

    // 小节按实际输出顺序连续编号，避免缺节时出现跳号
    let mut sec = 0usize;
    macro_rules! h2 {
        ($title:expr) => {{
            sec += 1;
            let _ = writeln!(out, "## {}. {}\n", sec, $title);
        }};
    }

    h2!("数据库概览");
    out.push_str(&overview_block(snap));
    out.push('\n');

    h2!("对象清单");
    out.push_str(&object_list_table(snap));
    out.push('\n');

    h2!("关系（外键）");
    out.push_str(&relations_section(snap));
    out.push('\n');

    h2!("表结构详解");
    for t in &snap.tables {
        out.push_str(&table_detail(t, settings, "###"));
        out.push_str("---\n\n");
    }

    if !snap.views.is_empty() && settings.include_views {
        h2!("视图");
        for v in &snap.views {
            out.push_str(&table_detail(v, settings, "###"));
        }
    }

    if !snap.modules.is_empty() {
        h2!("视图 / 存储过程 / 函数定义");
        for m in &snap.modules {
            let label = match m.kind.as_str() {
                "V" => "视图",
                "P" => "存储过程",
                "TR" => "触发器",
                _ => "函数",
            };
            let _ = writeln!(out, "### {} `{}.{}`\n", label, m.schema, m.name);
            out.push_str("```sql\n");
            out.push_str(m.definition.trim());
            out.push_str("\n```\n\n");
        }
    }

    if !snap.sequences.is_empty() {
        h2!("序列");
        out.push_str("| 序列 | 类型 | 起始 | 步长 | 当前值 |\n|---|---|---|---|---|\n");
        for q in &snap.sequences {
            let _ = writeln!(
                out,
                "| `{}.{}` | {} | {} | {} | {} |",
                q.schema, q.name, q.type_name, q.start, q.increment, q.current
            );
        }
        out.push('\n');
    }

    h2!("生成 SQL 的注意事项");
    out.push_str(&sql_notes(snap));
    out
}

// ---------------------------------------------------------------- 渲染：DDL

fn col_ddl(c: &ColumnSchema) -> String {
    let mut line = format!("    [{}] {}", c.name.replace(']', "]]"), c.sql_type());
    if let Some(coll) = &c.collation {
        if c.type_name.starts_with('n')
            || c.type_name.starts_with('v')
            || c.type_name.starts_with('c')
        {
            let _ = write!(line, " COLLATE {}", coll);
        }
    }
    if c.identity {
        line.push_str(" IDENTITY(1,1)");
    }
    if let Some(expr) = &c.computed {
        let _ = write!(line, " AS {}", expr);
    }
    line.push_str(if c.nullable { " NULL" } else { " NOT NULL" });
    if let Some(d) = &c.default {
        let _ = write!(line, " CONSTRAINT [DF_{}] DEFAULT {}", c.name, d);
    }
    line
}

/// 生成可执行 T-SQL DDL
pub fn render_ddl(snap: &SchemaSnapshot, settings: &SchemaSettings) -> String {
    let mut out = String::new();
    out.push_str("-- 由 MSSQL Transfer Studio 生成\n");
    let _ = writeln!(out, "-- 数据库：{}", snap.db.name);
    out.push_str("SET ANSI_NULLS ON;\nSET QUOTED_IDENTIFIER ON;\nGO\n\n");
    out.push_str(&format!(
        "IF DB_ID(N'{}') IS NULL CREATE DATABASE [{}];\nGO\nUSE [{}];\nGO\n\n",
        snap.db.name.replace('\'', "''"),
        snap.db.name.replace(']', "]]"),
        snap.db.name.replace(']', "]]")
    ));

    let mut schemas: BTreeSet<String> = BTreeSet::new();
    for t in snap.tables.iter().chain(snap.views.iter()) {
        if t.schema != "dbo" {
            schemas.insert(t.schema.clone());
        }
    }
    for s in &schemas {
        let _ = writeln!(
            out,
            "IF NOT EXISTS (SELECT 1 FROM sys.schemas WHERE name = N'{}') EXEC(N'CREATE SCHEMA [{}]');\nGO",
            s.replace('\'', "''"),
            s.replace(']', "]]")
        );
    }
    if !schemas.is_empty() {
        out.push('\n');
    }

    for t in &snap.tables {
        let _ = writeln!(
            out,
            "IF OBJECT_ID(N'{}.{}', N'U') IS NOT NULL DROP TABLE {};\nGO",
            t.schema.replace('\'', "''"),
            t.name.replace('\'', "''"),
            t.qualified()
        );
        let _ = writeln!(out, "CREATE TABLE {} (", t.qualified());
        let mut lines: Vec<String> = t.columns.iter().map(col_ddl).collect();
        if settings.include_pk && !t.primary_key.is_empty() {
            let cols = t
                .primary_key
                .iter()
                .map(|c| format!("[{}]", c))
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(format!(
                "    CONSTRAINT [PK_{}] PRIMARY KEY CLUSTERED ({})",
                t.name.replace(']', "]]"),
                cols
            ));
        }
        if settings.include_checks {
            for c in &t.checks {
                lines.push(format!(
                    "    CONSTRAINT [{}] CHECK {}",
                    c.name.replace(']', "]]"),
                    c.definition
                ));
            }
        }
        out.push_str(&lines.join(",\n"));
        out.push_str("\n);\nGO\n\n");
    }

    // 索引
    if settings.include_indexes {
        for t in &snap.tables {
            for i in &t.indexes {
                if i.is_primary_key || i.is_unique_constraint {
                    continue;
                }
                let uniq = if i.is_unique { "UNIQUE " } else { "" };
                let inc = if i.includes.is_empty() {
                    String::new()
                } else {
                    format!(
                        " INCLUDE ({})",
                        i.includes
                            .iter()
                            .map(|c| format!("[{}]", c))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                let _ = writeln!(
                    out,
                    "CREATE {}{} INDEX [{}] ON {} ({}){};\nGO",
                    uniq,
                    if i.kind.contains("NONCLUSTERED") {
                        "NONCLUSTERED"
                    } else {
                        "CLUSTERED"
                    },
                    i.name,
                    t.qualified(),
                    i.columns
                        .iter()
                        .map(|c| {
                            if let Some(base) = c.strip_suffix(" DESC") {
                                format!("[{}] DESC", base)
                            } else {
                                format!("[{}]", c)
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(", "),
                    inc
                );
            }
        }
        out.push('\n');
    }

    // 外键
    if settings.include_fk {
        for t in &snap.tables {
            for fk in &t.foreign_keys {
                let on_del = if fk.on_delete != "NO_ACTION" {
                    format!(" ON DELETE {}", fk.on_delete.replace('_', " "))
                } else {
                    String::new()
                };
                let on_upd = if fk.on_update != "NO_ACTION" {
                    format!(" ON UPDATE {}", fk.on_update.replace('_', " "))
                } else {
                    String::new()
                };
                let _ = writeln!(
                    out,
                    "ALTER TABLE {} ADD CONSTRAINT [{}] FOREIGN KEY ({}) REFERENCES [{}].[{}] ({}){}{};\nGO",
                    t.qualified(),
                    fk.name.replace(']', "]]"),
                    fk.columns.iter().map(|c| format!("[{}]", c)).collect::<Vec<_>>().join(", "),
                    fk.ref_schema,
                    fk.ref_table,
                    fk.ref_columns.iter().map(|c| format!("[{}]", c)).collect::<Vec<_>>().join(", "),
                    on_del,
                    on_upd
                );
            }
        }
        out.push('\n');
    }

    // 描述（扩展属性）
    if settings.include_descriptions {
        for t in &snap.tables {
            if let Some(d) = &t.description {
                let _ = writeln!(
                    out,
                    "EXEC sp_addextendedproperty N'MS_Description', N'{}', N'SCHEMA', N'{}', N'TABLE', N'{}', NULL, NULL;\nGO",
                    d.replace('\'', "''"),
                    t.schema.replace('\'', "''"),
                    t.name.replace('\'', "''")
                );
            }
            for c in &t.columns {
                if let Some(d) = &c.description {
                    let _ = writeln!(
                        out,
                        "EXEC sp_addextendedproperty N'MS_Description', N'{}', N'SCHEMA', N'{}', N'TABLE', N'{}', N'COLUMN', N'{}';\nGO",
                        d.replace('\'', "''"),
                        t.schema.replace('\'', "''"),
                        t.name.replace('\'', "''"),
                        c.name.replace('\'', "''")
                    );
                }
            }
        }
        out.push('\n');
    }

    // 视图与过程
    for m in &snap.modules {
        let _ = writeln!(out, "-- {} {}.{}", m.kind, m.schema, m.name);
        out.push_str(m.definition.trim());
        out.push_str("\nGO\n\n");
    }

    // 序列
    for q in &snap.sequences {
        let _ = writeln!(
            out,
            "IF NOT EXISTS (SELECT 1 FROM sys.sequences WHERE name = N'{}')\n    CREATE SEQUENCE [{}].[{}] AS {} START WITH {} INCREMENT BY {};\nGO",
            q.name.replace('\'', "''"),
            q.schema,
            q.name,
            q.type_name,
            q.start,
            q.increment
        );
    }
    out
}

// ---------------------------------------------------------------- 渲染：Mermaid

fn mermaid_id(schema: &str, name: &str) -> String {
    let mut s = String::new();
    for c in format!("{}_{}", schema, name).chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            s.push(c);
        } else {
            s.push('_');
        }
    }
    s
}

pub fn render_mermaid(snap: &SchemaSnapshot, _settings: &SchemaSettings) -> String {
    let _ = _settings;
    let mut out = String::from("erDiagram\n");
    for t in &snap.tables {
        let id = mermaid_id(&t.schema, &t.name);
        let _ = writeln!(out, "    {} {{", id);
        for c in &t.columns {
            let mut flags: Vec<&str> = Vec::new();
            if _settings.include_pk
                && t.primary_key
                    .iter()
                    .any(|p| p.eq_ignore_ascii_case(&c.name))
            {
                flags.push("PK");
            }
            if t.foreign_keys
                .iter()
                .any(|f| f.columns.iter().any(|x| x.eq_ignore_ascii_case(&c.name)))
            {
                flags.push("FK");
            }
            let base = c
                .type_name
                .chars()
                .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
                .collect::<String>();
            let _ = writeln!(
                out,
                "        {} {}{}",
                base,
                c.name.replace([' ', '-'], "_"),
                if flags.is_empty() {
                    String::new()
                } else {
                    format!(" {}", flags.join(","))
                }
            );
        }
        let _ = writeln!(out, "    }}");
    }

    for t in &snap.tables {
        for fk in &t.foreign_keys {
            let child = mermaid_id(&t.schema, &t.name);
            let parent = mermaid_id(&fk.ref_schema, &fk.ref_table);
            let nullable = t
                .columns
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(&fk.columns[0]))
                .map(|c| c.nullable)
                .unwrap_or(true);
            let card = if nullable { "|o--o{" } else { "||--o{" };
            let _ = writeln!(
                out,
                "    {} {} {} : \"{}\"",
                parent,
                card,
                child,
                fk.columns.join(",")
            );
        }
    }
    out
}

// ---------------------------------------------------------------- 渲染：JSON

fn col_json(c: &ColumnSchema) -> Value {
    json!({
        "name": c.name,
        "type": c.sql_type(),
        "baseType": c.type_name,
        "nullable": c.nullable,
        "identity": c.identity,
        "computed": c.computed,
        "default": c.default,
        "collation": c.collation,
        "description": c.description,
        "kind": type_kind(&c.type_name),
    })
}

pub fn render_json(snap: &SchemaSnapshot, _settings: &SchemaSettings) -> String {
    let tables: Vec<Value> = snap
        .tables
        .iter()
        .map(|t| {
            json!({
                "schema": t.schema,
                "name": t.name,
                "description": t.description,
                "rowCount": t.row_count,
                "primaryKey": t.primary_key,
                "columns": t.columns.iter().map(col_json).collect::<Vec<_>>(),
                "indexes": t.indexes.iter().map(|i| json!({
                    "name": i.name,
                    "unique": i.is_unique,
                    "primaryKey": i.is_primary_key,
                    "columns": i.columns,
                    "includes": i.includes,
                })).collect::<Vec<_>>(),
                "foreignKeys": t.foreign_keys.iter().map(|f| json!({
                    "name": f.name,
                    "columns": f.columns,
                    "references": {
                        "schema": f.ref_schema,
                        "table": f.ref_table,
                        "columns": f.ref_columns
                    },
                    "onDelete": f.on_delete,
                    "onUpdate": f.on_update,
                })).collect::<Vec<_>>(),
                "checks": t.checks.iter().map(|c| json!({
                    "name": c.name, "definition": c.definition
                })).collect::<Vec<_>>(),
                "samples": t.samples,
            })
        })
        .collect();

    let views: Vec<Value> = snap
        .views
        .iter()
        .map(|v| {
            json!({
                "schema": v.schema,
                "name": v.name,
                "description": v.description,
                "columns": v.columns.iter().map(col_json).collect::<Vec<_>>(),
            })
        })
        .collect();

    let v = json!({
        "generatedBy": "MSSQL Transfer Studio",
        "database": {
            "name": snap.db.name,
            "version": snap.db.version,
            "edition": snap.db.edition,
            "collation": snap.db.collation,
            "compatibilityLevel": snap.db.compat_level,
            "recoveryModel": snap.db.recovery_model,
        },
        "tables": tables,
        "views": views,
        "modules": snap.modules.iter().map(|m| json!({
            "schema": m.schema, "name": m.name, "kind": m.kind, "definition": m.definition
        })).collect::<Vec<_>>(),
        "sequences": snap.sequences.iter().map(|q| json!({
            "schema": q.schema, "name": q.name, "type": q.type_name,
            "start": q.start, "increment": q.increment, "current": q.current
        })).collect::<Vec<_>>(),
    });
    serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".into())
}

// ---------------------------------------------------------------- 拆文件

fn file_stem(settings: &SchemaSettings, fallback: &str) -> String {
    let n = settings.file_name.trim();
    if n.is_empty() {
        fallback.to_string()
    } else {
        n.to_string()
    }
}

fn write_file(dir: &std::path::Path, name: &str, body: &str, ctx: &TaskCtx) -> Result<PathBuf> {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("无法创建目录 {}", parent.display()))?;
    }
    std::fs::write(&path, body).with_context(|| format!("无法写入 {}", path.display()))?;
    ctx.log(format!(
        "  → {} ({})",
        path.display(),
        crate::db::export::human_bytes(body.len() as u64)
    ));
    Ok(path)
}

// ---------------------------------------------------------------- 主流程

pub async fn run(
    client: &mut SqlClient,
    settings: &SchemaSettings,
    selected: &[TableRef],
    ctx: &TaskCtx,
) -> Result<Summary> {
    std::fs::create_dir_all(&settings.dir)
        .with_context(|| format!("无法创建目录 {}", settings.dir.display()))?;

    ctx.log(format!(
        "▶ 采集结构：{}",
        if settings.use_selection && !selected.is_empty() {
            format!("已选 {} 个对象", selected.len())
        } else {
            "整库".to_string()
        }
    ));

    let snap = collect(client, settings, selected, ctx).await?;
    if snap.tables.is_empty() && snap.views.is_empty() {
        return Ok(Summary {
            ok: false,
            message: "没有匹配到任何对象".into(),
        });
    }

    let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
    let db = &snap.db.name;
    let mut files: Vec<PathBuf> = Vec::new();

    match settings.format {
        SchemaFormat::AiContext if settings.split => {
            // 多文件上下文包
            let mut index = String::new();
            let _ = writeln!(index, "# 数据库结构上下文：{}\n", db);
            if !settings.prompt_header.trim().is_empty() {
                let _ = writeln!(index, "{}\n---\n", settings.prompt_header.trim());
            }
            index.push_str("> 本目录由 MSSQL Transfer Studio 生成，可整体提供给 AI / Agent。\n\n");
            let _ = writeln!(index, "## 文件索引\n");
            let _ = writeln!(index, "| 文件 | 内容 |");
            let _ = writeln!(index, "|---|---|");
            let _ = writeln!(
                index,
                "| `00-overview.md` | 数据库概览、对象清单、关系、写 SQL 注意事项 |"
            );
            let _ = writeln!(index, "| `tables/*.md` | 每张表的完整结构（含样例数据） |");
            if settings.include_views {
                let _ = writeln!(index, "| `views/*.md` | 每个视图的列结构 |");
            }
            let _ = writeln!(index, "| `schema.sql` | 可执行建表脚本 |");
            let _ = writeln!(index, "| `er.mmd` | Mermaid ER 图 |");
            let _ = writeln!(index, "| `schema.json` | 结构化元数据 |");
            let _ = writeln!(index, "\n## 概览\n");
            index.push_str(&overview_block(&snap));
            let _ = writeln!(index, "\n## 对象清单\n");
            index.push_str(&object_list_table(&snap));
            let _ = writeln!(index, "\n## 关系（外键）\n");
            index.push_str(&relations_section(&snap));
            let _ = writeln!(index, "\n## 生成 SQL 的注意事项\n");
            index.push_str(&sql_notes(&snap));

            files.push(write_file(
                &settings.dir,
                &file_stem(settings, "00-overview.md"),
                &index,
                ctx,
            )?);

            let mut n = 0;
            let total = snap.tables.len() + snap.views.len();
            for t in snap.tables.iter() {
                n += 1;
                let body = table_detail(t, settings, "##");
                let name = format!(
                    "tables/{:03}-{}.md",
                    n,
                    t.display()
                        .replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|', ' '], "_")
                );
                files.push(write_file(&settings.dir, &name, &body, ctx)?);
                ctx.progress(
                    0.5 + 0.4 * (n as f32 / total.max(1) as f32),
                    "写出分表文档…",
                    0,
                    0,
                );
            }
            if settings.include_views {
                for v in snap.views.iter() {
                    n += 1;
                    let body = table_detail(v, settings, "##");
                    let name = format!(
                        "views/{:03}-{}.md",
                        n,
                        v.display()
                            .replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|', ' '], "_")
                    );
                    files.push(write_file(&settings.dir, &name, &body, ctx)?);
                }
            }

            if settings.include_indexes || settings.include_fk {
                let ddl = render_ddl(&snap, settings);
                files.push(write_file(&settings.dir, "schema.sql", &ddl, ctx)?);
            }
            let mmd = render_mermaid(&snap, settings);
            files.push(write_file(&settings.dir, "er.mmd", &mmd, ctx)?);
            let js = render_json(&snap, settings);
            files.push(write_file(&settings.dir, "schema.json", &js, ctx)?);
        }
        SchemaFormat::AiContext | SchemaFormat::Markdown => {
            let body = if settings.format == SchemaFormat::AiContext {
                render_ai_context(&snap, settings)
            } else {
                render_markdown(&snap, settings)
            };
            let name = file_stem(settings, &format!("{}_schema_{}.md", sanitize(db), stamp));
            files.push(write_file(&settings.dir, &name, &body, ctx)?);
        }
        SchemaFormat::Ddl => {
            let body = render_ddl(&snap, settings);
            let name = file_stem(settings, &format!("{}_ddl_{}.sql", sanitize(db), stamp));
            files.push(write_file(&settings.dir, &name, &body, ctx)?);
        }
        SchemaFormat::Json => {
            let body = render_json(&snap, settings);
            let name = file_stem(settings, &format!("{}_schema_{}.json", sanitize(db), stamp));
            files.push(write_file(&settings.dir, &name, &body, ctx)?);
        }
        SchemaFormat::Mermaid => {
            let body = render_mermaid(&snap, settings);
            let name = file_stem(settings, &format!("{}_er_{}.mmd", sanitize(db), stamp));
            files.push(write_file(&settings.dir, &name, &body, ctx)?);
        }
    }

    ctx.progress(1.0, "结构导出完成", 0, 0);

    let bytes: usize = files
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok().map(|m| m.len() as usize))
        .sum();

    Ok(Summary {
        ok: true,
        message: format!(
            "结构导出完成：{} 个对象，{} 个文件，共 {}",
            snap.tables.len() + snap.views.len(),
            files.len(),
            crate::db::export::human_bytes(bytes as u64)
        ),
    })
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ---------------------------------------------------------------- 预览

/// 生成一份供界面预览的文档（不落盘）
pub async fn preview(
    client: &mut SqlClient,
    settings: &SchemaSettings,
    selected: &[TableRef],
    limit_lines: usize,
) -> Result<String> {
    let mut quiet = settings.clone();
    quiet.sample_rows = quiet.sample_rows.min(3);
    let ctx = TaskCtx {
        cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        emit: std::sync::Arc::new(|_| {}),
    };
    let snap = collect(client, &quiet, selected, &ctx).await?;
    let body = match settings.format {
        SchemaFormat::Ddl => render_ddl(&snap, &quiet),
        SchemaFormat::Json => render_json(&snap, &quiet),
        SchemaFormat::Mermaid => render_mermaid(&snap, &quiet),
        SchemaFormat::Markdown => render_markdown(&snap, &quiet),
        SchemaFormat::AiContext => render_ai_context(&snap, &quiet),
    };
    let total = body.lines().count();
    let mut out: String = body
        .lines()
        .take(limit_lines)
        .collect::<Vec<_>>()
        .join("\n");
    if total > limit_lines {
        let _ = write!(out, "\n\n…（共 {} 行，仅预览前 {} 行）", total, limit_lines);
    }
    Ok(out)
}

// ---------------------------------------------------------------- 内置示例

/// 内置示例快照（SalesDB：客户 / 订单，含主键、外键级联、检查约束、注释）。
///
/// 不连数据库也能预览 5 种输出格式，命令行 `--sample-schema <目录>` 用它生成样例文件。
pub fn demo_snapshot() -> SchemaSnapshot {
    let customers = TableSchema {
        schema: "dbo".into(),
        name: "Customers".into(),
        is_view: false,
        row_count: 12480,
        description: Some("客户主表".into()),
        columns: vec![
            ColumnSchema {
                name: "Id".into(),
                type_name: "int".into(),
                max_length: 4,
                precision: 10,
                scale: 0,
                nullable: false,
                identity: true,
                computed: None,
                default: None,
                collation: None,
                description: Some("主键".into()),
            },
            ColumnSchema {
                name: "Name".into(),
                type_name: "nvarchar".into(),
                max_length: 100,
                precision: 0,
                scale: 0,
                nullable: false,
                identity: false,
                computed: None,
                default: None,
                collation: Some("Chinese_PRC_CI_AS".into()),
                description: None,
            },
            ColumnSchema {
                name: "Balance".into(),
                type_name: "decimal".into(),
                max_length: 9,
                precision: 18,
                scale: 2,
                nullable: true,
                identity: false,
                computed: None,
                default: Some("((0))".into()),
                collation: None,
                description: None,
            },
        ],
        primary_key: vec!["Id".into()],
        indexes: vec![IndexSchema {
            name: "IX_Customers_Name".into(),
            is_unique: false,
            is_primary_key: false,
            is_unique_constraint: false,
            kind: "NONCLUSTERED".into(),
            columns: vec!["Name".into()],
            includes: vec![],
        }],
        foreign_keys: vec![],
        checks: vec![],
        samples: vec![vec![
            Some("1".into()),
            Some("张伟".into()),
            Some("0.00".into()),
        ]],
        sample_headers: vec!["Id".into(), "Name".into(), "Balance".into()],
    };

    let orders = TableSchema {
        schema: "dbo".into(),
        name: "Orders".into(),
        is_view: false,
        row_count: 89301,
        description: None,
        columns: vec![
            ColumnSchema {
                name: "Id".into(),
                type_name: "bigint".into(),
                max_length: 8,
                precision: 19,
                scale: 0,
                nullable: false,
                identity: true,
                computed: None,
                default: None,
                collation: None,
                description: None,
            },
            ColumnSchema {
                name: "CustomerId".into(),
                type_name: "int".into(),
                max_length: 4,
                precision: 10,
                scale: 0,
                nullable: false,
                identity: false,
                computed: None,
                default: None,
                collation: None,
                description: None,
            },
        ],
        primary_key: vec!["Id".into()],
        indexes: vec![],
        foreign_keys: vec![ForeignKey {
            name: "FK_Orders_Customers".into(),
            columns: vec!["CustomerId".into()],
            ref_schema: "dbo".into(),
            ref_table: "Customers".into(),
            ref_columns: vec!["Id".into()],
            on_delete: "CASCADE".into(),
            on_update: "NO_ACTION".into(),
        }],
        checks: vec![CheckConstraint {
            name: "CK_Orders_CustomerId".into(),
            definition: "([CustomerId]>(0))".into(),
        }],
        samples: vec![],
        sample_headers: vec![],
    };

    SchemaSnapshot {
        db: DbInfo {
            name: "SalesDB".into(),
            version: "16.0.4135".into(),
            edition: "Enterprise".into(),
            collation: "Chinese_PRC_CI_AS".into(),
            compat_level: 160,
            recovery_model: "FULL".into(),
        },
        tables: vec![customers, orders],
        views: vec![],
        modules: vec![],
        sequences: vec![],
    }
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_snapshot() -> SchemaSnapshot {
        demo_snapshot()
    }

    #[test]
    fn column_types_render_like_tsql() {
        let snap = sample_snapshot();
        let name = &snap.tables[0].columns[1];
        assert_eq!(name.sql_type(), "nvarchar(50)"); // max_length 100 字节 = 50 字符
        let bal = &snap.tables[0].columns[2];
        assert_eq!(bal.sql_type(), "decimal(18,2)");
    }

    #[test]
    fn ai_context_contains_key_sections() {
        let snap = sample_snapshot();
        let md = render_ai_context(&snap, &SchemaSettings::default());
        for needle in [
            "数据库结构上下文",
            "SalesDB",
            "对象清单",
            "关系（外键）",
            "表结构详解",
            "生成 SQL 的注意事项",
            "dbo.Customers",
            "dbo.Orders",
            "FK_Orders_Customers",
            "CASCADE",
        ] {
            assert!(md.contains(needle), "缺少内容：{}\n{}", needle, md);
        }
        // 自动推导的注意事项
        assert!(md.contains("IDENTITY"), "应提示 IDENTITY 列");
        assert!(md.contains("decimal(18,2)"), "应提示精确小数列");
    }

    #[test]
    fn ddl_is_executable_looking() {
        let snap = sample_snapshot();
        let ddl = render_ddl(&snap, &SchemaSettings::default());
        assert!(ddl.contains("CREATE TABLE [dbo].[Customers]"));
        assert!(ddl.contains("[Id] int IDENTITY(1,1) NOT NULL"));
        assert!(ddl.contains("PRIMARY KEY CLUSTERED ([Id])"));
        assert!(ddl.contains("CREATE NONCLUSTERED INDEX [IX_Customers_Name]"));
        assert!(ddl.contains("ALTER TABLE [dbo].[Orders] ADD CONSTRAINT [FK_Orders_Customers]"));
        assert!(ddl.contains("ON DELETE CASCADE"));
        assert!(ddl.contains("CONSTRAINT [CK_Orders_CustomerId] CHECK"));
        assert!(ddl.contains("sp_addextendedproperty"));
    }

    #[test]
    fn mermaid_has_entities_and_relation() {
        let snap = sample_snapshot();
        let m = render_mermaid(&snap, &SchemaSettings::default());
        assert!(m.starts_with("erDiagram"));
        assert!(m.contains("dbo_Customers {"));
        assert!(m.contains("int Id PK"));
        assert!(m.contains("dbo_Customers ||--o{ dbo_Orders"));
    }

    #[test]
    fn json_is_valid_and_complete() {
        let snap = sample_snapshot();
        let js = render_json(&snap, &SchemaSettings::default());
        let v: Value = serde_json::from_str(&js).expect("必须是合法 JSON");
        assert_eq!(v["database"]["name"], "SalesDB");
        assert_eq!(v["tables"].as_array().unwrap().len(), 2);
        assert_eq!(v["tables"][0]["columns"][1]["type"], "nvarchar(50)");
        assert_eq!(
            v["tables"][1]["foreignKeys"][0]["references"]["table"],
            "Customers"
        );
    }

    #[test]
    fn prompt_header_is_prepended() {
        let snap = sample_snapshot();
        let st = SchemaSettings {
            prompt_header: "你是 T-SQL 专家，请只输出 SQL。".into(),
            ..Default::default()
        };
        let md = render_ai_context(&snap, &st);
        assert!(md.contains("你是 T-SQL 专家，请只输出 SQL。"));
        // 出现在标题之后、正文之前
        let pos_head = md.find("你是 T-SQL").unwrap();
        let pos_body = md.find("## 1. 数据库概览").unwrap();
        assert!(pos_head < pos_body);
    }

    #[test]
    fn sample_values_are_clipped() {
        assert_eq!(clip("abcdef", 3), "abc…");
        assert_eq!(clip("abc", 3), "abc");
        assert_eq!(clip("abcdef", 0), "abcdef");
    }
}
