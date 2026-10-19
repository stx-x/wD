//! 数据库备份（BACKUP DATABASE / LOG），生成 .bak 文件。

use anyhow::{anyhow, Context, Result};
use tiberius::Row;

use crate::db::conn::SqlClient;
use crate::db::export::TaskCtx;
use crate::model::{BackupKind, BackupSettings, Summary};

fn quote(s: &str) -> String {
    s.replace('\'', "''")
}

fn str_of(row: &Row, i: usize) -> String {
    row.try_get::<&str, _>(i)
        .ok()
        .flatten()
        .unwrap_or_default()
        .to_string()
}

/// 查询实例默认备份目录（服务器端路径）
pub async fn default_backup_dir(client: &mut SqlClient) -> Option<String> {
    let rows = client
        .query(
            "SELECT CAST(SERVERPROPERTY('InstanceDefaultBackupPath') AS nvarchar(400))",
            &[],
        )
        .await
        .ok()?
        .into_first_result()
        .await
        .ok()?;
    let p = rows.first().map(|r| str_of(r, 0)).unwrap_or_default();
    if p.trim().is_empty() {
        None
    } else {
        Some(p)
    }
}

pub fn build_file_name(db: &str, settings: &BackupSettings) -> String {
    if !settings.file_name.trim().is_empty() {
        let n = settings.file_name.trim();
        return if n.to_ascii_lowercase().ends_with(".bak")
            || n.to_ascii_lowercase().ends_with(".trn")
        {
            n.to_string()
        } else {
            format!("{}.bak", n)
        };
    }
    let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let ext = if settings.kind == BackupKind::Log {
        "trn"
    } else {
        "bak"
    };
    let safe_db: String = db
        .chars()
        .map(|c| if "/\\:*?\"<>| ".contains(c) { '_' } else { c })
        .collect();
    format!("{}_{}_{}.{}", safe_db, settings.kind.suffix(), stamp, ext)
}

fn join_server_path(dir: &str, file: &str) -> String {
    let d = dir.trim_end_matches(['/', '\\']);
    if d.is_empty() {
        file.to_string()
    } else {
        let sep = if d.contains('\\') { "\\" } else { "/" };
        format!("{}{}{}", d, sep, file)
    }
}

pub fn build_sql(db: &str, path: &str, settings: &BackupSettings) -> String {
    let mut with: Vec<String> = Vec::new();
    with.push(if settings.init {
        "INIT".to_string()
    } else {
        "NOINIT".to_string()
    });
    with.push(if settings.compress {
        "COMPRESSION".to_string()
    } else {
        "NO_COMPRESSION".to_string()
    });
    with.push(if settings.checksum {
        "CHECKSUM".to_string()
    } else {
        "NO_CHECKSUM".to_string()
    });
    if settings.copy_only {
        with.push("COPY_ONLY".to_string());
    }
    if settings.stats > 0 {
        with.push(format!("STATS = {}", settings.stats));
    }
    if !settings.description.trim().is_empty() {
        with.push(format!(
            "DESCRIPTION = N'{}'",
            quote(settings.description.trim())
        ));
    }

    match settings.kind {
        BackupKind::Full => format!(
            "BACKUP DATABASE [{}] TO DISK = N'{}' WITH {}",
            quote(db),
            quote(path),
            with.join(", ")
        ),
        BackupKind::Differential => format!(
            "BACKUP DATABASE [{}] TO DISK = N'{}' WITH DIFFERENTIAL, {}",
            quote(db),
            quote(path),
            with.join(", ")
        ),
        BackupKind::Log => format!(
            "BACKUP LOG [{}] TO DISK = N'{}' WITH {}",
            quote(db),
            quote(path),
            with.join(", ")
        ),
    }
}

/// 备份完成后读取文件大小
async fn backup_file_size(client: &mut SqlClient, path: &str) -> Option<i64> {
    let q = "SELECT CAST(size AS bigint) FROM sys.master_files WHERE physical_name = @P1";
    let rows = client
        .query(q, &[&path as &dyn tiberius::ToSql])
        .await
        .ok()?
        .into_first_result()
        .await
        .ok()?;
    rows.first()
        .and_then(|r| r.try_get::<i64, _>(0).ok().flatten())
}

pub async fn run(
    client: &mut SqlClient,
    settings: &BackupSettings,
    db: &str,
    ctx: &TaskCtx,
) -> Result<Summary> {
    if db.trim().is_empty() {
        return Err(anyhow!("未选择数据库"));
    }
    if settings.dir.trim().is_empty() {
        return Err(anyhow!("请填写服务器端备份目录"));
    }
    if settings.kind == BackupKind::Log {
        // 日志备份要求数据库为完整恢复模式
        let rows = client
            .query(
                "SELECT recovery_model_desc FROM sys.databases WHERE name = @P1",
                &[&db as &dyn tiberius::ToSql],
            )
            .await?
            .into_first_result()
            .await?;
        let model = rows.first().map(|r| str_of(r, 0)).unwrap_or_default();
        if model.eq_ignore_ascii_case("SIMPLE") {
            return Err(anyhow!(
                "数据库 [{}] 当前为 SIMPLE 恢复模式，无法进行事务日志备份。请先切换到 FULL 恢复模式。",
                db
            ));
        }
    }

    let file = build_file_name(db, settings);
    let path = join_server_path(&settings.dir, &file);
    let sql = build_sql(db, &path, settings);

    ctx.log(format!("▶ {}：{}", settings.kind.label(), path));
    ctx.log(format!("  SQL: {}", sql));

    let started = std::time::Instant::now();
    client
        .simple_query(sql)
        .await
        .with_context(|| {
            format!(
                "备份失败。请确认 SQL Server 服务账号对目录 `{}` 有写权限，且路径存在于服务器上。",
                settings.dir
            )
        })?
        .into_results()
        .await
        .context("备份执行失败")?;

    let elapsed = started.elapsed();
    ctx.progress(1.0, "备份完成", 0, 0);
    ctx.log(format!("✓ 备份完成，耗时 {:.1}s", elapsed.as_secs_f64()));

    let mut extra = String::new();
    if let Some(sz) = backup_file_size(client, &path).await {
        if sz > 0 {
            extra = format!("，约 {:.1} MB", sz as f64 / 1024.0 / 1024.0);
        }
    }

    if settings.verify {
        ctx.log("  正在校验备份…".to_string());
        let v = format!("RESTORE VERIFYONLY FROM DISK = N'{}'", quote(&path));
        match client.simple_query(v).await {
            Ok(s) => match s.into_results().await {
                Ok(_) => ctx.log("✓ 校验通过".to_string()),
                Err(e) => ctx.log(format!("⚠ 校验未通过：{}", e)),
            },
            Err(e) => ctx.log(format!("⚠ 校验未通过：{}", e)),
        }
    }

    Ok(Summary {
        ok: true,
        message: format!(
            "{} 完成：{}（耗时 {:.1}s{}）",
            settings.kind.label(),
            file,
            elapsed.as_secs_f64(),
            extra
        ),
    })
}

/// 最近备份历史
pub async fn history(client: &mut SqlClient, db: &str) -> Result<String> {
    let q = "SELECT TOP 15
                bs.type,
                CONVERT(nvarchar(19), bs.backup_finish_date, 120),
                CAST(bs.backup_size / 1048576 AS int),
                bmf.physical_device_name
             FROM msdb.dbo.backupset bs
             JOIN msdb.dbo.backupmediafamily bmf
               ON bmf.media_set_id = bs.media_set_id
             WHERE bs.database_name = @P1
             ORDER BY bs.backup_finish_date DESC";
    let rows = client
        .query(q, &[&db as &dyn tiberius::ToSql])
        .await?
        .into_first_result()
        .await?;

    if rows.is_empty() {
        return Ok("（暂无备份记录）".to_string());
    }
    let mut out = String::from("类型  完成时间              大小      文件\n");
    out.push_str("──────────────────────────────────────────────────────────────\n");
    for r in rows.iter() {
        let t = str_of(r, 0);
        let kind = match t.trim() {
            "D" => "差异",
            "L" => "日志",
            _ => "完整",
        };
        let when = str_of(r, 1);
        let mb = r.try_get::<i32, _>(2).ok().flatten().unwrap_or(0);
        let dev = str_of(r, 3);
        out.push_str(&format!("{:<6}{:<22}{:>6} MB  {}\n", kind, when, mb, dev));
    }
    Ok(out)
}
