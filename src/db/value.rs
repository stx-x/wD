//! 值读取：把 TDS 返回的 `ColumnData` 统一转换为 `Cell`。

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use tiberius::numeric::Numeric;
use tiberius::{ColumnData, FromSql, Row};

use crate::model::{hex_encode, Cell, ColumnMeta};

fn numeric_to_string(n: Numeric) -> String {
    let scale = n.scale() as usize;
    let v = n.value();
    if scale == 0 {
        return v.to_string();
    }
    let neg = v < 0;
    let abs = v.unsigned_abs();
    let digits = abs.to_string();
    let body = if digits.len() <= scale {
        format!("0.{}{}", "0".repeat(scale - digits.len()), digits)
    } else {
        let split = digits.len() - scale;
        format!("{}.{}", &digits[..split], &digits[split..])
    };
    if neg {
        format!("-{}", body)
    } else {
        body
    }
}

/// 把一个 TDS 值转成通用 Cell
pub fn cell_to_value(data: &ColumnData<'static>) -> Cell {
    match data {
        ColumnData::U8(v) => v.map(|x| Cell::Text(x.to_string())).unwrap_or(Cell::Null),
        ColumnData::I16(v) => v.map(|x| Cell::Text(x.to_string())).unwrap_or(Cell::Null),
        ColumnData::I32(v) => v.map(|x| Cell::Text(x.to_string())).unwrap_or(Cell::Null),
        ColumnData::I64(v) => v.map(|x| Cell::Text(x.to_string())).unwrap_or(Cell::Null),
        ColumnData::F32(v) => v.map(|x| Cell::Text(fmt_f32(x))).unwrap_or(Cell::Null),
        ColumnData::F64(v) => v.map(|x| Cell::Text(fmt_f64(x))).unwrap_or(Cell::Null),
        ColumnData::Bit(v) => v
            .map(|b| Cell::Text(if b { "1".into() } else { "0".into() }))
            .unwrap_or(Cell::Null),
        ColumnData::String(v) => v
            .as_ref()
            .map(|s| Cell::Text(s.to_string()))
            .unwrap_or(Cell::Null),
        ColumnData::Guid(v) => v.map(|g| Cell::Text(g.to_string())).unwrap_or(Cell::Null),
        ColumnData::Binary(v) => v
            .as_ref()
            .map(|b| Cell::Bytes(b.to_vec()))
            .unwrap_or(Cell::Null),
        ColumnData::Numeric(v) => v
            .map(|n| Cell::Text(numeric_to_string(n)))
            .unwrap_or(Cell::Null),
        ColumnData::Xml(v) => v
            .as_ref()
            .map(|x| Cell::Text(x.to_string()))
            .unwrap_or(Cell::Null),
        ColumnData::DateTime(_)
        | ColumnData::SmallDateTime(_)
        | ColumnData::DateTime2(_)
        | ColumnData::Date(_)
        | ColumnData::Time(_) => temporal_to_cell(data),
        ColumnData::DateTimeOffset(_) => DateTime::<Utc>::from_sql(data)
            .ok()
            .flatten()
            .map(|dt| Cell::Text(dt.format("%Y-%m-%d %H:%M:%S%.f %:z").to_string()))
            .unwrap_or(Cell::Null),
    }
}

fn temporal_to_cell(data: &ColumnData<'static>) -> Cell {
    if let Ok(Some(dt)) = NaiveDateTime::from_sql(data) {
        return Cell::Text(dt.format("%Y-%m-%d %H:%M:%S%.f").to_string());
    }
    if let Ok(Some(d)) = NaiveDate::from_sql(data) {
        return Cell::Text(d.format("%Y-%m-%d").to_string());
    }
    if let Ok(Some(t)) = NaiveTime::from_sql(data) {
        return Cell::Text(t.format("%H:%M:%S%.f").to_string());
    }
    Cell::Null
}

fn fmt_f32(v: f32) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        format!("{:.1}", v)
    } else {
        format!("{}", v)
    }
}

fn fmt_f64(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        format!("{:.1}", v)
    } else {
        format!("{}", v)
    }
}

pub fn row_to_cells(row: &Row) -> Vec<Cell> {
    row.cells().map(|(_, data)| cell_to_value(data)).collect()
}

/// JSON 输出：数值/布尔保持原生类型，其余按字符串
pub fn cell_to_json(cell: &Cell, col: Option<&ColumnMeta>) -> serde_json::Value {
    use serde_json::Value;
    match cell {
        Cell::Null => Value::Null,
        Cell::Bytes(b) => Value::String(hex_encode(b)),
        Cell::Text(s) => {
            if let Some(col) = col {
                if col.is_bool() {
                    return Value::Bool(s == "1" || s.eq_ignore_ascii_case("true"));
                }
                if col.is_numeric() {
                    if let Ok(i) = s.parse::<i64>() {
                        return Value::Number(i.into());
                    }
                    if let Ok(f) = s.parse::<f64>() {
                        if let Some(n) = serde_json::Number::from_f64(f) {
                            return Value::Number(n);
                        }
                    }
                }
            }
            Value::String(s.clone())
        }
    }
}

/// SQL 脚本输出：生成字面量
pub fn cell_to_sql_literal(cell: &Cell, col: Option<&ColumnMeta>) -> String {
    match cell {
        Cell::Null => "NULL".to_string(),
        Cell::Bytes(b) => {
            if b.is_empty() {
                "0x".to_string()
            } else {
                hex_encode(b)
            }
        }
        Cell::Text(s) => {
            if let Some(col) = col {
                if col.is_bool() {
                    return if s == "1" || s.eq_ignore_ascii_case("true") {
                        "1".into()
                    } else {
                        "0".into()
                    };
                }
                if col.is_numeric() && !s.is_empty() && is_plain_number(s) {
                    return s.clone();
                }
            }
            if s.is_empty() {
                "N''".to_string()
            } else {
                format!("N'{}'", s.replace('\'', "''"))
            }
        }
    }
}

fn is_plain_number(s: &str) -> bool {
    let mut has_digit = false;
    for (i, c) in s.chars().enumerate() {
        match c {
            '-' | '+' if i == 0 => {}
            '0'..='9' => has_digit = true,
            '.' | 'e' | 'E' => {}
            _ => return false,
        }
    }
    has_digit
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ColumnMeta;

    fn col(t: &str) -> ColumnMeta {
        ColumnMeta {
            name: "c".into(),
            type_name: t.into(),
            max_length: 10,
            precision: 18,
            scale: 2,
            nullable: true,
            is_identity: false,
        }
    }

    #[test]
    fn sql_literal_escapes_single_quote() {
        assert_eq!(
            cell_to_sql_literal(&Cell::Text("O'Brien".into()), Some(&col("nvarchar"))),
            "N'O''Brien'"
        );
    }

    #[test]
    fn sql_literal_keeps_numeric_raw() {
        assert_eq!(
            cell_to_sql_literal(&Cell::Text("-12.50".into()), Some(&col("decimal"))),
            "-12.50"
        );
    }

    #[test]
    fn sql_literal_null_and_bytes() {
        assert_eq!(cell_to_sql_literal(&Cell::Null, None), "NULL");
        assert_eq!(
            cell_to_sql_literal(&Cell::Bytes(vec![0xDE, 0xAD]), None),
            "0xDEAD"
        );
    }

    #[test]
    fn sql_literal_bit() {
        assert_eq!(
            cell_to_sql_literal(&Cell::Text("1".into()), Some(&col("bit"))),
            "1"
        );
        assert_eq!(
            cell_to_sql_literal(&Cell::Text("0".into()), Some(&col("bit"))),
            "0"
        );
    }

    #[test]
    fn json_keeps_numeric_type() {
        assert_eq!(
            cell_to_json(&Cell::Text("42".into()), Some(&col("int"))),
            serde_json::json!(42)
        );
        assert_eq!(
            cell_to_json(&Cell::Text("1".into()), Some(&col("bit"))),
            serde_json::json!(true)
        );
        assert_eq!(
            cell_to_json(&Cell::Null, Some(&col("int"))),
            serde_json::Value::Null
        );
    }

    #[test]
    fn numeric_render_keeps_scale() {
        let n = Numeric::new_with_scale(12345, 2);
        assert_eq!(numeric_to_string(n), "123.45");
        let n2 = Numeric::new_with_scale(-5, 1);
        assert_eq!(numeric_to_string(n2), "-0.5");
        let n3 = Numeric::new_with_scale(7, 0);
        assert_eq!(numeric_to_string(n3), "7");
    }
}
