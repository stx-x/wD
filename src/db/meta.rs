//! 元数据查询：数据库列表、表/视图列表、列定义、主键。

use anyhow::Result;
use tiberius::Row;

use crate::db::conn::SqlClient;
use crate::model::{ColumnMeta, TableRef};

fn s(row: &Row, i: usize) -> String {
    row.try_get::<&str, _>(i)
        .ok()
        .flatten()
        .unwrap_or_default()
        .to_string()
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

pub async fn list_databases(client: &mut SqlClient) -> Result<Vec<String>> {
    let rows = client
        .query(
            "SELECT name FROM sys.databases
             WHERE state = 0 AND HAS_DBACCESS(name) = 1
             ORDER BY CASE WHEN name IN ('master','tempdb','model','msdb') THEN 1 ELSE 0 END, name",
            &[],
        )
        .await?
        .into_first_result()
        .await?;
    Ok(rows.iter().map(|r| s(r, 0)).collect())
}

pub async fn list_objects(client: &mut SqlClient) -> Result<Vec<(TableRef, i64)>> {
    let rows = client
        .query(
            "SELECT s.name AS schema_name,
                    o.name AS object_name,
                    o.type,
                    ISNULL(SUM(p.rows), 0) AS row_count
             FROM sys.objects o
             JOIN sys.schemas s ON s.schema_id = o.schema_id
             LEFT JOIN sys.partitions p
                    ON p.object_id = o.object_id AND p.index_id IN (0, 1)
             WHERE o.type IN ('U', 'V') AND o.is_ms_shipped = 0
             GROUP BY s.name, o.name, o.type
             ORDER BY o.type, s.name, o.name",
            &[],
        )
        .await?
        .into_first_result()
        .await?;

    Ok(rows
        .iter()
        .map(|r| {
            let kind_raw = s(r, 2);
            let kind = match kind_raw.trim() {
                "V" => "view",
                _ => "table",
            };
            (
                TableRef {
                    schema: s(r, 0),
                    name: s(r, 1),
                    kind: kind.to_string(),
                },
                i64v(r, 3),
            )
        })
        .collect())
}

pub async fn columns(client: &mut SqlClient, table: &TableRef) -> Result<Vec<ColumnMeta>> {
    let qn = table.qualified();
    let rows = client
        .query(
            "SELECT c.name,
                    t.name AS type_name,
                    c.max_length,
                    c.precision,
                    c.scale,
                    c.is_nullable,
                    c.is_identity
             FROM sys.columns c
             JOIN sys.types t ON t.user_type_id = c.user_type_id
             WHERE c.object_id = OBJECT_ID(@P1)
             ORDER BY c.column_id",
            &[&qn as &dyn tiberius::ToSql],
        )
        .await?
        .into_first_result()
        .await?;

    Ok(rows
        .iter()
        .map(|r| ColumnMeta {
            name: s(r, 0),
            type_name: s(r, 1).to_ascii_lowercase(),
            max_length: i32v(r, 2),
            precision: i32v(r, 3),
            scale: i32v(r, 4),
            nullable: boolv(r, 5),
            is_identity: boolv(r, 6),
        })
        .collect())
}

pub async fn primary_key(client: &mut SqlClient, table: &TableRef) -> Result<Vec<String>> {
    let qn = table.qualified();
    let rows = client
        .query(
            "SELECT c.name
             FROM sys.indexes i
             JOIN sys.index_columns ic
               ON ic.object_id = i.object_id AND ic.index_id = i.index_id
             JOIN sys.columns c
               ON c.object_id = ic.object_id AND c.column_id = ic.column_id
             WHERE i.is_primary_key = 1 AND i.object_id = OBJECT_ID(@P1)
             ORDER BY ic.key_ordinal",
            &[&qn as &dyn tiberius::ToSql],
        )
        .await?
        .into_first_result()
        .await?;
    Ok(rows.iter().map(|r| s(r, 0)).collect())
}

/// 生成 CREATE TABLE 语句
pub fn create_table_sql(
    table: &TableRef,
    cols: &[ColumnMeta],
    pk: &[String],
    drop_first: bool,
) -> String {
    let mut out = String::new();
    if drop_first {
        out.push_str(&format!(
            "IF OBJECT_ID(N'{}.{}', N'U') IS NOT NULL DROP TABLE {};\n",
            table.schema.replace('\'', "''"),
            table.name.replace('\'', "''"),
            table.qualified()
        ));
    }
    out.push_str(&format!("CREATE TABLE {} (\n", table.qualified()));
    let mut lines: Vec<String> = Vec::new();
    for c in cols {
        let mut line = format!("    [{}] {}", c.name, c.sql_type());
        if c.is_identity {
            line.push_str(" IDENTITY(1,1)");
        }
        line.push_str(if c.nullable { " NULL" } else { " NOT NULL" });
        lines.push(line);
    }
    if !pk.is_empty() {
        let cols_list = pk
            .iter()
            .map(|c| format!("[{}]", c))
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!(
            "    CONSTRAINT [PK_{}] PRIMARY KEY ({})",
            table.name, cols_list
        ));
    }
    out.push_str(&lines.join(",\n"));
    out.push_str("\n);\n");
    out
}
