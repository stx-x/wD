//! 导入引擎：从 CSV / TSV / JSON / Excel 读取数据，参数化批量写入 SQL Server。
//! 支持递归扫描、多格式混排、Excel 多 Sheet、同名表策略。

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use calamine::{open_workbook_auto, Data, Reader};
use chrono::{DateTime, NaiveDate, NaiveDateTime};
use serde_json::Value;
use tiberius::numeric::Numeric;
use tiberius::ToSql;

use crate::db::conn::SqlClient;
use crate::db::export::TaskCtx;
use crate::db::meta;
use crate::db::value::cell_to_sql_literal;
use crate::model::{
    ColumnMeta, ColumnMode, ExportFormat, ImportFormat, ImportPlanItem, ImportSettings, OnExisting,
    Summary, TableRef,
};

// ---------------------------------------------------------------- 数据源

pub enum RowSource {
    Csv {
        rdr: csv::Reader<Box<dyn Read>>,
        total_bytes: u64,
        null_text: String,
    },
    JsonArray {
        items: Vec<Value>,
        idx: usize,
        keys: Vec<String>,
    },
    Ndjson {
        rdr: BufReader<Box<dyn Read>>,
        line: String,
        keys: Vec<String>,
        consumed: u64,
        total_bytes: u64,
    },
    Excel {
        rows: Vec<Vec<Option<String>>>,
        idx: usize,
    },
}

fn data_to_opt(d: &Data) -> Option<String> {
    match d {
        Data::Empty => None,
        Data::String(s) => {
            if s.trim().is_empty() {
                None
            } else {
                Some(s.clone())
            }
        }
        Data::Int(i) => Some(i.to_string()),
        Data::Float(f) => Some(if f.fract() == 0.0 && f.abs() < 1e15 {
            format!("{:.0}", f)
        } else {
            format!("{}", f)
        }),
        Data::Bool(b) => Some(if *b { "1".into() } else { "0".into() }),
        Data::DateTime(dt) => {
            let (y, mo, d, h, mi, s, ms) = dt.to_ymd_hms_milli();
            if dt.is_duration() {
                Some(format!("{:02}:{:02}:{:02}", h, mi, s))
            } else if h == 0 && mi == 0 && s == 0 && ms == 0 {
                Some(format!("{:04}-{:02}-{:02}", y, mo, d))
            } else {
                Some(format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
                    y, mo, d, h, mi, s, ms
                ))
            }
        }
        Data::DateTimeIso(s) => Some(s.clone()),
        Data::DurationIso(s) => Some(s.clone()),
        Data::Error(_) => None,
    }
}

/// 读取 Excel 的所有工作表名
pub fn list_sheets(path: &Path) -> Result<Vec<String>> {
    let wb =
        open_workbook_auto(path).with_context(|| format!("无法打开 Excel：{}", path.display()))?;
    Ok(wb.sheet_names().to_vec())
}

impl RowSource {
    pub fn open(
        path: &Path,
        settings: &ImportSettings,
        sheet: Option<&str>,
    ) -> Result<(Self, Vec<String>)> {
        let fmt = if settings.format == ImportFormat::Auto {
            ImportFormat::detect(path)
        } else {
            settings.format
        };

        match fmt {
            ImportFormat::Csv | ImportFormat::Tsv => {
                let file =
                    File::open(path).with_context(|| format!("无法打开 {}", path.display()))?;
                let total_bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
                let delim = if fmt == ImportFormat::Tsv {
                    b'\t'
                } else {
                    b','
                };
                let mut rdr = csv::ReaderBuilder::new()
                    .delimiter(delim)
                    .has_headers(settings.has_header)
                    .flexible(true)
                    .from_reader(Box::new(file) as Box<dyn Read>);
                let headers = if settings.has_header {
                    rdr.headers()?
                        .iter()
                        .enumerate()
                        .map(|(i, h)| {
                            let h = h.trim().trim_start_matches('\u{feff}');
                            if h.is_empty() {
                                format!("col{}", i + 1)
                            } else {
                                h.to_string()
                            }
                        })
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                Ok((
                    RowSource::Csv {
                        rdr,
                        total_bytes,
                        null_text: settings.null_text.clone(),
                    },
                    headers,
                ))
            }
            ImportFormat::Json => {
                let mut f =
                    File::open(path).with_context(|| format!("无法打开 {}", path.display()))?;
                let total_bytes = f.metadata().map(|m| m.len()).unwrap_or(0);
                let mut s = String::new();
                f.read_to_string(&mut s)?;
                let body = s.trim_start_matches('\u{feff}').trim_start();

                if body.starts_with('[') || body.starts_with('{') {
                    if let Ok(parsed) = serde_json::from_str::<Value>(body) {
                        let items = match parsed {
                            Value::Array(a) => a,
                            Value::Object(o) => vec![Value::Object(o)],
                            _ => Vec::new(),
                        };
                        let keys = collect_keys(&items);
                        return Ok((
                            RowSource::JsonArray {
                                items,
                                idx: 0,
                                keys: keys.clone(),
                            },
                            keys,
                        ));
                    }
                }

                // 回退：按 NDJSON 逐行解析
                let cursor = std::io::Cursor::new(s.into_bytes());
                let mut rdr = BufReader::new(Box::new(cursor) as Box<dyn Read>);
                let mut first = String::new();
                let n = rdr.read_line(&mut first)?;
                let mut keys = Vec::new();
                if n > 0 {
                    if let Ok(Value::Object(o)) = serde_json::from_str::<Value>(first.trim()) {
                        keys = o.keys().cloned().collect();
                    }
                }
                Ok((
                    RowSource::Ndjson {
                        rdr,
                        line: first,
                        keys: keys.clone(),
                        consumed: n as u64,
                        total_bytes,
                    },
                    keys,
                ))
            }
            ImportFormat::Excel => {
                let mut wb = open_workbook_auto(path)
                    .with_context(|| format!("无法打开 Excel：{}", path.display()))?;
                let names: Vec<String> = wb.sheet_names().to_vec();
                let chosen = match sheet {
                    Some(s) if names.iter().any(|n| n == s) => s.to_string(),
                    _ => names
                        .first()
                        .cloned()
                        .ok_or_else(|| anyhow!("Excel 文件没有工作表"))?,
                };
                let range = wb.worksheet_range(&chosen)?;
                let mut rows: Vec<Vec<Option<String>>> = Vec::new();
                for r in range.rows() {
                    let vals: Vec<Option<String>> = r.iter().map(data_to_opt).collect();
                    if vals.iter().all(|v| v.is_none()) {
                        continue;
                    }
                    rows.push(vals);
                }
                let headers = if settings.has_header && !rows.is_empty() {
                    let h = rows.remove(0);
                    h.into_iter()
                        .enumerate()
                        .map(|(i, v)| {
                            v.filter(|x| !x.trim().is_empty())
                                .unwrap_or_else(|| format!("col{}", i + 1))
                        })
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                Ok((RowSource::Excel { rows, idx: 0 }, headers))
            }
            ImportFormat::Auto => unreachable!(),
        }
    }

    pub fn progress(&self) -> f32 {
        match self {
            RowSource::Csv {
                rdr, total_bytes, ..
            } => {
                if *total_bytes == 0 {
                    0.0
                } else {
                    let pos = rdr.position().byte();
                    (pos as f32 / *total_bytes as f32).min(0.999)
                }
            }
            RowSource::JsonArray { items, idx, .. } => {
                if items.is_empty() {
                    1.0
                } else {
                    (*idx as f32 / items.len() as f32).min(0.999)
                }
            }
            RowSource::Ndjson {
                consumed,
                total_bytes,
                ..
            } => {
                if *total_bytes == 0 {
                    0.0
                } else {
                    (*consumed as f32 / *total_bytes as f32).min(0.999)
                }
            }
            RowSource::Excel { rows, idx } => {
                if rows.is_empty() {
                    1.0
                } else {
                    (*idx as f32 / rows.len() as f32).min(0.999)
                }
            }
        }
    }

    pub fn next_row(&mut self) -> Result<Option<Vec<Option<String>>>> {
        match self {
            RowSource::Csv { rdr, null_text, .. } => {
                let mut rec = csv::StringRecord::new();
                if !rdr.read_record(&mut rec)? {
                    return Ok(None);
                }
                let nt = null_text.as_str();
                Ok(Some(
                    rec.iter()
                        .enumerate()
                        .map(|(i, f)| {
                            let f = if i == 0 {
                                f.trim_start_matches('\u{feff}')
                            } else {
                                f
                            };
                            if f.is_empty() || (!nt.is_empty() && f == nt) {
                                None
                            } else {
                                Some(f.to_string())
                            }
                        })
                        .collect(),
                ))
            }
            RowSource::JsonArray { items, idx, keys } => {
                if *idx >= items.len() {
                    return Ok(None);
                }
                let item = &items[*idx];
                *idx += 1;
                Ok(Some(json_row(item, keys)))
            }
            RowSource::Ndjson {
                rdr,
                line,
                keys,
                consumed,
                ..
            } => loop {
                let cur = std::mem::take(line);
                if cur.trim().is_empty() {
                    let mut buf = String::new();
                    let n = rdr.read_line(&mut buf)?;
                    *consumed += n as u64;
                    if n == 0 {
                        return Ok(None);
                    }
                    *line = buf;
                    continue;
                }
                let v: Value = match serde_json::from_str(cur.trim()) {
                    Ok(v) => v,
                    Err(_) => {
                        let mut buf = String::new();
                        let n = rdr.read_line(&mut buf)?;
                        *consumed += n as u64;
                        if n == 0 {
                            return Ok(None);
                        }
                        *line = buf;
                        continue;
                    }
                };
                let mut buf = String::new();
                let n = rdr.read_line(&mut buf)?;
                *consumed += n as u64;
                *line = buf;
                return Ok(Some(json_row(&v, keys)));
            },
            RowSource::Excel { rows, idx } => {
                if *idx >= rows.len() {
                    return Ok(None);
                }
                let r = rows[*idx].clone();
                *idx += 1;
                Ok(Some(r))
            }
        }
    }
}

fn nonempty(f: &str) -> Option<String> {
    if f.is_empty() {
        None
    } else {
        Some(f.to_string())
    }
}

fn collect_keys(items: &[Value]) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for it in items.iter().take(500) {
        if let Value::Object(o) = it {
            for k in o.keys() {
                if !keys.contains(k) {
                    keys.push(k.clone());
                }
            }
        }
    }
    keys
}

fn json_row(v: &Value, keys: &[String]) -> Vec<Option<String>> {
    match v {
        Value::Object(o) => keys
            .iter()
            .map(|k| match o.get(k) {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => nonempty(s),
                Some(Value::Bool(b)) => Some(if *b { "1".into() } else { "0".into() }),
                Some(Value::Number(n)) => Some(n.to_string()),
                Some(other) => Some(other.to_string()),
            })
            .collect(),
        Value::Array(a) => a
            .iter()
            .map(|x| match x {
                Value::Null => None,
                Value::String(s) => nonempty(s),
                Value::Bool(b) => Some(if *b { "1".into() } else { "0".into() }),
                Value::Number(n) => Some(n.to_string()),
                other => Some(other.to_string()),
            })
            .collect(),
        _ => vec![None],
    }
}

// ---------------------------------------------------------------- 参数构造

fn parse_datetime(s: &str) -> Option<NaiveDateTime> {
    const FMTS: &[&str] = &[
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y/%m/%d %H:%M:%S%.f",
        "%d/%m/%Y %H:%M:%S",
        "%Y-%m-%d %H:%M",
    ];
    for f in FMTS {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, f) {
            return Some(dt);
        }
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.naive_utc());
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return d.and_hms_opt(0, 0, 0);
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y/%m/%d") {
        return d.and_hms_opt(0, 0, 0);
    }
    None
}

fn parse_numeric(s: &str) -> Option<Numeric> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let (neg, body) = if let Some(r) = t.strip_prefix('-') {
        (true, r)
    } else if let Some(r) = t.strip_prefix('+') {
        (false, r)
    } else {
        (false, t)
    };
    if body.contains(['e', 'E']) {
        return None;
    }
    let (int_part, dec_part) = match body.split_once('.') {
        Some((a, b)) => (a, b),
        None => (body, ""),
    };
    if dec_part.len() > 38 {
        return None;
    }
    let int_clean = if int_part.is_empty() { "0" } else { int_part };
    if !int_clean.chars().all(|c| c.is_ascii_digit())
        || !dec_part.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let digits = format!("{}{}", int_clean, dec_part);
    let v: i128 = digits.parse().ok()?;
    Some(Numeric::new_with_scale(
        if neg { -v } else { v },
        dec_part.len() as u8,
    ))
}

fn parse_bool(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "y" | "t" | "是"
    )
}

fn parse_bytes(s: &str) -> Option<Vec<u8>> {
    let t = s.trim();
    let hex = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X"))?;
    if hex.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let b = hex.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let hi = (b[i] as char).to_digit(16)?;
        let lo = (b[i + 1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
        i += 2;
    }
    Some(out)
}

/// 把源字符串按目标列类型转成 tiberius 参数
fn parse_param(raw: &Option<String>, col: &ColumnMeta) -> Box<dyn ToSql> {
    let s = match raw {
        None => return Box::new(None::<String>),
        Some(s) => s,
    };
    let t = col.type_name.as_str();

    if s.is_empty()
        && !matches!(
            t,
            "varchar" | "nvarchar" | "char" | "nchar" | "text" | "ntext" | "xml"
        )
    {
        return Box::new(None::<String>);
    }

    match t {
        "bit" => Box::new(parse_bool(s)),
        "tinyint" => match s.trim().parse::<u8>() {
            Ok(v) => Box::new(v),
            Err(_) => Box::new(None::<String>),
        },
        "smallint" => match s.trim().parse::<i16>() {
            Ok(v) => Box::new(v),
            Err(_) => Box::new(None::<String>),
        },
        "int" => match s.trim().parse::<i32>() {
            Ok(v) => Box::new(v),
            Err(_) => Box::new(None::<String>),
        },
        "bigint" => match s.trim().parse::<i64>() {
            Ok(v) => Box::new(v),
            Err(_) => Box::new(None::<String>),
        },
        "real" => match s.trim().parse::<f32>() {
            Ok(v) => Box::new(v),
            Err(_) => Box::new(None::<String>),
        },
        "float" | "money" | "smallmoney" => match s.trim().parse::<f64>() {
            Ok(v) => Box::new(v),
            Err(_) => Box::new(None::<String>),
        },
        "decimal" | "numeric" => match parse_numeric(s) {
            Some(n) => Box::new(n),
            None => Box::new(s.clone()),
        },
        "binary" | "varbinary" | "image" => match parse_bytes(s) {
            Some(b) => Box::new(b),
            None => Box::new(None::<String>),
        },
        "date" | "datetime" | "datetime2" | "smalldatetime" | "datetimeoffset" | "time" => {
            match parse_datetime(s) {
                Some(dt) => Box::new(dt),
                None => Box::new(s.clone()),
            }
        }
        _ => Box::new(s.clone()),
    }
}

// ---------------------------------------------------------------- 建表推断

fn infer_type(values: &[Option<String>]) -> String {
    let vals: Vec<&str> = values
        .iter()
        .filter_map(|v| v.as_deref())
        .filter(|v| !v.is_empty())
        .collect();
    if vals.is_empty() {
        return "nvarchar(max)".into();
    }
    if vals.iter().all(|v| v.parse::<i64>().is_ok()) {
        // 能装进 int 的用 int，否则 bigint
        return if vals.iter().all(|v| v.parse::<i32>().is_ok()) {
            "int".into()
        } else {
            "bigint".into()
        };
    }
    if vals.iter().all(|v| v.parse::<f64>().is_ok()) {
        return "float".into();
    }
    if vals.iter().all(|v| parse_datetime(v).is_some()) {
        return "datetime2(3)".into();
    }
    let max_len = vals.iter().map(|v| v.chars().count()).max().unwrap_or(0);
    if max_len <= 200 {
        format!("nvarchar({})", ((max_len / 10 + 1) * 10).clamp(10, 4000))
    } else {
        "nvarchar(max)".into()
    }
}

async fn table_exists(client: &mut SqlClient, table: &TableRef) -> Result<bool> {
    let q = "SELECT CASE WHEN OBJECT_ID(@P1, N'U') IS NULL THEN 0 ELSE 1 END";
    let qn = table.qualified();
    let rows = client
        .query(q, &[&qn as &dyn ToSql])
        .await?
        .into_first_result()
        .await?;
    Ok(rows
        .first()
        .and_then(|r| r.try_get::<i32, _>(0).ok().flatten())
        .unwrap_or(0)
        == 1)
}

// ---------------------------------------------------------------- 扫描与计划

/// 简易通配符匹配：`*` 任意长度、`?` 单字符。空模式视为全部匹配。
pub fn glob_match(pattern: &str, name: &str) -> bool {
    fn go(p: &[u8], n: &[u8]) -> bool {
        if p.is_empty() {
            return n.is_empty();
        }
        match p[0] {
            b'*' => {
                let rest = &p[1..];
                if go(rest, n) {
                    return true;
                }
                for i in 0..n.len() {
                    if go(rest, &n[i + 1..]) {
                        return true;
                    }
                }
                false
            }
            b'?' => !n.is_empty() && go(&p[1..], &n[1..]),
            c => !n.is_empty() && n[0] == c && go(&p[1..], &n[1..]),
        }
    }
    let p = pattern.trim();
    if p.is_empty() {
        return true;
    }
    go(p.as_bytes(), name.as_bytes())
}

fn supported_ext(fmt: ImportFormat) -> &'static [&'static str] {
    match fmt {
        ImportFormat::Auto => &[
            "csv", "tsv", "tab", "txt", "json", "jsonl", "ndjson", "xlsx", "xlsm", "xlsb", "xls",
            "ods",
        ],
        ImportFormat::Csv => &["csv", "txt"],
        ImportFormat::Tsv => &["tsv", "tab"],
        ImportFormat::Json => &["json", "jsonl", "ndjson"],
        ImportFormat::Excel => &["xlsx", "xlsm", "xlsb", "xls", "ods"],
    }
}

/// 清理成合法表名（保留中文，替换空格与特殊符号，合并连续下划线）
fn sanitize_table_name(raw: &str) -> String {
    let mapped: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_whitespace() || "/\\:*?\"<>|.()[]{}'`,;".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    // 合并连续下划线，去掉首尾下划线
    let mut out = String::with_capacity(mapped.len());
    let mut prev_us = false;
    for c in mapped.chars() {
        if c == '_' {
            if !prev_us {
                out.push('_');
            }
            prev_us = true;
        } else {
            out.push(c);
            prev_us = false;
        }
    }
    let s = out.trim_matches('_').to_string();
    if s.is_empty() {
        "imported".to_string()
    } else {
        s
    }
}

/// 由文件名推导目标表名（应用 strip-prefix / strip-suffix）
fn derive_table_name(file: &Path, settings: &ImportSettings) -> String {
    let mut stem = file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("imported")
        .to_string();
    let pre = settings.strip_prefix.trim();
    if !pre.is_empty() {
        if let Some(rest) = stem.strip_prefix(pre) {
            stem = rest.to_string();
        }
    }
    let suf = settings.strip_suffix.trim();
    if !suf.is_empty() {
        if let Some(rest) = stem.strip_suffix(suf) {
            stem = rest.to_string();
        }
    }
    sanitize_table_name(&stem)
}

/// 扫描源路径，生成导入计划（递归 / 通配 / 多格式 / Excel 多 Sheet）
pub fn scan_plan(settings: &ImportSettings, pattern: &str) -> Result<Vec<ImportPlanItem>> {
    let path = &settings.path;
    if !path.exists() {
        return Err(anyhow!("源路径不存在：{}", path.display()));
    }
    let exts = supported_ext(settings.format);
    let pat = {
        let p = pattern.trim();
        if p.is_empty() {
            settings.glob.trim()
        } else {
            p
        }
    };

    let mut files: Vec<PathBuf> = Vec::new();
    if path.is_dir() {
        let depth = if settings.recursive { usize::MAX } else { 1 };
        for e in walkdir::WalkDir::new(path)
            .max_depth(depth)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !e.file_type().is_file() {
                continue;
            }
            let p = e.path().to_path_buf();
            let ext = p
                .extension()
                .and_then(|x| x.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if !exts.contains(&ext.as_str()) {
                continue;
            }
            let fname = p.file_name().and_then(|x| x.to_str()).unwrap_or("");
            if !glob_match(pat, fname) {
                continue;
            }
            files.push(p);
        }
        files.sort();
    } else {
        files.push(path.clone());
    }

    if files.is_empty() {
        return Err(anyhow!(
            "没有匹配到可导入的文件{}",
            if pat.is_empty() {
                String::new()
            } else {
                format!("（通配符 `{}`）", pat)
            }
        ));
    }

    let mut items: Vec<ImportPlanItem> = Vec::new();
    for f in files {
        let fmt = if settings.format == ImportFormat::Auto {
            ImportFormat::detect(&f)
        } else {
            settings.format
        };
        let derived = derive_table_name(&f, settings);

        // Excel 多 Sheet：每个工作表一个表
        let mut targets: Vec<(Option<String>, String)> = Vec::new();
        if fmt == ImportFormat::Excel && settings.all_sheets {
            if let Ok(names) = list_sheets(&f) {
                if names.len() > 1 {
                    for n in names.iter() {
                        targets.push((Some(n.clone()), sanitize_table_name(n)));
                    }
                }
            }
        }
        if targets.is_empty() {
            targets.push((None, derived));
        }

        let single = targets.len() == 1;
        for (sheet, name) in targets {
            let table_name = if single && !settings.target.name.trim().is_empty() {
                settings.target.name.trim().to_string()
            } else {
                name
            };
            let note = match &sheet {
                Some(s) => format!(
                    "{} · {}",
                    f.file_name().and_then(|x| x.to_str()).unwrap_or(""),
                    s
                ),
                None => f
                    .file_name()
                    .and_then(|x| x.to_str())
                    .unwrap_or("")
                    .to_string(),
            };
            items.push(ImportPlanItem {
                path: f.clone(),
                format: fmt,
                target: TableRef {
                    schema: settings.schema.clone(),
                    name: table_name,
                    kind: "table".into(),
                },
                sheet,
                enabled: true,
                note,
            });
        }
    }
    Ok(items)
}

// ---------------------------------------------------------------- 主流程

pub async fn run(
    client: &mut SqlClient,
    settings: &ImportSettings,
    items: &[ImportPlanItem],
    ctx: &TaskCtx,
) -> Result<Summary> {
    let active: Vec<&ImportPlanItem> = items.iter().filter(|i| i.enabled).collect();
    if active.is_empty() {
        return Ok(Summary {
            ok: false,
            message: "没有启用任何导入任务".into(),
        });
    }

    let total = active.len();
    let mut total_rows: u64 = 0;
    let mut total_errors: u64 = 0;
    let mut ok_items = 0usize;
    let mut skipped = 0usize;

    for (idx, item) in active.iter().enumerate() {
        if ctx.cancelled() {
            break;
        }
        let base = idx as f32 / total as f32;
        let span = 1.0 / total as f32;
        ctx.log(format!(
            "▶ [{}/{}] {} → {}",
            idx + 1,
            total,
            item.note,
            item.target.qualified()
        ));

        match import_one(client, item, settings, ctx, base, span).await {
            Ok((rows, errs, was_skipped)) => {
                if was_skipped {
                    skipped += 1;
                    ctx.log(format!(
                        "↷ 跳过 {}（目标表已存在）",
                        item.target.qualified()
                    ));
                } else {
                    ok_items += 1;
                    total_rows += rows;
                    total_errors += errs;
                    ctx.log(format!(
                        "✓ {} 完成：{} 行{}",
                        item.target.qualified(),
                        rows,
                        if errs > 0 {
                            format!("，{} 行失败", errs)
                        } else {
                            String::new()
                        }
                    ));
                }
            }
            Err(e) => {
                if settings.skip_errors {
                    ctx.log(format!("✗ {} 失败：{:#}", item.target.qualified(), e));
                    total_errors += 1;
                } else {
                    return Err(e.context(format!("导入 {} 失败", item.target.qualified())));
                }
            }
        }
    }

    ctx.progress(
        1.0,
        if ctx.cancelled() {
            "导入已取消"
        } else {
            "导入完成"
        },
        total_rows,
        0,
    );

    Ok(Summary {
        ok: !ctx.cancelled(),
        message: format!(
            "{}：成功 {} 个任务 / {} 行{}",
            if ctx.cancelled() {
                "已取消"
            } else {
                "导入完成"
            },
            ok_items,
            total_rows,
            {
                let mut tail = String::new();
                if total_errors > 0 {
                    tail.push_str(&format!("，失败 {}", total_errors));
                }
                if skipped > 0 {
                    tail.push_str(&format!("，跳过 {}", skipped));
                }
                tail
            }
        ),
    })
}

async fn import_one(
    client: &mut SqlClient,
    item: &ImportPlanItem,
    settings: &ImportSettings,
    ctx: &TaskCtx,
    base: f32,
    span: f32,
) -> Result<(u64, u64, bool)> {
    let target = &item.target;
    if target.name.trim().is_empty() {
        return Err(anyhow!("目标表名为空"));
    }

    // 以计划项自身格式打开（Auto 已在扫描阶段解析）
    let mut eff = settings.clone();
    eff.format = item.format;
    let sheet = item.sheet.as_deref();

    let (mut src, headers) = RowSource::open(&item.path, &eff, sheet)?;

    let exists = table_exists(client, target).await?;
    if exists {
        match settings.on_existing {
            OnExisting::Skip => return Ok((0, 0, true)),
            OnExisting::Fail => {
                return Err(anyhow!(
                    "目标表 {} 已存在，按「已存在时中止」设置停止",
                    target.qualified()
                ))
            }
            _ => {}
        }
    }

    let mut cols = if exists {
        meta::columns(client, target).await?
    } else {
        Vec::new()
    };

    if !exists {
        if !settings.create_table {
            return Err(anyhow!(
                "目标表 {} 不存在。可勾选「自动建表」，或先手动创建。",
                target.qualified()
            ));
        }
        // 采样推断
        let mut samples: Vec<Vec<Option<String>>> = Vec::new();
        for _ in 0..200 {
            match src.next_row()? {
                Some(r) => samples.push(r),
                None => break,
            }
        }
        if headers.is_empty() && samples.is_empty() {
            return Err(anyhow!("源文件为空，无法推断表结构"));
        }
        let width = headers
            .len()
            .max(samples.iter().map(|r| r.len()).max().unwrap_or(0));
        let names: Vec<String> = (0..width)
            .map(|i| {
                headers
                    .get(i)
                    .filter(|h| !h.is_empty())
                    .cloned()
                    .unwrap_or_else(|| format!("col{}", i + 1))
            })
            .collect();
        let mut ddl_cols = Vec::new();
        for (i, name) in names.iter().enumerate() {
            let sample: Vec<Option<String>> = samples
                .iter()
                .map(|r| r.get(i).cloned().unwrap_or(None))
                .collect();
            let ty = infer_type(&sample);
            ddl_cols.push(format!("    [{}] {} NULL", name.replace(']', "]]"), ty));
        }
        let ddl = format!(
            "IF OBJECT_ID(N'{q}', N'U') IS NULL\nCREATE TABLE {q} (\n{c}\n);",
            q = target.qualified(),
            c = ddl_cols.join(",\n")
        );
        ctx.log(format!("＋ 自动建表 {}", target.qualified()));
        client.execute(ddl, &[]).await?;
        cols = meta::columns(client, target).await?;
        // 重新打开，从头导入
        let (s2, _) = RowSource::open(&item.path, &eff, sheet)?;
        src = s2;
    }

    if cols.is_empty() {
        return Err(anyhow!("无法读取目标表列定义"));
    }

    // 列映射
    let src_names: Vec<String> = if headers.is_empty() {
        (0..cols.len()).map(|i| format!("col{}", i + 1)).collect()
    } else {
        headers.clone()
    };

    let mapping: Vec<(usize, usize)> = match settings.column_mode {
        ColumnMode::ByName => {
            let mut m = Vec::new();
            for (si, sn) in src_names.iter().enumerate() {
                if let Some((ti, _)) = cols
                    .iter()
                    .enumerate()
                    .find(|(_, c)| c.name.eq_ignore_ascii_case(sn))
                {
                    m.push((si, ti));
                }
            }
            m
        }
        ColumnMode::ByPosition => {
            let n = src_names.len().min(cols.len());
            (0..n).map(|i| (i, i)).collect()
        }
    };

    if mapping.is_empty() {
        return Err(anyhow!(
            "源文件列名与目标表不匹配。源列：{}",
            src_names.join(", ")
        ));
    }
    if mapping.len() < cols.len() {
        let missing: Vec<String> = cols
            .iter()
            .enumerate()
            .filter(|(i, _)| !mapping.iter().any(|(_, t)| t == i))
            .map(|(_, c)| c.name.clone())
            .collect();
        if !missing.is_empty() {
            ctx.log(format!(
                "  提示：以下列未提供值，将使用默认值 / NULL：{}",
                missing.join(", ")
            ));
        }
    }

    let target_cols: Vec<&ColumnMeta> = mapping.iter().map(|(_, t)| &cols[*t]).collect();
    let col_list = target_cols
        .iter()
        .map(|c| format!("[{}]", c.name))
        .collect::<Vec<_>>()
        .join(", ");

    let identity_cols: Vec<&ColumnMeta> = target_cols
        .iter()
        .copied()
        .filter(|c| c.is_identity)
        .collect();
    let identity_on = !identity_cols.is_empty();

    let use_tx = settings.transaction;
    if use_tx {
        client
            .simple_query("BEGIN TRANSACTION")
            .await?
            .into_results()
            .await?;
    }

    // 清空必须放在事务内，否则导入失败回滚后数据已丢失
    let do_truncate = exists && (settings.truncate || settings.on_existing == OnExisting::Truncate);
    if do_truncate {
        let sql = format!("TRUNCATE TABLE {}", target.qualified());
        match client.execute(sql.as_str(), &[]).await {
            Ok(_) => ctx.log(format!("  已清空 {}", target.qualified())),
            Err(_) => {
                client
                    .execute(format!("DELETE FROM {}", target.qualified()).as_str(), &[])
                    .await?;
                ctx.log(format!("  已清空（DELETE）{}", target.qualified()));
            }
        }
    }

    if identity_on {
        client
            .simple_query(format!("SET IDENTITY_INSERT {} ON", target.qualified()).as_str())
            .await?
            .into_results()
            .await?;
    }

    let max_per_stmt = (2000 / target_cols.len().max(1)).max(1);
    let batch_size = settings.batch.max(1).min(max_per_stmt);

    let mut inserted: u64 = 0;
    let mut errors: u64 = 0;
    let mut batch: Vec<Vec<Option<String>>> = Vec::with_capacity(batch_size);
    let mut aborted: Option<anyhow::Error> = None;

    loop {
        if ctx.cancelled() {
            break;
        }
        let row = match src.next_row() {
            Ok(Some(r)) => r,
            Ok(None) => break,
            Err(e) => {
                if settings.skip_errors {
                    errors += 1;
                    if errors >= settings.max_errors {
                        ctx.log(format!("  已达最大错误数 {}，停止", settings.max_errors));
                        break;
                    }
                    continue;
                } else {
                    aborted = Some(e);
                    break;
                }
            }
        };

        batch.push(
            mapping
                .iter()
                .map(|(si, _)| row.get(*si).cloned().unwrap_or(None))
                .collect(),
        );

        if batch.len() >= batch_size {
            match flush_batch(client, target, &target_cols, &col_list, &batch).await {
                Ok(_) => inserted += batch.len() as u64,
                Err(e) => {
                    if settings.skip_errors {
                        for r in batch.iter() {
                            match flush_batch(
                                client,
                                target,
                                &target_cols,
                                &col_list,
                                std::slice::from_ref(r),
                            )
                            .await
                            {
                                Ok(_) => inserted += 1,
                                Err(e2) => {
                                    errors += 1;
                                    if errors <= 5 {
                                        ctx.log(format!("  ✗ 行失败：{}", short_err(&e2)));
                                    }
                                    if errors >= settings.max_errors {
                                        break;
                                    }
                                }
                            }
                        }
                    } else {
                        aborted = Some(e);
                        break;
                    }
                }
            }
            batch.clear();
            ctx.progress(
                base + span * src.progress(),
                format!("导入 {} · {} 行", target.qualified(), inserted),
                inserted,
                0,
            );
        }
    }

    if !batch.is_empty() && aborted.is_none() {
        match flush_batch(client, target, &target_cols, &col_list, &batch).await {
            Ok(_) => inserted += batch.len() as u64,
            Err(e) => {
                if settings.skip_errors {
                    for r in batch.iter() {
                        match flush_batch(
                            client,
                            target,
                            &target_cols,
                            &col_list,
                            std::slice::from_ref(r),
                        )
                        .await
                        {
                            Ok(_) => inserted += 1,
                            Err(e2) => {
                                errors += 1;
                                if errors <= 5 {
                                    ctx.log(format!("  ✗ 行失败：{}", short_err(&e2)));
                                }
                            }
                        }
                    }
                } else {
                    aborted = Some(e);
                }
            }
        }
    }

    if identity_on {
        client
            .simple_query(format!("SET IDENTITY_INSERT {} OFF", target.qualified()).as_str())
            .await?
            .into_results()
            .await?;
    }

    if use_tx {
        if aborted.is_some() || (ctx.cancelled() && !settings.skip_errors) {
            client
                .simple_query("ROLLBACK TRANSACTION")
                .await?
                .into_results()
                .await?;
        } else {
            client
                .simple_query("COMMIT TRANSACTION")
                .await?
                .into_results()
                .await?;
        }
    }

    if let Some(e) = aborted {
        return Err(e);
    }

    ctx.progress(
        base + span,
        format!("导入 {} 完成", target.qualified()),
        inserted,
        0,
    );
    Ok((inserted, errors, false))
}

fn short_err(e: &anyhow::Error) -> String {
    let s = format!("{:#}", e);
    if s.chars().count() > 180 {
        s.chars().take(180).collect::<String>() + "…"
    } else {
        s
    }
}

async fn flush_batch(
    client: &mut SqlClient,
    target: &TableRef,
    cols: &[&ColumnMeta],
    col_list: &str,
    rows: &[Vec<Option<String>>],
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut sql = format!("INSERT INTO {} ({}) VALUES\n", target.qualified(), col_list);
    let mut tuples = Vec::with_capacity(rows.len());
    let mut idx = 1usize;
    let mut params: Vec<Box<dyn ToSql>> = Vec::with_capacity(rows.len() * cols.len());

    for row in rows {
        let mut ph = Vec::with_capacity(cols.len());
        for (ci, col) in cols.iter().enumerate() {
            ph.push(format!("@P{}", idx));
            idx += 1;
            let raw = row.get(ci).cloned().unwrap_or(None);
            params.push(parse_param(&raw, col));
        }
        tuples.push(format!("({})", ph.join(", ")));
    }
    sql.push_str(&tuples.join(",\n"));

    let refs: Vec<&dyn ToSql> = params.iter().map(|b| &**b).collect();
    client
        .execute(sql.as_str(), &refs)
        .await
        .with_context(|| "INSERT 执行失败")?;
    Ok(())
}

/// 仅预览：读取前 N 行，返回格式化文本
pub fn preview(
    path: &Path,
    settings: &ImportSettings,
    sheet: Option<&str>,
    limit: usize,
) -> Result<String> {
    if path.is_dir() {
        let mut names: Vec<String> = std::fs::read_dir(path)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .take(40)
            .collect();
        names.sort();
        return Ok(format!(
            "目录包含 {} 个条目：\n{}",
            names.len(),
            names.join("\n")
        ));
    }
    let (mut src, headers) = RowSource::open(path, settings, sheet)?;
    let mut out = String::new();
    if !headers.is_empty() {
        out.push_str(&headers.join(" | "));
        out.push('\n');
        out.push_str(&"-".repeat(headers.join(" | ").chars().count().min(120)));
        out.push('\n');
    }
    for _ in 0..limit {
        match src.next_row()? {
            Some(r) => {
                let line: Vec<String> = r
                    .iter()
                    .map(|v| v.clone().unwrap_or_else(|| "∅".into()))
                    .collect();
                out.push_str(&line.join(" | "));
                out.push('\n');
            }
            None => break,
        }
    }
    if out.is_empty() {
        out.push_str("（没有可预览的数据）");
    }
    Ok(out)
}

/// 导出 SQL 预览
pub async fn preview_sql(
    client: &mut SqlClient,
    settings: &crate::model::ExportSettings,
    tables: &[TableRef],
    limit: usize,
) -> Result<String> {
    let mut out = String::new();
    for table in tables.iter().take(3) {
        let cols = meta::columns(client, table).await?;
        if cols.is_empty() {
            continue;
        }
        let pk = meta::primary_key(client, table).await.unwrap_or_default();
        if settings.format == ExportFormat::SqlFull {
            out.push_str(&meta::create_table_sql(
                table,
                &cols,
                &pk,
                settings.drop_table,
            ));
            out.push_str("GO\n");
        }
        let col_list = cols
            .iter()
            .map(|c| format!("[{}]", c.name))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "INSERT INTO {} ({}) VALUES\n",
            table.qualified(),
            col_list
        ));

        let sql = format!(
            "SELECT TOP {} {} FROM {}",
            limit,
            col_list,
            table.qualified()
        );
        let rows = client.query(sql, &[]).await?.into_first_result().await?;
        let mut tuples = Vec::new();
        for row in rows.iter() {
            let cells = crate::db::value::row_to_cells(row);
            let vals: Vec<String> = cells
                .iter()
                .enumerate()
                .map(|(i, c)| cell_to_sql_literal(c, cols.get(i)))
                .collect();
            tuples.push(format!("({})", vals.join(", ")));
        }
        out.push_str(&tuples.join(",\n"));
        out.push_str(";\n\n");
    }
    if out.is_empty() {
        out.push_str("（没有可预览的数据）");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_parse_keeps_scale() {
        let n = parse_numeric("123.45").unwrap();
        assert_eq!(n.value(), 12345);
        assert_eq!(n.scale(), 2);
        let n2 = parse_numeric("-0.5").unwrap();
        assert_eq!(n2.value(), -5);
        assert_eq!(n2.scale(), 1);
        assert!(parse_numeric("abc").is_none());
        assert!(parse_numeric("1e5").is_none());
    }

    #[test]
    fn infer_types_from_samples() {
        assert_eq!(infer_type(&[Some("1".into()), Some("22".into())]), "int");
        // 超出 int 范围才升级为 bigint
        assert_eq!(
            infer_type(&[Some("1".into()), Some("9999999999".into())]),
            "bigint"
        );
        assert_eq!(infer_type(&[Some("1.5".into()), Some("2".into())]), "float");
        assert!(infer_type(&[Some("2024-01-02 03:04:05".into())]).starts_with("datetime2"));
        assert!(infer_type(&[Some("张三".into())]).starts_with("nvarchar"));
    }

    #[test]
    fn bool_variants() {
        assert!(parse_bool("TRUE"));
        assert!(parse_bool("1"));
        assert!(parse_bool("是"));
        assert!(!parse_bool("0"));
        assert!(!parse_bool("no"));
    }

    #[test]
    fn hex_bytes() {
        assert_eq!(parse_bytes("0xDEAD").unwrap(), vec![0xDE, 0xAD]);
        assert!(parse_bytes("0xDEA").is_none());
        assert!(parse_bytes("DEAD").is_none());
    }

    #[test]
    fn datetime_formats() {
        assert!(parse_datetime("2024-01-02 03:04:05").is_some());
        assert!(parse_datetime("2024-01-02T03:04:05.123").is_some());
        assert!(parse_datetime("2024-01-02").is_some());
        assert!(parse_datetime("not a date").is_none());
    }

    #[test]
    fn glob_patterns() {
        assert!(glob_match("", "anything.csv"));
        assert!(glob_match("*", "anything.csv"));
        assert!(glob_match("*.csv", "orders.csv"));
        assert!(!glob_match("*.csv", "orders.tsv"));
        assert!(glob_match("orders_2024*.csv", "orders_2024_01.csv"));
        assert!(glob_match("data?.json", "data1.json"));
        assert!(!glob_match("data?.json", "data12.json"));
        assert!(glob_match("a*b*c.txt", "aXXbYYc.txt"));
        assert!(!glob_match("a*b*c.txt", "aXXbYYd.txt"));
    }

    #[test]
    fn table_name_derivation() {
        let s = ImportSettings {
            strip_prefix: "exp_".into(),
            strip_suffix: "_dump".into(),
            ..Default::default()
        };
        assert_eq!(
            derive_table_name(Path::new("/tmp/exp_Customers_dump.csv"), &s),
            "Customers"
        );
        // 空格与非法字符被替换
        assert_eq!(
            derive_table_name(Path::new("/tmp/order items (2024).csv"), &s),
            "order_items_2024"
        );
        // 空名兜底
        assert_eq!(sanitize_table_name("   "), "imported");
    }

    #[test]
    fn ext_filtering_by_format() {
        assert!(supported_ext(ImportFormat::Auto).contains(&"xlsx"));
        assert!(supported_ext(ImportFormat::Auto).contains(&"csv"));
        assert!(!supported_ext(ImportFormat::Json).contains(&"xlsx"));
        assert!(supported_ext(ImportFormat::Excel).contains(&"ods"));
    }

    #[test]
    fn scan_plan_over_temp_dir() {
        use std::io::Write;
        let dir = std::env::temp_dir().join("mt_scan_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        for (name, body) in [
            ("a.csv", "Id,Name\n1,x\n"),
            ("sub/b.csv", "Id,Name\n2,y\n"),
            ("skip.txt", "nope"),
        ] {
            let mut f = std::fs::File::create(dir.join(name)).unwrap();
            f.write_all(body.as_bytes()).unwrap();
        }

        let mut s = ImportSettings {
            path: dir.clone(),
            format: ImportFormat::Auto,
            ..Default::default()
        };

        // 递归：a.csv + sub/b.csv + skip.txt(txt 也属 csv 扩展) => 3
        let all = scan_plan(&s, "").unwrap();
        assert_eq!(all.len(), 3, "递归应包含子目录中的 csv 与顶层 txt");

        // 非递归：只有顶层两个
        s.recursive = false;
        let top = scan_plan(&s, "").unwrap();
        assert_eq!(top.len(), 2);

        // 通配符过滤
        s.recursive = true;
        let g = scan_plan(&s, "a*").unwrap();
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].target.name, "a");
        assert_eq!(g[0].format, ImportFormat::Csv);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reads_generated_excel_sheets() {
        use rust_xlsxwriter::Workbook;

        let path = std::env::temp_dir().join("mt_import_test.xlsx");
        let mut wb = Workbook::new();
        for (name, id) in [("Alpha", 1.0), ("Beta", 2.0)] {
            let ws = wb.add_worksheet();
            ws.set_name(name).unwrap();
            // 第 0 行：列头
            ws.write_string(0, 0, "Id").unwrap();
            ws.write_string(0, 1, "Name").unwrap();
            // 第 1 行：一条数据
            ws.write_number(1, 0, id).unwrap();
            ws.write_string(1, 1, "text").unwrap();
        }
        wb.save(&path).unwrap();

        // 1) 列出工作表
        let sheets = list_sheets(&path).unwrap();
        assert_eq!(sheets, vec!["Alpha".to_string(), "Beta".to_string()]);

        // 2) 读取指定 Sheet 的列头与数据
        let s = ImportSettings::default();
        let (mut src, headers) = RowSource::open(&path, &s, Some("Beta")).unwrap();
        assert_eq!(headers, vec!["Id".to_string(), "Name".to_string()]);
        let row = src.next_row().unwrap().unwrap();
        assert_eq!(row[0].as_deref(), Some("2"));
        assert_eq!(row[1].as_deref(), Some("text"));
        assert!(src.next_row().unwrap().is_none());

        // 3) all_sheets 扫描应拆成两个任务
        let mut st = ImportSettings {
            path: path.clone(),
            format: ImportFormat::Auto,
            all_sheets: true,
            ..Default::default()
        };
        let plan = scan_plan(&st, "").unwrap();
        assert_eq!(plan.len(), 2, "两个 Sheet 应生成两个任务");
        assert_eq!(plan[0].target.name, "Alpha");
        assert_eq!(plan[1].target.name, "Beta");
        assert_eq!(plan[0].format, ImportFormat::Excel);

        // 4) 关闭多 Sheet 时只生成一个任务
        st.all_sheets = false;
        let one = scan_plan(&st, "").unwrap();
        assert_eq!(one.len(), 1);
        assert!(one[0].sheet.is_none());

        let _ = std::fs::remove_file(&path);
    }
}
