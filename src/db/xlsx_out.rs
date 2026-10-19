//! Excel (.xlsx) 导出：原生数值 / 布尔 / 日期写入，多表多 Sheet。

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{Datelike, NaiveDate, NaiveDateTime, Timelike};
use futures_util::TryStreamExt;
use rust_xlsxwriter::{Color, ExcelDateTime, Format, Workbook, Worksheet};

use crate::db::conn::SqlClient;
use crate::db::export::TaskCtx;
use crate::db::meta;
use crate::db::value::row_to_cells;
use crate::model::{hex_encode, Cell, ColumnMeta, ExportSettings, FileScope, Summary, TableRef};

fn safe_sheet_name(raw: &str, used: &mut HashSet<String>) -> String {
    let mut s: String = raw
        .chars()
        .map(|c| if "[]:*?/\\".contains(c) { '_' } else { c })
        .collect();
    s = s.trim_matches('\'').to_string();
    if s.is_empty() {
        s = "Sheet".to_string();
    }
    if s.chars().count() > 31 {
        s = s.chars().take(31).collect();
    }
    let base = s.clone();
    let mut n = 2;
    while used.contains(&s) {
        let suffix = format!("_{}", n);
        let keep = 31usize.saturating_sub(suffix.chars().count());
        s = format!("{}{}", base.chars().take(keep).collect::<String>(), suffix);
        n += 1;
    }
    used.insert(s.clone());
    s
}

fn to_excel_dt(s: &str) -> Option<ExcelDateTime> {
    let dt = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f")
        .ok()
        .or_else(|| {
            NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
        })?;
    ExcelDateTime::from_ymd(dt.year() as u16, dt.month() as u8, dt.day() as u8)
        .ok()?
        .and_hms(
            dt.hour() as u16,
            dt.minute() as u8,
            dt.second() as f64 + dt.nanosecond() as f64 / 1e9,
        )
        .ok()
}

fn write_cell(
    ws: &mut Worksheet,
    row: u32,
    col: u16,
    cell: &Cell,
    meta: Option<&ColumnMeta>,
) -> Result<()> {
    match cell {
        Cell::Null => {}
        Cell::Bytes(b) => {
            ws.write_string(row, col, hex_encode(b))?;
        }
        Cell::Text(s) => {
            if let Some(m) = meta {
                if m.is_bool() {
                    ws.write_boolean(row, col, s == "1" || s.eq_ignore_ascii_case("true"))?;
                    return Ok(());
                }
                if m.is_numeric() {
                    if let Ok(n) = s.trim().parse::<f64>() {
                        if n.is_finite() {
                            ws.write_number(row, col, n)?;
                            return Ok(());
                        }
                    }
                }
                if m.is_temporal() {
                    if let Some(dt) = to_excel_dt(s) {
                        ws.write_datetime(row, col, dt)?;
                        return Ok(());
                    }
                }
            }
            let text: String = if s.chars().count() > 32_000 {
                s.chars().take(32_000).collect()
            } else {
                s.clone()
            };
            ws.write_string(row, col, &text)?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn write_one(
    client: &mut SqlClient,
    wb: &mut Workbook,
    table: &TableRef,
    settings: &ExportSettings,
    ctx: &TaskCtx,
    used: &mut HashSet<String>,
    idx: usize,
    total: usize,
) -> Result<u64> {
    let mut cols = meta::columns(client, table).await?;
    let exclude: HashSet<String> = settings
        .exclude_columns
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if !exclude.is_empty() {
        cols.retain(|c| !exclude.contains(&c.name.to_ascii_lowercase()));
    }
    if cols.is_empty() {
        ctx.log(format!("⚠ {} 没有可导出的列，已跳过", table.display()));
        return Ok(0);
    }

    let raw_name = if settings.schema_qualified {
        table.display()
    } else {
        table.name.clone()
    };
    let sheet_name = safe_sheet_name(&raw_name, used);

    let header_fmt = if settings.excel_bold_header {
        Format::new()
            .set_bold()
            .set_background_color(Color::RGB(0xEEF2F7))
    } else {
        Format::new()
    };

    let ws = wb.add_worksheet();
    ws.set_name(&sheet_name)?;

    if settings.header {
        for (i, c) in cols.iter().enumerate() {
            ws.write_string_with_format(0, i as u16, &c.name, &header_fmt)?;
        }
    }
    let start_row: u32 = if settings.header { 1 } else { 0 };

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

    let est = crate::db::export::estimate_rows(client, table).await;
    let mut rows: u64 = 0;
    let mut stream = client
        .query(sql, &[])
        .await
        .with_context(|| format!("查询 {} 失败", table.display()))?
        .into_row_stream();

    let mut since_tick: u64 = 0;
    while let Some(row) = stream.try_next().await? {
        let cells = row_to_cells(&row);
        let r = start_row + rows as u32;
        for (i, cell) in cells.iter().enumerate() {
            if i >= cols.len() {
                break;
            }
            write_cell(ws, r, i as u16, cell, cols.get(i))?;
        }
        rows += 1;
        since_tick += 1;
        if since_tick >= 500 {
            since_tick = 0;
            if ctx.cancelled() {
                break;
            }
            let span = 1.0 / total.max(1) as f32;
            let frac = if est > 0 {
                (rows as f32 / est as f32).min(0.999)
            } else {
                0.5
            };
            ctx.progress(
                idx as f32 * span + span * frac,
                format!("导出 {} · {} 行", table.display(), rows),
                rows,
                0,
            );
        }
    }
    drop(stream);

    if settings.excel_freeze && settings.header {
        ws.set_freeze_panes(1, 0)?;
    }
    if settings.excel_autofit {
        ws.autofit();
    }

    ctx.log(format!(
        "✓ {} → Sheet「{}」· {} 行",
        table.display(),
        sheet_name,
        rows
    ));
    Ok(rows)
}

pub async fn run(
    client: &mut SqlClient,
    settings: &ExportSettings,
    tables: &[TableRef],
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

    let single = settings.scope == FileScope::SingleFile;
    let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
    let mut total_rows: u64 = 0;
    let mut files: Vec<PathBuf> = Vec::new();

    if single {
        let path = settings
            .dir
            .join(format!("{}export_{}.xlsx", settings.file_prefix, stamp));
        if path.exists() {
            ctx.log(format!("⚠ 文件已存在，将被覆盖：{}", path.display()));
        }
        let mut wb = Workbook::new();
        let mut used = HashSet::new();
        for (idx, table) in tables.iter().enumerate() {
            if ctx.cancelled() {
                break;
            }
            total_rows += write_one(
                client,
                &mut wb,
                table,
                settings,
                ctx,
                &mut used,
                idx,
                tables.len(),
            )
            .await?;
        }
        wb.save(&path)?;
        files.push(path);
    } else {
        for (idx, table) in tables.iter().enumerate() {
            if ctx.cancelled() {
                break;
            }
            let base = if settings.schema_qualified {
                table.display()
            } else {
                table.name.clone()
            };
            let fname = format!(
                "{}{}.xlsx",
                settings.file_prefix,
                base.chars()
                    .map(|c| if "/\\:*?\"<>|".contains(c) { '_' } else { c })
                    .collect::<String>()
            );
            let path = settings.dir.join(fname);
            if path.exists() {
                ctx.log(format!("⚠ 已存在，将覆盖：{}", path.display()));
            }
            let mut wb = Workbook::new();
            let mut used = HashSet::new();
            total_rows += write_one(
                client,
                &mut wb,
                table,
                settings,
                ctx,
                &mut used,
                idx,
                tables.len(),
            )
            .await?;
            wb.save(&path)?;
            files.push(path);
        }
    }

    if ctx.cancelled() {
        return Ok(Summary {
            ok: false,
            message: format!("已取消。已写 {} 行", total_rows),
        });
    }

    ctx.progress(
        1.0,
        format!("Excel 导出完成：{} 个文件", files.len()),
        total_rows,
        0,
    );
    Ok(Summary {
        ok: true,
        message: format!(
            "Excel 导出完成：{} 个对象，{} 行，{} 个文件",
            tables.len(),
            total_rows,
            files.len()
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sheet_names_are_sanitized_and_unique() {
        let mut used = HashSet::new();
        let a = safe_sheet_name("dbo.Order/Items", &mut used);
        assert_eq!(a, "dbo.Order_Items");
        // 重复名加后缀
        let b = safe_sheet_name("dbo.Order/Items", &mut used);
        assert_eq!(b, "dbo.Order_Items_2");
        // 超长截断到 31
        let long = "x".repeat(60);
        let c = safe_sheet_name(&long, &mut used);
        assert_eq!(c.chars().count(), 31);
        // 空名兜底
        assert_eq!(safe_sheet_name("", &mut used), "Sheet");
    }

    #[test]
    fn datetime_parsing_for_cells() {
        assert!(to_excel_dt("2024-03-11 09:12:00").is_some());
        assert!(to_excel_dt("2024-03-11").is_some());
        assert!(to_excel_dt("not a date").is_none());
    }
}
