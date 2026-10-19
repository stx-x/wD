//! 导出引擎：流式读取 SQL Server 数据并写入多种格式。

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use flate2::write::GzEncoder;
use flate2::Compression;
use futures_util::TryStreamExt;
use serde_json::Value;

use crate::db::conn::SqlClient;
use crate::db::meta;
use crate::db::value::{cell_to_json, cell_to_sql_literal, row_to_cells};
use crate::model::{
    Cell, ColumnMeta, ExportFormat, ExportSettings, FileScope, Summary, TableRef, UiEvent,
};

pub struct TaskCtx {
    pub cancel: Arc<AtomicBool>,
    pub emit: Arc<dyn Fn(UiEvent) + Send + Sync>,
}

impl TaskCtx {
    pub fn log(&self, msg: impl Into<String>) {
        (self.emit)(UiEvent::Log(msg.into()));
    }
    pub fn progress(&self, frac: f32, text: impl Into<String>, rows: u64, bytes: u64) {
        (self.emit)(UiEvent::Progress {
            frac: frac.clamp(0.0, 1.0),
            text: text.into(),
            rows,
            bytes,
        });
    }
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

struct CountingWriter {
    inner: Box<dyn Write>,
    count: Arc<AtomicU64>,
}

impl Write for CountingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.count.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[allow(clippy::large_enum_variant)]
enum Sink {
    Csv {
        wtr: csv::Writer<CountingWriter>,
        header_written: bool,
    },
    Json {
        w: CountingWriter,
        map_mode: bool,
        first_table: bool,
        first_row: bool,
        table_name: String,
        include_table_field: bool,
    },
    Ndjson {
        w: CountingWriter,
        include_table_field: bool,
        table_name: String,
    },
    Sql {
        w: CountingWriter,
        batch: usize,
        pending: Vec<String>,
        prefix: String,
        identity_open: bool,
    },
}

pub struct Exporter {
    sink: Sink,
    settings: ExportSettings,
    bytes: Arc<AtomicU64>,
}

impl Exporter {
    fn open(path: &Path, settings: &ExportSettings, bytes: Arc<AtomicU64>) -> Result<Self> {
        let file =
            File::create(path).with_context(|| format!("无法创建文件 {}", path.display()))?;
        let buf = BufWriter::with_capacity(512 * 1024, file);
        // gzip 流在 Drop 时自动写入尾部校验，无需显式 finish
        let mut inner: Box<dyn Write> = if settings.gzip && !settings.format.is_excel() {
            Box::new(GzEncoder::new(buf, Compression::default()))
        } else {
            Box::new(buf)
        };
        if settings.bom && !settings.format.is_sql() {
            inner.write_all(&[0xEF, 0xBB, 0xBF])?;
        }
        let w = CountingWriter {
            inner,
            count: bytes.clone(),
        };

        let sink = match settings.format {
            ExportFormat::Csv | ExportFormat::Tsv => {
                let delim = if settings.format == ExportFormat::Tsv {
                    b'\t'
                } else {
                    settings.delimiter
                };
                let wtr = csv::WriterBuilder::new()
                    .delimiter(delim)
                    .quote_style(if settings.quote_all {
                        csv::QuoteStyle::Always
                    } else {
                        csv::QuoteStyle::Necessary
                    })
                    .from_writer(w);
                Sink::Csv {
                    wtr,
                    header_written: false,
                }
            }
            ExportFormat::JsonArray => Sink::Json {
                w,
                map_mode: settings.scope == FileScope::SingleFile,
                first_table: true,
                first_row: true,
                table_name: String::new(),
                include_table_field: false,
            },
            ExportFormat::Ndjson => Sink::Ndjson {
                w,
                include_table_field: settings.scope == FileScope::SingleFile,
                table_name: String::new(),
            },
            ExportFormat::SqlInsert | ExportFormat::SqlFull => Sink::Sql {
                w,
                batch: settings.batch.max(1),
                pending: Vec::with_capacity(settings.batch.max(1)),
                prefix: String::new(),
                identity_open: false,
            },
            // Excel 由 xlsx_out 引擎处理，不会走到这里
            ExportFormat::Excel => return Err(anyhow::anyhow!("Excel 导出请使用 xlsx 引擎")),
        };

        Ok(Self {
            sink,
            settings: settings.clone(),
            bytes,
        })
    }

    fn begin_table(&mut self, table: &TableRef, cols: &[ColumnMeta]) -> Result<()> {
        match &mut self.sink {
            Sink::Csv {
                wtr,
                header_written,
            } => {
                let single = self.settings.scope == FileScope::SingleFile;
                let mut headers: Vec<String> = Vec::new();
                if single {
                    headers.push("__table".to_string());
                }
                headers.extend(cols.iter().map(|c| c.name.clone()));
                if self.settings.header && (!single || !*header_written) {
                    wtr.write_record(&headers)?;
                    *header_written = true;
                }
            }
            Sink::Json {
                w,
                map_mode,
                first_table,
                first_row,
                table_name,
                include_table_field,
            } => {
                *table_name = table.display();
                *first_row = true;
                *include_table_field = false;
                if *map_mode {
                    if *first_table {
                        w.write_all(b"{\n")?;
                        *first_table = false;
                    } else {
                        w.write_all(b",\n")?;
                    }
                    w.write_all(
                        format!("\n  \"{}\": [", json_escape(&table.display())).as_bytes(),
                    )?;
                } else {
                    w.write_all(b"[\n")?;
                }
            }
            Sink::Ndjson {
                include_table_field,
                table_name,
                ..
            } => {
                *table_name = table.display();
                *include_table_field = self.settings.scope == FileScope::SingleFile;
            }
            Sink::Sql {
                w,
                pending,
                prefix,
                identity_open,
                ..
            } => {
                pending.clear();
                *prefix = format!(
                    "INSERT INTO {} ({}) VALUES\n",
                    table.qualified(),
                    cols.iter()
                        .map(|c| format!("[{}]", c.name))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                if self.settings.identity_insert && cols.iter().any(|c| c.is_identity) {
                    w.write_all(
                        format!("SET IDENTITY_INSERT {} ON;\n", table.qualified()).as_bytes(),
                    )?;
                    *identity_open = true;
                }
            }
        }
        Ok(())
    }

    fn write_row(&mut self, table: &TableRef, cols: &[ColumnMeta], cells: &[Cell]) -> Result<()> {
        match &mut self.sink {
            Sink::Csv { wtr, .. } => {
                let mut rec: Vec<String> = Vec::with_capacity(cells.len() + 1);
                if self.settings.scope == FileScope::SingleFile {
                    rec.push(table.display());
                }
                for c in cells {
                    rec.push(c.as_display(&self.settings.null_text));
                }
                wtr.write_record(&rec)?;
            }
            Sink::Json {
                w,
                first_row,
                table_name,
                include_table_field,
                ..
            } => {
                if !*first_row {
                    w.write_all(b",\n")?;
                }
                *first_row = false;
                let mut map = serde_json::Map::with_capacity(cols.len() + 1);
                if *include_table_field {
                    map.insert("__table".into(), Value::String(table_name.clone()));
                }
                for (i, cell) in cells.iter().enumerate() {
                    let key = cols
                        .get(i)
                        .map(|c| c.name.clone())
                        .unwrap_or_else(|| format!("col{}", i + 1));
                    let val = cell_to_json(cell, cols.get(i));
                    map.insert(key, val);
                }
                w.write_all(b"    ")?;
                serde_json::to_writer(&mut *w, &Value::Object(map))?;
            }
            Sink::Ndjson {
                w,
                include_table_field,
                table_name,
            } => {
                let mut map = serde_json::Map::with_capacity(cols.len() + 1);
                if *include_table_field {
                    map.insert("__table".into(), Value::String(table_name.clone()));
                }
                for (i, cell) in cells.iter().enumerate() {
                    let key = cols
                        .get(i)
                        .map(|c| c.name.clone())
                        .unwrap_or_else(|| format!("col{}", i + 1));
                    map.insert(key, cell_to_json(cell, cols.get(i)));
                }
                serde_json::to_writer(&mut *w, &Value::Object(map))?;
                w.write_all(b"\n")?;
            }
            Sink::Sql {
                w,
                batch,
                pending,
                prefix,
                ..
            } => {
                let vals: Vec<String> = cells
                    .iter()
                    .enumerate()
                    .map(|(i, c)| cell_to_sql_literal(c, cols.get(i)))
                    .collect();
                pending.push(format!("({})", vals.join(", ")));
                if pending.len() >= *batch {
                    w.write_all(prefix.as_bytes())?;
                    w.write_all(pending.join(",\n").as_bytes())?;
                    w.write_all(b";\n")?;
                    pending.clear();
                }
            }
        }
        Ok(())
    }

    fn end_table(&mut self, table: &TableRef, cols: &[ColumnMeta], rows: u64) -> Result<()> {
        match &mut self.sink {
            Sink::Csv { wtr, .. } => {
                wtr.flush()?;
            }
            Sink::Json { w, map_mode, .. } => {
                if *map_mode {
                    w.write_all(b"\n  ]")?;
                }
                let _ = (table, cols, rows);
            }
            Sink::Ndjson { w, .. } => {
                w.flush()?;
            }
            Sink::Sql {
                w,
                pending,
                prefix,
                identity_open,
                ..
            } => {
                if !pending.is_empty() {
                    w.write_all(prefix.as_bytes())?;
                    w.write_all(pending.join(",\n").as_bytes())?;
                    w.write_all(b";\n")?;
                    pending.clear();
                }
                if *identity_open {
                    w.write_all(
                        format!("SET IDENTITY_INSERT {} OFF;\n", table.qualified()).as_bytes(),
                    )?;
                    *identity_open = false;
                }
            }
        }
        Ok(())
    }

    fn finish(&mut self, single: bool) -> Result<()> {
        match &mut self.sink {
            Sink::Csv { wtr, .. } => {
                wtr.flush()?;
            }
            Sink::Json { w, map_mode, .. } => {
                if *map_mode {
                    w.write_all(b"\n}\n")?;
                } else {
                    w.write_all(b"\n]\n")?;
                }
                w.flush()?;
            }
            Sink::Ndjson { w, .. } => {
                w.flush()?;
            }
            Sink::Sql { w, .. } => {
                w.write_all(b"\n")?;
                w.flush()?;
            }
        }
        let _ = single;
        Ok(())
    }
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

fn safe_name(s: &str) -> String {
    s.chars()
        .map(|c| {
            if "/\\:*?\"<>|".contains(c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect()
}

fn file_stem(settings: &ExportSettings, table: &TableRef) -> String {
    let base = if settings.schema_qualified {
        table.display()
    } else {
        table.name.clone()
    };
    format!("{}{}", settings.file_prefix, safe_name(&base))
}

/// 人类可读的字节数
pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", b, UNITS[0])
    } else {
        format!("{:.1} {}", v, UNITS[i])
    }
}

/// 输出扩展名；开启 gzip 时追加 `.gz`（Excel 本身已压缩，不再叠加）
pub fn file_ext(settings: &ExportSettings) -> String {
    if settings.gzip && !settings.format.is_excel() {
        format!("{}.gz", settings.format.ext())
    } else {
        settings.format.ext().to_string()
    }
}

/// 解析 `exclude_columns` 为小写列名集合
pub fn excluded_set(settings: &ExportSettings) -> HashSet<String> {
    settings
        .exclude_columns
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

pub async fn run(
    client: &mut SqlClient,
    settings: &ExportSettings,
    tables: &[TableRef],
    db_name: &str,
    ctx: &TaskCtx,
) -> Result<Summary> {
    if tables.is_empty() {
        return Ok(Summary {
            ok: false,
            message: "没有选中任何对象".into(),
        });
    }
    std::fs::create_dir_all(&settings.dir)
        .with_context(|| format!("无法创建目录 {}", settings.dir.display()))?;

    let bytes = Arc::new(AtomicU64::new(0));
    let single = settings.scope == FileScope::SingleFile;
    let total = tables.len();

    let mut grand_rows: u64 = 0;
    let mut written_files: Vec<PathBuf> = Vec::new();
    let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();

    if single {
        let fname = format!(
            "{}export_{}.{}",
            settings.file_prefix,
            stamp,
            file_ext(settings)
        );
        let path = settings.dir.join(fname);
        if path.exists() {
            ctx.log(format!("⚠ 文件已存在，将被覆盖：{}", path.display()));
        }
        ctx.log(format!("→ 输出文件：{}", path.display()));

        let mut ex = Exporter::open(&path, settings, bytes.clone())?;
        if settings.format.is_sql() {
            write_sql_header(&mut ex, settings, db_name, tables)?;
        }

        for (idx, table) in tables.iter().enumerate() {
            if ctx.cancelled() {
                break;
            }
            let span = 1.0 / total as f32;
            let base = idx as f32 * span;
            let rows = export_one(client, table, settings, &mut ex, ctx, base, span).await?;
            grand_rows += rows;
        }
        ex.finish(single)?;
        written_files.push(path);
    } else {
        for (idx, table) in tables.iter().enumerate() {
            if ctx.cancelled() {
                break;
            }
            let fname = format!("{}.{}", file_stem(settings, table), file_ext(settings));
            let path = settings.dir.join(fname);
            if path.exists() {
                ctx.log(format!("⚠ 已存在，将覆盖：{}", path.display()));
            }
            ctx.log(format!("→ {}", path.display()));

            let mut ex = Exporter::open(&path, settings, bytes.clone())?;
            if settings.format.is_sql() {
                write_sql_header(&mut ex, settings, db_name, std::slice::from_ref(table))?;
            }
            let span = 1.0 / total as f32;
            let base = idx as f32 * span;
            let rows = export_one(client, table, settings, &mut ex, ctx, base, span).await?;
            ex.finish(single)?;
            grand_rows += rows;
            written_files.push(path);
        }
    }

    if ctx.cancelled() {
        return Ok(Summary {
            ok: false,
            message: format!(
                "已取消。已完成 {} 行，{} 个文件",
                grand_rows,
                written_files.len()
            ),
        });
    }

    ctx.progress(
        1.0,
        format!("导出完成：{} 个对象", written_files.len()),
        grand_rows,
        bytes.load(Ordering::Relaxed),
    );
    Ok(Summary {
        ok: true,
        message: format!(
            "导出完成：{} 个对象，{} 行数据，{} 个文件",
            tables.len(),
            grand_rows,
            written_files.len()
        ),
    })
}

fn write_sql_header(
    ex: &mut Exporter,
    settings: &ExportSettings,
    db_name: &str,
    tables: &[TableRef],
) -> Result<()> {
    if let Sink::Sql { w, .. } = &mut ex.sink {
        w.write_all(
            b"-- Generated by MSSQL Transfer Studio\nSET NOCOUNT ON;\nSET XACT_ABORT ON;\nGO\n",
        )?;
        if settings.create_database && !db_name.is_empty() {
            let esc = db_name.replace(']', "]]");
            w.write_all(
                format!(
                    "IF DB_ID(N'{db}') IS NULL CREATE DATABASE [{db2}];\nGO\nUSE [{db2}];\nGO\n",
                    db = db_name.replace('\'', "''"),
                    db2 = esc
                )
                .as_bytes(),
            )?;
        } else if !db_name.is_empty() {
            w.write_all(format!("USE [{}];\nGO\n", db_name.replace(']', "]]")).as_bytes())?;
        }
        let _ = tables;
    }
    Ok(())
}

async fn export_one(
    client: &mut SqlClient,
    table: &TableRef,
    settings: &ExportSettings,
    ex: &mut Exporter,
    ctx: &TaskCtx,
    base: f32,
    span: f32,
) -> Result<u64> {
    let mut cols = meta::columns(client, table).await?;
    let excluded = excluded_set(settings);
    if !excluded.is_empty() {
        let before = cols.len();
        cols.retain(|c| !excluded.contains(&c.name.to_ascii_lowercase()));
        let dropped = before - cols.len();
        if dropped > 0 {
            ctx.log(format!("  已排除 {} 个列", dropped));
        }
    }
    if cols.is_empty() {
        ctx.log(format!("⚠ {} 没有可读取的列，已跳过", table.display()));
        return Ok(0);
    }

    let want_data = settings.content.with_data();
    let is_view = table.kind == "view";
    let want_schema =
        settings.content.with_schema() && settings.format == ExportFormat::SqlFull && !is_view;
    if is_view && settings.content.with_schema() && settings.format == ExportFormat::SqlFull {
        ctx.log(format!(
            "  提示：{} 是视图，跳过 DDL 生成（仅导出数据）",
            table.display()
        ));
    }

    // 结构部分
    if want_schema {
        let pk = meta::primary_key(client, table).await.unwrap_or_default();
        let ddl = meta::create_table_sql(table, &cols, &pk, settings.drop_table);
        if let Sink::Sql { w, .. } = &mut ex.sink {
            w.write_all(b"\n")?;
            w.write_all(ddl.as_bytes())?;
            w.write_all(b"GO\n")?;
        }
    }

    if !want_data {
        ctx.log(format!("✓ {} 结构已导出", table.display()));
        return Ok(0);
    }

    ex.begin_table(table, &cols)?;

    let col_list = cols
        .iter()
        .map(|c| format!("[{}]", c.name))
        .collect::<Vec<_>>()
        .join(", ");
    let mut sql = if settings.limit_rows > 0 {
        format!(
            "SELECT TOP ({}) {} FROM {}",
            settings.limit_rows,
            col_list,
            table.qualified()
        )
    } else {
        format!("SELECT {} FROM {}", col_list, table.qualified())
    };
    let where_clause = settings.where_clause.trim();
    if !where_clause.is_empty() {
        let w = if where_clause.to_ascii_lowercase().starts_with("where") {
            where_clause[5..].trim()
        } else {
            where_clause
        };
        if !w.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(w);
        }
    }

    let est = estimate_rows(client, table).await;
    let mut rows: u64 = 0;
    let mut stream = client
        .query(sql, &[])
        .await
        .with_context(|| format!("查询 {} 失败", table.display()))?
        .into_row_stream();

    let mut since_tick: u64 = 0;
    while let Some(row) = stream.try_next().await? {
        let cells = row_to_cells(&row);
        ex.write_row(table, &cols, &cells)?;
        rows += 1;
        since_tick += 1;
        if since_tick >= 200 {
            since_tick = 0;
            if ctx.cancelled() {
                break;
            }
            let frac = if est > 0 {
                (rows as f32 / est as f32).min(0.999)
            } else {
                0.5
            };
            ctx.progress(
                base + span * frac,
                format!("导出 {} · {} 行", table.display(), rows),
                rows,
                ex.bytes.load(Ordering::Relaxed),
            );
        }
    }
    drop(stream);

    ex.end_table(table, &cols, rows)?;
    ctx.log(format!(
        "✓ {} · {} 行{}",
        table.display(),
        rows,
        if est > 0 {
            format!("（预估 {}）", est)
        } else {
            String::new()
        }
    ));
    Ok(rows)
}

pub(crate) async fn estimate_rows(client: &mut SqlClient, table: &TableRef) -> i64 {
    let q = "SELECT ISNULL(SUM(p.rows), 0) FROM sys.partitions p WHERE p.object_id = OBJECT_ID(@P1) AND p.index_id IN (0,1)";
    let qn = table.qualified();
    if let Ok(stream) = client.query(q, &[&qn as &dyn tiberius::ToSql]).await {
        if let Ok(rows) = stream.into_first_result().await {
            if let Some(r) = rows.first() {
                if let Ok(Some(v)) = r.try_get::<i64, _>(0) {
                    return v;
                }
                if let Ok(Some(v)) = r.try_get::<i32, _>(0) {
                    return v as i64;
                }
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnMeta, ExportContent};

    fn col(name: &str, t: &str) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            type_name: t.into(),
            max_length: 100,
            precision: 18,
            scale: 2,
            nullable: true,
            is_identity: false,
        }
    }

    fn settings(fmt: ExportFormat, scope: FileScope) -> ExportSettings {
        ExportSettings {
            dir: std::env::temp_dir(),
            format: fmt,
            scope,
            content: ExportContent::DataOnly,
            ..Default::default()
        }
    }

    fn tbl(name: &str) -> TableRef {
        TableRef {
            schema: "dbo".into(),
            name: name.into(),
            kind: "table".into(),
        }
    }

    fn path(name: &str) -> PathBuf {
        std::env::temp_dir().join(name)
    }

    #[test]
    fn json_per_table_is_valid_array() {
        let p = path("mt_test_arr.json");
        let st = settings(ExportFormat::JsonArray, FileScope::PerTable);
        let bytes = Arc::new(AtomicU64::new(0));
        let mut ex = Exporter::open(&p, &st, bytes).unwrap();
        let cols = vec![col("Id", "int"), col("Name", "nvarchar")];
        let t = tbl("T");
        ex.begin_table(&t, &cols).unwrap();
        ex.write_row(&t, &cols, &[Cell::Text("1".into()), Cell::Text("a".into())])
            .unwrap();
        ex.write_row(&t, &cols, &[Cell::Text("2".into()), Cell::Null])
            .unwrap();
        ex.end_table(&t, &cols, 2).unwrap();
        ex.finish(false).unwrap();

        let s = std::fs::read_to_string(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).expect("必须是合法 JSON");
        let arr = v.as_array().expect("顶层应为数组");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["Id"], serde_json::json!(1));
        assert_eq!(arr[1]["Name"], serde_json::Value::Null);
    }

    #[test]
    fn json_single_file_is_object_map() {
        let p = path("mt_test_map.json");
        let st = settings(ExportFormat::JsonArray, FileScope::SingleFile);
        let bytes = Arc::new(AtomicU64::new(0));
        let mut ex = Exporter::open(&p, &st, bytes).unwrap();
        let cols = vec![col("Id", "int")];
        for name in ["A", "B"] {
            let t = tbl(name);
            ex.begin_table(&t, &cols).unwrap();
            ex.write_row(&t, &cols, &[Cell::Text("7".into())]).unwrap();
            ex.end_table(&t, &cols, 1).unwrap();
        }
        ex.finish(true).unwrap();

        let s = std::fs::read_to_string(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).expect("必须是合法 JSON");
        assert_eq!(v["dbo.A"][0]["Id"], serde_json::json!(7));
        assert_eq!(v["dbo.B"][0]["Id"], serde_json::json!(7));
    }

    #[test]
    fn csv_single_file_writes_header_once() {
        let p = path("mt_test_merge.csv");
        let st = settings(ExportFormat::Csv, FileScope::SingleFile);
        let bytes = Arc::new(AtomicU64::new(0));
        let mut ex = Exporter::open(&p, &st, bytes).unwrap();
        let cols = vec![col("Id", "int")];
        for name in ["A", "B"] {
            let t = tbl(name);
            ex.begin_table(&t, &cols).unwrap();
            ex.write_row(&t, &cols, &[Cell::Text("1".into())]).unwrap();
            ex.end_table(&t, &cols, 1).unwrap();
        }
        ex.finish(true).unwrap();

        let s = std::fs::read_to_string(&p).unwrap();
        assert_eq!(s.matches("Id").count(), 1, "表头只能出现一次：{}", s);
        assert!(s.contains("dbo.A"));
        assert!(s.contains("dbo.B"));
    }

    #[test]
    fn sql_insert_quotes_and_batches() {
        let p = path("mt_test.sql");
        let mut st = settings(ExportFormat::SqlInsert, FileScope::PerTable);
        st.batch = 2;
        let bytes = Arc::new(AtomicU64::new(0));
        let mut ex = Exporter::open(&p, &st, bytes).unwrap();
        let cols = vec![col("Name", "nvarchar")];
        let t = tbl("T");
        ex.begin_table(&t, &cols).unwrap();
        for v in ["O'Brien", "x", "y"] {
            ex.write_row(&t, &cols, &[Cell::Text(v.into())]).unwrap();
        }
        ex.end_table(&t, &cols, 3).unwrap();
        ex.finish(false).unwrap();

        let s = std::fs::read_to_string(&p).unwrap();
        assert!(s.contains("INSERT INTO [dbo].[T] ([Name]) VALUES"));
        assert!(s.contains("N'O''Brien'"), "单引号必须转义：{}", s);
        assert_eq!(
            s.matches("INSERT INTO").count(),
            2,
            "3 行按批大小 2 应分两条语句"
        );
    }

    #[test]
    fn gzip_output_round_trips() {
        use flate2::read::GzDecoder;
        use std::io::Read;

        let p = path("mt_test_gz.csv.gz");
        let mut st = settings(ExportFormat::Csv, FileScope::PerTable);
        st.gzip = true;
        assert_eq!(file_ext(&st), "csv.gz");

        let bytes = Arc::new(AtomicU64::new(0));
        let mut ex = Exporter::open(&p, &st, bytes).unwrap();
        let cols = vec![col("Id", "int"), col("Name", "nvarchar")];
        let t = tbl("T");
        ex.begin_table(&t, &cols).unwrap();
        ex.write_row(
            &t,
            &cols,
            &[Cell::Text("1".into()), Cell::Text("张三".into())],
        )
        .unwrap();
        ex.end_table(&t, &cols, 1).unwrap();
        ex.finish(false).unwrap();
        drop(ex); // 触发 gzip 尾部写入

        let raw = std::fs::read(&p).unwrap();
        assert_eq!(&raw[..2], &[0x1f, 0x8b], "应为 gzip 魔数");

        let mut out = String::new();
        GzDecoder::new(&raw[..]).read_to_string(&mut out).unwrap();
        assert!(out.starts_with("Id,Name"), "解压后应有表头：{}", out);
        assert!(out.contains("张三"));
    }

    #[test]
    fn excluded_columns_parse() {
        let mut st = settings(ExportFormat::Csv, FileScope::PerTable);
        st.exclude_columns = " PasswordHash , RowVersion ,,".into();
        let set = excluded_set(&st);
        assert_eq!(set.len(), 2);
        assert!(set.contains("passwordhash"));
        assert!(set.contains("rowversion"));
    }
}
