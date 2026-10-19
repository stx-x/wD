//! SQL Server 连接管理（基于 tiberius 纯 Rust TDS 驱动）。

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use crate::model::{AuthKind, ConnSettings, EncryptMode};

pub type SqlClient = Client<Compat<TcpStream>>;

fn build_config(s: &ConnSettings) -> Result<Config> {
    let mut cfg = Config::new();
    cfg.host(s.server.trim());
    cfg.port(if s.port == 0 { 1433 } else { s.port });
    if !s.instance.trim().is_empty() {
        cfg.instance_name(s.instance.trim());
    }
    let db = if s.database.trim().is_empty() {
        "master"
    } else {
        s.database.trim()
    };
    cfg.database(db);
    cfg.application_name(if s.app_name.trim().is_empty() {
        "MSSQL Transfer Studio"
    } else {
        s.app_name.trim()
    });
    cfg.encryption(match s.encrypt {
        EncryptMode::Optional => EncryptionLevel::On,
        EncryptMode::Required => EncryptionLevel::Required,
    });
    if s.trust_cert {
        cfg.trust_cert();
    }
    let timeout = s.timeout_secs.max(3);
    cfg.handshake_timeout(Some(Duration::from_secs(timeout)));
    cfg.command_timeout(None);

    match s.auth {
        AuthKind::Sql => {
            cfg.authentication(AuthMethod::sql_server(&s.username, &s.password));
        }
        AuthKind::Windows => {
            #[cfg(windows)]
            {
                cfg.authentication(AuthMethod::windows(&s.username, &s.password));
            }
            #[cfg(not(windows))]
            {
                return Err(anyhow!(
                    "Windows 集成验证仅在 Windows 平台可用。当前平台请使用 SQL Server 身份验证。"
                ));
            }
        }
    }
    Ok(cfg)
}

pub async fn connect(s: &ConnSettings) -> Result<SqlClient> {
    if s.server.trim().is_empty() {
        return Err(anyhow!("请填写服务器地址"));
    }
    let cfg = build_config(s)?;
    let addr = cfg.get_addr();
    let timeout = Duration::from_secs(s.timeout_secs.max(3));

    let tcp = tokio::time::timeout(timeout, TcpStream::connect(&addr))
        .await
        .map_err(|_| anyhow!("连接 {} 超时（{} 秒）", addr, timeout.as_secs()))?
        .with_context(|| {
            format!(
                "无法连接到 {}，请检查地址、端口与 SQL Server 服务状态",
                addr
            )
        })?;
    tcp.set_nodelay(true).ok();

    let client = Client::connect(cfg, tcp.compat_write())
        .await
        .with_context(|| format!("登录失败（{}）。请检查账号密码、数据库名与加密设置", addr))?;
    Ok(client)
}

/// 读取服务器版本信息，用于连接成功后展示
pub async fn server_info(client: &mut SqlClient) -> Result<String> {
    let rows = client
        .query(
            "SELECT CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)),
                    CAST(SERVERPROPERTY('Edition') AS nvarchar(128)),
                    CAST(@@SERVERNAME AS nvarchar(128))",
            &[],
        )
        .await?
        .into_first_result()
        .await?;

    if let Some(row) = rows.first() {
        let ver = row.try_get::<&str, _>(0).ok().flatten().unwrap_or("?");
        let ed = row.try_get::<&str, _>(1).ok().flatten().unwrap_or("?");
        let name = row.try_get::<&str, _>(2).ok().flatten().unwrap_or("?");
        let short_ed = ed.split(" (").next().unwrap_or(ed);
        Ok(format!("{} · SQL Server {} · {}", name, ver, short_ed))
    } else {
        Ok(String::new())
    }
}
