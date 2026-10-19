//! MSSQL Transfer Studio — SQL Server 数据库/表 导入导出工具
//! Rust + Slint

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

slint::include_modules!();

mod bridge;
mod config;
mod db;
mod model;

use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use slint::{Model, ModelRc, VecModel};

use model::{
    AuthKind, BackupKind, BackupSettings, Cmd, ColumnMode, ConnSettings, EncryptMode,
    ExportContent, ExportFormat, ExportSettings, FileScope, ImportFormat, ImportPlanItem,
    ImportSettings, OnExisting, SchemaFormat, SchemaSettings, TableRef, UiEvent,
};

#[derive(Default)]
pub struct UiState {
    pub all: Vec<(TableRef, i64)>,
    pub selected: HashSet<String>,
    /// 当前导入计划（UI 线程维护）
    pub plan: Vec<ImportPlanItem>,
}

pub type SharedState = Arc<Mutex<UiState>>;

fn main() -> Result<(), slint::PlatformError> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("mssql-transfer {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if let Some(pos) = args.iter().position(|a| a == "--sample-schema") {
        let dir = args
            .get(pos + 1)
            .cloned()
            .unwrap_or_else(|| "schema-sample".into());
        return dump_sample_schema(&dir).map_err(|e| slint::PlatformError::Other(e.to_string()));
    }
    if let Some(pos) = args.iter().position(|a| a == "--screenshot") {
        let path = args
            .get(pos + 1)
            .cloned()
            .unwrap_or_else(|| "ui-preview.png".into());
        return render_preview(&path);
    }
    let demo = args.iter().any(|a| a == "--demo");

    let ui = AppWindow::new()?;
    let cfg = config::load();
    apply_config(&ui, &cfg);
    restore_window(&ui, &cfg);
    if demo {
        fill_demo(&ui);
    }

    let state: SharedState = Arc::new(Mutex::new(UiState::default()));
    let bridge = Rc::new(bridge::spawn(ui.as_weak(), state.clone()));

    wire(&ui, &bridge, &state);

    let res = ui.run();
    config::save(&persist_from(&ui));
    res
}

// ---------------------------------------------------------------- 示例结构

/// 用内置示例快照输出 5 种格式的样例文件（无需数据库）
fn dump_sample_schema(dir: &str) -> std::io::Result<()> {
    let snap = db::schema::demo_snapshot();
    let st = SchemaSettings::default();
    let p = std::path::Path::new(dir);
    std::fs::create_dir_all(p)?;
    let files = [
        (
            "sample-ai-context.md",
            db::schema::render_ai_context(&snap, &st),
        ),
        ("sample-schema.md", db::schema::render_markdown(&snap, &st)),
        ("sample-schema.sql", db::schema::render_ddl(&snap, &st)),
        ("sample-schema.json", db::schema::render_json(&snap, &st)),
        ("sample-er.mmd", db::schema::render_mermaid(&snap, &st)),
    ];
    for (name, body) in files {
        let path = p.join(name);
        std::fs::write(&path, body)?;
        println!("saved {}", path.display());
    }
    Ok(())
}

// ---------------------------------------------------------------- 窗口尺寸

fn restore_window(ui: &AppWindow, c: &config::Persist) {
    if c.window_w >= 400.0 && c.window_h >= 300.0 {
        ui.window()
            .set_size(slint::LogicalSize::new(c.window_w, c.window_h));
    }
}

fn current_window_size(ui: &AppWindow) -> (f32, f32) {
    let sf = ui.window().scale_factor();
    let s = ui.window().size();
    let sf = if sf > 0.1 { sf } else { 1.0 };
    (s.width as f32 / sf, s.height as f32 / sf)
}

// ---------------------------------------------------------------- 预览渲染

struct PreviewPlatform {
    window: Rc<slint::platform::software_renderer::MinimalSoftwareWindow>,
}

impl slint::platform::Platform for PreviewPlatform {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }
}

fn render_preview(path: &str) -> Result<(), slint::PlatformError> {
    use slint::platform::software_renderer::{
        MinimalSoftwareWindow, RepaintBufferType, Rgb565Pixel,
    };

    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(PreviewPlatform {
        window: window.clone(),
    }))
    .map_err(|e| slint::PlatformError::Other(e.to_string()))?;

    let ui = AppWindow::new()?;
    let cfg = config::load();
    apply_config(&ui, &cfg);
    fill_demo(&ui);
    ui.show()?;

    let (w, h) = (1320usize, 880usize);
    window.set_size(slint::PhysicalSize::new(w as u32, h as u32));

    for page in 0..7 {
        ui.set_nav_index(page);
        let mut buffer = vec![Rgb565Pixel(0); w * h];
        for _ in 0..5 {
            slint::platform::update_timers_and_animations();
            window.draw_if_needed(|renderer| {
                renderer.render(&mut buffer, w);
            });
        }

        let mut rgb = Vec::with_capacity(w * h * 3);
        for px in &buffer {
            let v = px.0;
            let r = ((v >> 11) & 0x1f) as u8;
            let g = ((v >> 5) & 0x3f) as u8;
            let b = (v & 0x1f) as u8;
            rgb.push((r << 3) | (r >> 2));
            rgb.push((g << 2) | (g >> 4));
            rgb.push((b << 3) | (b >> 2));
        }

        let out = if page == 0 {
            path.to_string()
        } else {
            path.replace(".png", &format!("-{}.png", page))
        };
        let file = std::fs::File::create(&out).expect("无法创建截图文件");
        let wtr = std::io::BufWriter::new(file);
        let mut enc = png::Encoder::new(wtr, w as u32, h as u32);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().expect("PNG 头写入失败");
        writer.write_image_data(&rgb).expect("PNG 数据写入失败");
        println!("saved {}", out);
    }
    Ok(())
}

fn fill_demo(ui: &AppWindow) {
    ui.set_connected(true);
    ui.set_status_text("已连接".into());
    ui.set_server_info("SQLPROD-01 · SQL Server 16.0.4135 · Enterprise".into());
    ui.set_databases(ModelRc::new(VecModel::from(vec![
        slint::SharedString::from("SalesDB"),
        slint::SharedString::from("Warehouse"),
        slint::SharedString::from("master"),
    ])));
    ui.set_db_index(0);
    ui.set_database("SalesDB".into());
    ui.set_export_dir("/Users/kk/Downloads/sql-export".into());
    ui.set_import_path("/Users/kk/Downloads/inbox".into());
    ui.set_import_target("dbo.Customers".into());
    ui.set_import_glob("*.csv".into());
    ui.set_backup_dir("/var/opt/mssql/backup".into());
    ui.set_schema_dir("/Users/kk/Downloads/db-context".into());
    ui.set_schema_prompt("你是 T-SQL 专家，请基于以下结构编写查询。".into());
    ui.set_schema_preview(
        "# 数据库结构上下文：SalesDB\n\
         \n\
         ## 1. 数据库概览\n\
         | 项 | 值 |\n\
         |---|---|\n\
         | 数据库 | `SalesDB` |\n\
         | SQL Server 版本 | 16.0.4135 Enterprise |\n\
         | 表 | 6 个 |\n\
         | 数据总行数（估算） | 1,720,593 |\n\
         \n\
         ## 4. 表结构详解\n\
         ### `dbo.Customers`\n\
         行数约 12,480 · 4 列 · 主键 (Id)\n\
         | 列 | 类型 | 可空 | 键 | 说明 |\n\
         |---|---|---|---|---|\n\
         | `Id` | int | 否 | PK IDENTITY | 主键 |\n\
         | `Name` | nvarchar(50) | 否 | — | 客户名称 |\n\
         | `Email` | nvarchar(100) | 是 | — | — |\n\
         | `Balance` | decimal(18,2) | 是 | — | 账户余额 |"
            .into(),
    );
    ui.set_backup_desc("nightly full backup".into());
    ui.set_import_preview(
        "Id | Name | Email | CreatedAt\n\
         ----------------------------------------\n\
         1 | 张伟 | zhang@example.com | 2024-03-11 09:12:00\n\
         2 | 李娜 | li@example.com | 2024-03-12 14:05:31\n\
         3 | 王强 | wang@example.com | 2024-03-13 08:44:12"
            .into(),
    );
    ui.set_backup_info(
        "类型  完成时间              大小      文件\n\
         ──────────────────────────────────────────────────────────────\n\
         完整  2024-05-11 02:00:13     412 MB  /var/opt/mssql/backup/SalesDB_full_20240511.bak\n\
         差异  2024-05-11 14:00:08      38 MB  /var/opt/mssql/backup/SalesDB_diff_20240511.bak\n\
         日志  2024-05-11 18:00:02       2 MB  /var/opt/mssql/backup/SalesDB_log_20240511.trn"
            .into(),
    );

    let rows: Vec<(&str, &str, i32, bool)> = vec![
        ("dbo.Customers", "table", 12_480, true),
        ("dbo.Orders", "table", 89_301, true),
        ("dbo.OrderItems", "table", 412_776, true),
        ("dbo.Products", "table", 2_105, false),
        ("dbo.Categories", "table", 42, false),
        ("dbo.vOrderSummary", "view", 0, false),
        ("dbo.vCustomerLTV", "view", 0, false),
        ("dbo.AuditLog", "table", 1_204_889, false),
    ];
    let items: Vec<ObjectItem> = rows
        .into_iter()
        .map(|(name, kind, rows, checked)| {
            let (schema, table) = name.split_once('.').unwrap_or(("dbo", name));
            ObjectItem {
                display: name.into(),
                kind: kind.into(),
                rows,
                rows_text: if rows > 0 {
                    fmt_thousands(rows as i64).into()
                } else {
                    "".into()
                },
                checked,
                schema: schema.into(),
                name: table.into(),
            }
        })
        .collect();
    ui.set_objects(ModelRc::new(VecModel::from(items)));
    ui.set_selected_count(3);
    ui.set_total_count(8);

    let plan: Vec<PlanItem> = vec![
        ("customers_2024.csv", "CSV", "dbo.customers_2024", true),
        ("orders_2024.csv", "CSV", "dbo.orders_2024", true),
        ("OrderItems.tsv", "TSV", "dbo.OrderItems", true),
        ("Products.xlsx · Sheet1", "Excel", "dbo.Products", true),
        ("Categories.xlsx · Sheet1", "Excel", "dbo.Categories", true),
        ("AuditLog.json", "JSON", "dbo.AuditLog", false),
    ]
    .into_iter()
    .map(|(note, format, target, enabled)| PlanItem {
        note: note.into(),
        format: format.into(),
        target: target.into(),
        sheet: "".into(),
        enabled,
    })
    .collect();
    ui.set_plan_count(5);
    ui.set_import_plan(ModelRc::new(VecModel::from(plan)));

    ui.set_progress(0.0);
    ui.set_progress_text("就绪 · 已选择 3 个对象，可开始导出".into());
    ui.set_rows_text("".into());
    ui.set_bytes_text("".into());
    ui.set_running(false);
    ui.set_log_text(
        "MSSQL Transfer Studio\n\
         ────────────────────────────────────────\n\
         ✓ 已连接 SQLPROD-01 · SQL Server 16.0.4135 · Enterprise\n\
         读取到 8 个表 / 视图\n\
         ━━ 开始导出 3 个对象 ━━\n\
         → /Users/kk/Downloads/sql-export/dbo.Customers.csv\n\
         ✓ dbo.Customers · 12,480 行（预估 12,480）\n\
         → /Users/kk/Downloads/sql-export/dbo.Orders.csv\n\
         ✓ dbo.Orders · 89,301 行（预估 89,301）\n\
         → /Users/kk/Downloads/sql-export/dbo.OrderItems.csv\n"
            .into(),
    );
}

// ---------------------------------------------------------------- 配置 <-> UI

fn apply_config(ui: &AppWindow, c: &config::Persist) {
    ui.set_server(c.server.clone().into());
    ui.set_port(c.port.clone().into());
    ui.set_instance(c.instance.clone().into());
    ui.set_auth_index(c.auth_index);
    ui.set_username(c.username.clone().into());
    ui.set_database(c.database.clone().into());
    ui.set_encrypt_index(c.encrypt_index);
    ui.set_trust_cert(c.trust_cert);
    ui.set_conn_timeout(c.conn_timeout.clone().into());

    ui.set_export_dir(c.export_dir.clone().into());
    ui.set_export_format(c.export_format);
    ui.set_export_scope(c.export_scope);
    ui.set_export_what(c.export_what);
    ui.set_export_batch(c.export_batch.clone().into());
    ui.set_export_null(c.export_null.clone().into());
    ui.set_export_delimiter(c.export_delimiter);
    ui.set_export_header(c.export_header);
    ui.set_export_drop(c.export_drop);
    ui.set_export_identity(c.export_identity);
    ui.set_export_quote_all(c.export_quote_all);
    ui.set_export_bom(c.export_bom);
    ui.set_export_create_db(c.export_create_db);
    ui.set_export_schema_qualified(c.export_schema_qualified);
    ui.set_export_file_prefix(c.export_prefix.clone().into());
    ui.set_export_where(c.export_where.clone().into());
    ui.set_export_limit(c.export_limit.clone().into());
    ui.set_export_exclude(c.export_exclude.clone().into());
    ui.set_export_gzip(c.export_gzip);
    ui.set_excel_freeze(c.excel_freeze);
    ui.set_excel_autofit(c.excel_autofit);
    ui.set_excel_bold(c.excel_bold);

    ui.set_backup_dir(c.backup_dir.clone().into());
    ui.set_backup_file(c.backup_file.clone().into());
    ui.set_backup_kind(c.backup_kind);
    ui.set_backup_compress(c.backup_compress);
    ui.set_backup_checksum(c.backup_checksum);
    ui.set_backup_init(c.backup_init);
    ui.set_backup_copy_only(c.backup_copy_only);
    ui.set_backup_desc(c.backup_desc.clone().into());
    ui.set_backup_stats(c.backup_stats.clone().into());
    ui.set_backup_verify(c.backup_verify);

    ui.set_import_path(c.import_path.clone().into());
    ui.set_import_format(c.import_format);
    ui.set_import_target(c.import_target.clone().into());
    ui.set_import_batch(c.import_batch.clone().into());
    ui.set_import_truncate(c.import_truncate);
    ui.set_import_transaction(c.import_transaction);
    ui.set_import_skip_errors(c.import_skip_errors);
    ui.set_import_create_table(c.import_create_table);
    ui.set_import_column_mode(c.import_column_mode);
    ui.set_import_null(c.import_null.clone().into());
    ui.set_import_header(c.import_header);
    ui.set_import_max_errors(c.import_max_errors.clone().into());
    ui.set_import_recursive(c.import_recursive);
    ui.set_import_glob(c.import_glob.clone().into());
    ui.set_import_strip_prefix(c.import_strip_prefix.clone().into());
    ui.set_import_strip_suffix(c.import_strip_suffix.clone().into());
    ui.set_import_schema(c.import_schema.clone().into());
    ui.set_import_on_existing(c.import_on_existing);
    ui.set_import_all_sheets(c.import_all_sheets);

    ui.set_schema_dir(c.schema_dir.clone().into());
    ui.set_schema_format(c.schema_format);
    ui.set_schema_use_selection(c.schema_use_selection);
    ui.set_schema_samples(c.schema_samples.clone().into());
    ui.set_schema_max_len(c.schema_max_len.clone().into());
    ui.set_schema_pk(c.schema_pk);
    ui.set_schema_fk(c.schema_fk);
    ui.set_schema_idx(c.schema_idx);
    ui.set_schema_checks(c.schema_checks);
    ui.set_schema_defaults(c.schema_defaults);
    ui.set_schema_desc(c.schema_desc);
    ui.set_schema_rows(c.schema_rows);
    ui.set_schema_views(c.schema_views);
    ui.set_schema_procs(c.schema_procs);
    ui.set_schema_triggers(c.schema_triggers);
    ui.set_schema_seq(c.schema_seq);
    ui.set_schema_system(c.schema_system);
    ui.set_schema_split(c.schema_split);
    ui.set_schema_prompt(c.schema_prompt.clone().into());
    ui.set_schema_filename(c.schema_filename.clone().into());

    ui.set_nav_index(c.nav_index.clamp(0, 6));
    ui.set_log_text(logo().into());
}

fn logo() -> String {
    "MSSQL Transfer Studio\n\
     SQL Server 数据库 / 表 · 导出 · 备份 · 导入\n\
     ────────────────────────────────────────\n\
     · 左侧按功能分组导航，右侧配置面板，窗口可自由缩放\n\
     · 导出：CSV / TSV / JSON / NDJSON / SQL / Excel（可 gzip）\n\
     · 备份：完整 / 差异 / 事务日志，支持压缩、校验与历史查询\n\
     · 导入：CSV / TSV / JSON / Excel，递归扫描 + 多格式混排 + 多 Sheet 拆分\n"
        .to_string()
}

fn persist_from(ui: &AppWindow) -> config::Persist {
    let (w, h) = current_window_size(ui);
    config::Persist {
        server: ui.get_server().to_string(),
        port: ui.get_port().to_string(),
        instance: ui.get_instance().to_string(),
        auth_index: ui.get_auth_index(),
        username: ui.get_username().to_string(),
        database: ui.get_database().to_string(),
        encrypt_index: ui.get_encrypt_index(),
        trust_cert: ui.get_trust_cert(),
        conn_timeout: ui.get_conn_timeout().to_string(),
        export_dir: ui.get_export_dir().to_string(),
        export_format: ui.get_export_format(),
        export_scope: ui.get_export_scope(),
        export_what: ui.get_export_what(),
        export_batch: ui.get_export_batch().to_string(),
        export_null: ui.get_export_null().to_string(),
        export_delimiter: ui.get_export_delimiter(),
        export_header: ui.get_export_header(),
        export_drop: ui.get_export_drop(),
        export_identity: ui.get_export_identity(),
        export_quote_all: ui.get_export_quote_all(),
        export_bom: ui.get_export_bom(),
        export_create_db: ui.get_export_create_db(),
        export_schema_qualified: ui.get_export_schema_qualified(),
        export_prefix: ui.get_export_file_prefix().to_string(),
        export_where: ui.get_export_where().to_string(),
        export_limit: ui.get_export_limit().to_string(),
        export_exclude: ui.get_export_exclude().to_string(),
        export_gzip: ui.get_export_gzip(),
        excel_freeze: ui.get_excel_freeze(),
        excel_autofit: ui.get_excel_autofit(),
        excel_bold: ui.get_excel_bold(),
        backup_dir: ui.get_backup_dir().to_string(),
        backup_file: ui.get_backup_file().to_string(),
        backup_kind: ui.get_backup_kind(),
        backup_compress: ui.get_backup_compress(),
        backup_checksum: ui.get_backup_checksum(),
        backup_init: ui.get_backup_init(),
        backup_copy_only: ui.get_backup_copy_only(),
        backup_desc: ui.get_backup_desc().to_string(),
        backup_stats: ui.get_backup_stats().to_string(),
        backup_verify: ui.get_backup_verify(),
        import_path: ui.get_import_path().to_string(),
        import_format: ui.get_import_format(),
        import_target: ui.get_import_target().to_string(),
        import_batch: ui.get_import_batch().to_string(),
        import_truncate: ui.get_import_truncate(),
        import_transaction: ui.get_import_transaction(),
        import_skip_errors: ui.get_import_skip_errors(),
        import_create_table: ui.get_import_create_table(),
        import_column_mode: ui.get_import_column_mode(),
        import_null: ui.get_import_null().to_string(),
        import_header: ui.get_import_header(),
        import_max_errors: ui.get_import_max_errors().to_string(),
        import_recursive: ui.get_import_recursive(),
        import_glob: ui.get_import_glob().to_string(),
        import_strip_prefix: ui.get_import_strip_prefix().to_string(),
        import_strip_suffix: ui.get_import_strip_suffix().to_string(),
        import_schema: ui.get_import_schema().to_string(),
        import_on_existing: ui.get_import_on_existing(),
        import_all_sheets: ui.get_import_all_sheets(),
        schema_dir: ui.get_schema_dir().to_string(),
        schema_format: ui.get_schema_format(),
        schema_use_selection: ui.get_schema_use_selection(),
        schema_samples: ui.get_schema_samples().to_string(),
        schema_max_len: ui.get_schema_max_len().to_string(),
        schema_pk: ui.get_schema_pk(),
        schema_fk: ui.get_schema_fk(),
        schema_idx: ui.get_schema_idx(),
        schema_checks: ui.get_schema_checks(),
        schema_defaults: ui.get_schema_defaults(),
        schema_desc: ui.get_schema_desc(),
        schema_rows: ui.get_schema_rows(),
        schema_views: ui.get_schema_views(),
        schema_procs: ui.get_schema_procs(),
        schema_triggers: ui.get_schema_triggers(),
        schema_seq: ui.get_schema_seq(),
        schema_system: ui.get_schema_system(),
        schema_split: ui.get_schema_split(),
        schema_prompt: ui.get_schema_prompt().to_string(),
        schema_filename: ui.get_schema_filename().to_string(),
        nav_index: ui.get_nav_index(),
        window_w: w,
        window_h: h,
    }
}

// ---------------------------------------------------------------- 参数收集

fn conn_from(ui: &AppWindow) -> ConnSettings {
    ConnSettings {
        server: ui.get_server().to_string(),
        port: ui.get_port().trim().parse().unwrap_or(1433),
        instance: ui.get_instance().to_string(),
        auth: if ui.get_auth_index() == 1 {
            AuthKind::Windows
        } else {
            AuthKind::Sql
        },
        username: ui.get_username().to_string(),
        password: ui.get_password().to_string(),
        database: ui.get_database().to_string(),
        encrypt: if ui.get_encrypt_index() == 1 {
            EncryptMode::Required
        } else {
            EncryptMode::Optional
        },
        trust_cert: ui.get_trust_cert(),
        timeout_secs: ui.get_conn_timeout().trim().parse().unwrap_or(15),
        app_name: ui.get_app_name().to_string(),
    }
}

fn export_from(ui: &AppWindow) -> ExportSettings {
    ExportSettings {
        dir: PathBuf::from(ui.get_export_dir().to_string()),
        format: ExportFormat::from_index(ui.get_export_format()),
        scope: FileScope::from_index(ui.get_export_scope()),
        content: ExportContent::from_index(ui.get_export_what()),
        batch: ui
            .get_export_batch()
            .trim()
            .parse()
            .unwrap_or(1000usize)
            .clamp(1, 100_000),
        null_text: ui.get_export_null().to_string(),
        delimiter: match ui.get_export_delimiter() {
            1 => b'\t',
            2 => b';',
            3 => b'|',
            _ => b',',
        },
        header: ui.get_export_header(),
        quote_all: ui.get_export_quote_all(),
        bom: ui.get_export_bom(),
        drop_table: ui.get_export_drop(),
        identity_insert: ui.get_export_identity(),
        create_database: ui.get_export_create_db(),
        schema_qualified: ui.get_export_schema_qualified(),
        file_prefix: ui.get_export_file_prefix().to_string(),
        where_clause: ui.get_export_where().to_string(),
        limit_rows: ui.get_export_limit().trim().parse().unwrap_or(0u64),
        exclude_columns: ui.get_export_exclude().to_string(),
        gzip: ui.get_export_gzip() && ui.get_export_format() != 6,
        excel_freeze: ui.get_excel_freeze(),
        excel_autofit: ui.get_excel_autofit(),
        excel_bold_header: ui.get_excel_bold(),
    }
}

fn backup_from(ui: &AppWindow) -> BackupSettings {
    BackupSettings {
        dir: ui.get_backup_dir().to_string(),
        file_name: ui.get_backup_file().to_string(),
        kind: BackupKind::from_index(ui.get_backup_kind()),
        compress: ui.get_backup_compress(),
        checksum: ui.get_backup_checksum(),
        init: ui.get_backup_init(),
        copy_only: ui.get_backup_copy_only(),
        description: ui.get_backup_desc().to_string(),
        stats: ui.get_backup_stats().trim().parse().unwrap_or(10u32),
        verify: ui.get_backup_verify(),
    }
}

fn import_from(ui: &AppWindow) -> ImportSettings {
    let raw = ui.get_import_target().to_string();
    let (schema_inline, name) = match raw.split_once('.') {
        Some((s, n)) => (s.trim().to_string(), n.trim().to_string()),
        None => (String::new(), raw.trim().to_string()),
    };
    let schema = if schema_inline.is_empty() {
        let s = ui.get_import_schema().trim().to_string();
        if s.is_empty() {
            "dbo".to_string()
        } else {
            s
        }
    } else {
        schema_inline
    };
    ImportSettings {
        path: PathBuf::from(ui.get_import_path().to_string()),
        format: ImportFormat::from_index(ui.get_import_format()),
        target: TableRef {
            schema,
            name,
            kind: "table".into(),
        },
        batch: ui
            .get_import_batch()
            .trim()
            .parse()
            .unwrap_or(500usize)
            .clamp(1, 100_000),
        truncate: ui.get_import_truncate(),
        transaction: ui.get_import_transaction(),
        skip_errors: ui.get_import_skip_errors(),
        create_table: ui.get_import_create_table(),
        column_mode: ColumnMode::from_index(ui.get_import_column_mode()),
        null_text: ui.get_import_null().to_string(),
        has_header: ui.get_import_header(),
        max_errors: ui
            .get_import_max_errors()
            .trim()
            .parse()
            .unwrap_or(50u64)
            .max(1),
        recursive: ui.get_import_recursive(),
        glob: ui.get_import_glob().to_string(),
        strip_prefix: ui.get_import_strip_prefix().to_string(),
        strip_suffix: ui.get_import_strip_suffix().to_string(),
        schema: ui.get_import_schema().to_string(),
        on_existing: OnExisting::from_index(ui.get_import_on_existing()),
        all_sheets: ui.get_import_all_sheets(),
    }
}

fn schema_from(ui: &AppWindow) -> SchemaSettings {
    SchemaSettings {
        dir: PathBuf::from(ui.get_schema_dir().to_string()),
        format: SchemaFormat::from_index(ui.get_schema_format()),
        use_selection: ui.get_schema_use_selection(),
        sample_rows: ui
            .get_schema_samples()
            .trim()
            .parse()
            .unwrap_or(3u32)
            .min(50),
        max_value_len: ui
            .get_schema_max_len()
            .trim()
            .parse()
            .unwrap_or(60usize)
            .clamp(8, 4000),
        include_pk: ui.get_schema_pk(),
        include_fk: ui.get_schema_fk(),
        include_indexes: ui.get_schema_idx(),
        include_checks: ui.get_schema_checks(),
        include_defaults: ui.get_schema_defaults(),
        include_descriptions: ui.get_schema_desc(),
        include_row_counts: ui.get_schema_rows(),
        include_views: ui.get_schema_views(),
        include_procs: ui.get_schema_procs(),
        include_triggers: ui.get_schema_triggers(),
        include_sequences: ui.get_schema_seq(),
        include_system: ui.get_schema_system(),
        split: ui.get_schema_split() && ui.get_schema_format() == 0,
        prompt_header: ui.get_schema_prompt().to_string(),
        file_name: ui.get_schema_filename().to_string(),
    }
}

fn selected_tables(ui: &AppWindow) -> Vec<TableRef> {
    let objs = ui.get_objects();
    (0..objs.row_count())
        .filter_map(|i| objs.row_data(i))
        .filter(|o| o.checked)
        .map(|o| TableRef {
            schema: o.schema.to_string(),
            name: o.name.to_string(),
            kind: o.kind.to_string(),
        })
        .collect()
}

fn rebuild_objects(ui: &AppWindow, state: &SharedState) {
    let st = match state.lock() {
        Ok(s) => s,
        Err(_) => return,
    };
    let filter = ui.get_obj_filter().to_string().to_lowercase();
    let items: Vec<ObjectItem> = st
        .all
        .iter()
        .filter(|(t, _)| filter.is_empty() || t.display().to_lowercase().contains(&filter))
        .map(|(t, rows)| ObjectItem {
            display: t.display().into(),
            kind: t.kind.clone().into(),
            rows: (*rows).clamp(0, i32::MAX as i64) as i32,
            rows_text: if *rows > 0 {
                fmt_thousands(*rows).into()
            } else {
                "".into()
            },
            checked: st.selected.contains(&t.display()),
            schema: t.schema.clone().into(),
            name: t.name.clone().into(),
        })
        .collect();
    let count = items.iter().filter(|o| o.checked).count();
    let total = st.all.len() as i32;
    drop(st);

    ui.set_objects(ModelRc::new(VecModel::from(items)));
    ui.set_selected_count(count as i32);
    ui.set_total_count(total);
}

fn rebuild_plan(ui: &AppWindow, state: &SharedState) {
    let st = match state.lock() {
        Ok(s) => s,
        Err(_) => return,
    };
    let items: Vec<PlanItem> = st
        .plan
        .iter()
        .map(|p| PlanItem {
            note: p.note.clone().into(),
            format: p.format.label().into(),
            target: p.target.display().into(),
            sheet: p.sheet.clone().unwrap_or_default().into(),
            enabled: p.enabled,
        })
        .collect();
    let enabled = st.plan.iter().filter(|p| p.enabled).count() as i32;
    drop(st);

    ui.set_plan_count(enabled);
    ui.set_import_plan(ModelRc::new(VecModel::from(items)));
}

// ---------------------------------------------------------------- 事件接线

fn wire(ui: &AppWindow, bridge: &Rc<bridge::Bridge>, state: &SharedState) {
    // ===== 连接 =====
    {
        let w = ui.as_weak();
        let b = bridge.clone();
        ui.on_connect_clicked(move || {
            if let Some(u) = w.upgrade() {
                if u.get_server().trim().is_empty() {
                    push_log(&u, "✗ 请填写服务器地址");
                    return;
                }
                u.set_connecting(true);
                u.set_status_text("连接中…".into());
                config::save(&persist_from(&u));
                b.send(Cmd::Connect(conn_from(&u)));
            }
        });
    }
    {
        let w = ui.as_weak();
        let b = bridge.clone();
        ui.on_test_clicked(move || {
            if let Some(u) = w.upgrade() {
                u.set_connecting(true);
                u.set_status_text("测试中…".into());
                b.send(Cmd::Test(conn_from(&u)));
            }
        });
    }
    {
        let w = ui.as_weak();
        let b = bridge.clone();
        let st = state.clone();
        ui.on_disconnect_clicked(move || {
            if let Some(u) = w.upgrade() {
                u.set_connected(false);
                u.set_connecting(false);
                u.set_status_text("未连接".into());
                u.set_server_info("".into());
                u.set_databases(ModelRc::default());
                u.set_db_index(0);
                if let Ok(mut s) = st.lock() {
                    s.all.clear();
                    s.selected.clear();
                }
                rebuild_objects(&u, &st);
                b.send(Cmd::Disconnect);
            }
        });
    }

    // ===== 刷新 & 切库 =====
    {
        let w = ui.as_weak();
        let b = bridge.clone();
        ui.on_refresh_clicked(move || {
            if let Some(u) = w.upgrade() {
                let db = u.get_database().to_string();
                if db.is_empty() {
                    push_log(&u, "✗ 请先选择数据库");
                    return;
                }
                u.set_loading_objects(true);
                b.send(Cmd::LoadObjects {
                    conn: conn_from(&u),
                    db,
                });
            }
        });
    }
    {
        let w = ui.as_weak();
        let b = bridge.clone();
        ui.on_db_picked(move |db| {
            if let Some(u) = w.upgrade() {
                u.set_database(db.clone());
                u.set_loading_objects(true);
                b.send(Cmd::LoadObjects {
                    conn: conn_from(&u),
                    db: db.to_string(),
                });
            }
        });
    }
    {
        let w = ui.as_weak();
        let st = state.clone();
        ui.on_filter_edited(move |v| {
            if let Some(u) = w.upgrade() {
                u.set_obj_filter(v);
                rebuild_objects(&u, &st);
            }
        });
    }

    // ===== 勾选 =====
    {
        let w = ui.as_weak();
        let st = state.clone();
        ui.on_object_toggled(move |i| {
            let Some(u) = w.upgrade() else { return };
            let objs = u.get_objects();
            let Some(mut item) = objs.row_data(i as usize) else {
                return;
            };
            item.checked = !item.checked;
            let key = format!("{}.{}", item.schema, item.name);
            if let Ok(mut s) = st.lock() {
                if item.checked {
                    s.selected.insert(key);
                } else {
                    s.selected.remove(&key);
                }
            }
            if let Some(vm) = objs.as_any().downcast_ref::<VecModel<ObjectItem>>() {
                vm.set_row_data(i as usize, item);
            }
            let c = (0..objs.row_count())
                .filter(|k| objs.row_data(*k).map(|x| x.checked).unwrap_or(false))
                .count();
            u.set_selected_count(c as i32);
        });
    }
    {
        let w = ui.as_weak();
        let st = state.clone();
        ui.on_select_all_clicked(move || {
            let Some(u) = w.upgrade() else { return };
            let objs = u.get_objects();
            if let Ok(mut s) = st.lock() {
                for i in 0..objs.row_count() {
                    if let Some(o) = objs.row_data(i) {
                        s.selected.insert(format!("{}.{}", o.schema, o.name));
                    }
                }
            }
            rebuild_objects(&u, &st);
        });
    }
    {
        let w = ui.as_weak();
        let st = state.clone();
        ui.on_select_none_clicked(move || {
            let Some(u) = w.upgrade() else { return };
            if let Ok(mut s) = st.lock() {
                s.selected.clear();
            }
            rebuild_objects(&u, &st);
        });
    }

    // ===== 目录 / 文件浏览 =====
    {
        let w = ui.as_weak();
        ui.on_export_browse_clicked(move || {
            if let Some(u) = w.upgrade() {
                let mut dlg = rfd::FileDialog::new().set_title("选择导出目录");
                let cur = u.get_export_dir().to_string();
                if !cur.is_empty() {
                    dlg = dlg.set_directory(cur);
                }
                if let Some(dir) = dlg.pick_folder() {
                    u.set_export_dir(dir.to_string_lossy().to_string().into());
                }
            }
        });
    }
    {
        let w = ui.as_weak();
        ui.on_import_browse_file_clicked(move || {
            if let Some(u) = w.upgrade() {
                let mut dlg = rfd::FileDialog::new()
                    .set_title("选择要导入的数据文件")
                    .add_filter(
                        "数据文件",
                        &[
                            "csv", "tsv", "tab", "json", "jsonl", "ndjson", "txt", "xlsx", "xlsm",
                            "xls", "ods",
                        ],
                    )
                    .add_filter("所有文件", &["*"]);
                let cur = u.get_import_path().to_string();
                if let Some(parent) = std::path::Path::new(&cur).parent() {
                    if parent.exists() {
                        dlg = dlg.set_directory(parent);
                    }
                }
                if let Some(f) = dlg.pick_file() {
                    u.set_import_path(f.to_string_lossy().to_string().into());
                    if let Some(stem) = f.file_stem().and_then(|s| s.to_str()) {
                        if u.get_import_target().trim().is_empty() {
                            u.set_import_target(stem.into());
                        }
                    }
                }
            }
        });
    }
    {
        let w = ui.as_weak();
        let st = state.clone();
        ui.on_import_browse_dir_clicked(move || {
            if let Some(u) = w.upgrade() {
                let mut dlg = rfd::FileDialog::new().set_title("选择包含数据文件的目录");
                let cur = u.get_import_path().to_string();
                if !cur.is_empty() && std::path::Path::new(&cur).is_dir() {
                    dlg = dlg.set_directory(cur);
                }
                if let Some(dir) = dlg.pick_folder() {
                    u.set_import_path(dir.to_string_lossy().to_string().into());
                    // 目录模式下目标表由文件名推导
                    u.set_import_target("".into());
                    if let Ok(mut s) = st.lock() {
                        s.plan.clear();
                    }
                    rebuild_plan(&u, &st);
                }
            }
        });
    }

    // ===== 扫描导入计划 =====
    {
        let w = ui.as_weak();
        let b = bridge.clone();
        ui.on_scan_import_clicked(move || {
            if let Some(u) = w.upgrade() {
                if u.get_import_path().trim().is_empty() {
                    push_log(&u, "✗ 请先选择源文件或目录");
                    return;
                }
                b.send(Cmd::ScanImport {
                    settings: import_from(&u),
                    pattern: u.get_import_glob().to_string(),
                });
            }
        });
    }
    {
        let w = ui.as_weak();
        let st = state.clone();
        ui.on_plan_toggled(move |i| {
            let Some(u) = w.upgrade() else { return };
            if let Ok(mut s) = st.lock() {
                if let Some(p) = s.plan.get_mut(i as usize) {
                    p.enabled = !p.enabled;
                }
            }
            rebuild_plan(&u, &st);
        });
    }
    {
        let w = ui.as_weak();
        let st = state.clone();
        ui.on_plan_clear_clicked(move || {
            let Some(u) = w.upgrade() else { return };
            if let Ok(mut s) = st.lock() {
                s.plan.clear();
            }
            rebuild_plan(&u, &st);
        });
    }

    // ===== 预览 =====
    {
        let w = ui.as_weak();
        let b = bridge.clone();
        let st = state.clone();
        ui.on_import_preview_clicked(move || {
            if let Some(u) = w.upgrade() {
                if u.get_import_path().trim().is_empty() {
                    push_log(&u, "✗ 请先选择源文件");
                    return;
                }
                let sheet = st.lock().ok().and_then(|s| {
                    let p = PathBuf::from(u.get_import_path().to_string());
                    s.plan
                        .iter()
                        .find(|i| i.path == p)
                        .and_then(|i| i.sheet.clone())
                });
                b.send(Cmd::PreviewImport {
                    settings: import_from(&u),
                    path: PathBuf::from(u.get_import_path().to_string()),
                    sheet,
                });
            }
        });
    }
    {
        let w = ui.as_weak();
        let b = bridge.clone();
        ui.on_export_preview_clicked(move || {
            if let Some(u) = w.upgrade() {
                let tables = selected_tables(&u);
                if tables.is_empty() {
                    push_log(&u, "✗ 请先勾选要导出的表");
                    return;
                }
                b.send(Cmd::PreviewSql {
                    conn: conn_from(&u),
                    settings: export_from(&u),
                    tables,
                });
            }
        });
    }

    // ===== 备份 =====
    {
        let w = ui.as_weak();
        let b = bridge.clone();
        ui.on_backup_dir_query_clicked(move || {
            if let Some(u) = w.upgrade() {
                b.send(Cmd::BackupDir {
                    conn: conn_from(&u),
                });
            }
        });
    }
    {
        let w = ui.as_weak();
        let b = bridge.clone();
        ui.on_start_backup_clicked(move || {
            if let Some(u) = w.upgrade() {
                if u.get_backup_dir().trim().is_empty() {
                    push_log(&u, "✗ 请填写服务器端备份目录");
                    return;
                }
                config::save(&persist_from(&u));
                u.set_running(true);
                u.set_progress(0.0);
                u.set_rows_text("".into());
                u.set_bytes_text("".into());
                u.set_progress_text("准备备份…".into());
                push_log(&u, format!("━━ 开始备份 {} ━━", u.get_database()));
                b.send(Cmd::Backup {
                    conn: conn_from(&u),
                    settings: backup_from(&u),
                });
            }
        });
    }
    {
        let w = ui.as_weak();
        let b = bridge.clone();
        ui.on_backup_history_clicked(move || {
            if let Some(u) = w.upgrade() {
                b.send(Cmd::BackupHistory {
                    conn: conn_from(&u),
                    db: u.get_database().to_string(),
                });
            }
        });
    }

    // ===== 执行导出 =====
    {
        let w = ui.as_weak();
        let b = bridge.clone();
        ui.on_start_export_clicked(move || {
            if let Some(u) = w.upgrade() {
                let tables = selected_tables(&u);
                if tables.is_empty() {
                    push_log(&u, "✗ 请先在「对象浏览」中勾选对象");
                    return;
                }
                if u.get_export_dir().trim().is_empty() {
                    push_log(&u, "✗ 请选择导出目录");
                    return;
                }
                config::save(&persist_from(&u));
                u.set_running(true);
                u.set_progress(0.0);
                u.set_rows_text("".into());
                u.set_bytes_text("".into());
                u.set_progress_text(format!("准备导出 {} 个对象…", tables.len()).into());
                push_log(&u, format!("━━ 开始导出 {} 个对象 ━━", tables.len()));
                b.send(Cmd::Export {
                    conn: conn_from(&u),
                    settings: export_from(&u),
                    tables,
                });
            }
        });
    }

    // ===== 执行导入 =====
    {
        let w = ui.as_weak();
        let b = bridge.clone();
        let st = state.clone();
        ui.on_start_import_clicked(move || {
            let Some(u) = w.upgrade() else { return };
            if u.get_import_path().trim().is_empty() {
                push_log(&u, "✗ 请选择源文件或目录");
                return;
            }
            let settings = import_from(&u);

            // 未扫描、或源路径已变更时，就地重新生成计划
            let src = PathBuf::from(u.get_import_path().to_string());
            let mut items: Vec<ImportPlanItem> = match st.lock() {
                Ok(s) => s.plan.clone(),
                Err(_) => Vec::new(),
            };
            let stale = items.iter().any(|i| {
                if src.is_dir() {
                    !i.path.starts_with(&src)
                } else {
                    i.path != src
                }
            });
            if items.is_empty() || stale {
                match db::import::scan_plan(&settings, u.get_import_glob().as_ref()) {
                    Ok(p) => {
                        if let Ok(mut s) = st.lock() {
                            s.plan = p.clone();
                        }
                        rebuild_plan(&u, &st);
                        items = p;
                    }
                    Err(e) => {
                        push_log(&u, format!("✗ 扫描失败：{:#}", e));
                        return;
                    }
                }
            }
            if items.iter().all(|i| !i.enabled) {
                push_log(&u, "✗ 没有启用的导入任务");
                return;
            }

            config::save(&persist_from(&u));
            u.set_running(true);
            u.set_progress(0.0);
            u.set_rows_text("".into());
            u.set_bytes_text("".into());
            u.set_progress_text("准备导入…".into());
            push_log(&u, format!("━━ 开始导入 {} 个任务 ━━", items.len()));
            b.send(Cmd::Import {
                conn: conn_from(&u),
                settings,
                items,
            });
        });
    }

    // ===== 结构导出 =====
    {
        let w = ui.as_weak();
        ui.on_schema_browse_clicked(move || {
            if let Some(u) = w.upgrade() {
                let mut dlg = rfd::FileDialog::new().set_title("选择结构文档输出目录");
                let cur = u.get_schema_dir().to_string();
                if !cur.is_empty() {
                    dlg = dlg.set_directory(cur);
                }
                if let Some(dir) = dlg.pick_folder() {
                    u.set_schema_dir(dir.to_string_lossy().to_string().into());
                }
            }
        });
    }
    {
        let w = ui.as_weak();
        let b = bridge.clone();
        ui.on_schema_preview_clicked(move || {
            if let Some(u) = w.upgrade() {
                u.set_schema_preview("".into());
                b.send(Cmd::PreviewSchema {
                    conn: conn_from(&u),
                    settings: schema_from(&u),
                    tables: selected_tables(&u),
                });
            }
        });
    }
    {
        let w = ui.as_weak();
        let b = bridge.clone();
        ui.on_start_schema_clicked(move || {
            if let Some(u) = w.upgrade() {
                if u.get_schema_dir().trim().is_empty() {
                    push_log(&u, "✗ 请选择输出目录");
                    return;
                }
                config::save(&persist_from(&u));
                u.set_running(true);
                u.set_progress(0.0);
                u.set_rows_text("".into());
                u.set_bytes_text("".into());
                u.set_progress_text("正在采集数据库结构…".into());
                push_log(&u, "━━ 结构导出 ━━");
                b.send(Cmd::DumpSchema {
                    conn: conn_from(&u),
                    settings: schema_from(&u),
                    tables: selected_tables(&u),
                });
            }
        });
    }

    {
        let b = bridge.clone();
        ui.on_cancel_clicked(move || {
            b.cancel();
        });
    }
    {
        let w = ui.as_weak();
        ui.on_clear_log_clicked(move || {
            if let Some(u) = w.upgrade() {
                u.set_log_text("".into());
            }
        });
    }
    {
        let w = ui.as_weak();
        ui.on_open_dir_clicked(move || {
            if let Some(u) = w.upgrade() {
                let dir = if u.get_nav_index() == 2 {
                    u.get_schema_dir().to_string()
                } else {
                    u.get_export_dir().to_string()
                };
                if dir.is_empty() {
                    return;
                }
                #[cfg(target_os = "macos")]
                {
                    let _ = std::process::Command::new("open").arg(&dir).spawn();
                }
                #[cfg(target_os = "windows")]
                {
                    let _ = std::process::Command::new("explorer").arg(&dir).spawn();
                }
                #[cfg(all(unix, not(target_os = "macos")))]
                {
                    let _ = std::process::Command::new("xdg-open").arg(&dir).spawn();
                }
            }
        });
    }
}

// ---------------------------------------------------------------- 事件应用

fn push_log(ui: &AppWindow, line: impl AsRef<str>) {
    let mut s = ui.get_log_text().to_string();
    if s.len() > 400_000 {
        let keep = s.len() - 200_000;
        s = s[keep..].to_string();
    }
    if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }
    s.push_str(line.as_ref());
    s.push('\n');
    ui.set_log_text(s.into());
}

fn fmt_thousands(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 {
        format!("-{}", out)
    } else {
        out
    }
}

fn human_bytes(b: u64) -> String {
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

pub fn apply_event(ui: &AppWindow, state: &SharedState, ev: UiEvent) {
    match ev {
        UiEvent::Status(s) => {
            ui.set_status_text(s.into());
            ui.set_connecting(false);
        }
        UiEvent::Connected { info } => {
            ui.set_connected(true);
            ui.set_connecting(false);
            ui.set_status_text("已连接".into());
            ui.set_server_info(info.clone().into());
            push_log(ui, format!("✓ {}", info));
        }
        UiEvent::Disconnected => {
            ui.set_connected(false);
            ui.set_connecting(false);
            ui.set_status_text("未连接".into());
            ui.set_server_info("".into());
        }
        UiEvent::Databases(dbs) => {
            let cur = ui.get_database().to_string();
            let idx = dbs.iter().position(|d| d == &cur).unwrap_or(0);
            let items: Vec<slint::SharedString> = dbs.iter().map(|d| d.as_str().into()).collect();
            ui.set_databases(ModelRc::new(VecModel::from(items)));
            if !dbs.is_empty() {
                ui.set_db_index(idx as i32);
                if cur.is_empty() || !dbs.contains(&cur) {
                    ui.set_database(dbs[idx].clone().into());
                }
            }
        }
        UiEvent::Objects(objs) => {
            ui.set_loading_objects(false);
            if let Ok(mut s) = state.lock() {
                s.all = objs;
                s.selected.clear();
            }
            rebuild_objects(ui, state);
        }
        UiEvent::Progress {
            frac,
            text,
            rows,
            bytes,
        } => {
            ui.set_progress(frac);
            ui.set_progress_text(text.into());
            if rows > 0 {
                ui.set_rows_text(format!("{} 行", fmt_thousands(rows as i64)).into());
            }
            if bytes > 0 {
                ui.set_bytes_text(human_bytes(bytes).into());
            }
        }
        UiEvent::Log(line) => push_log(ui, line),
        UiEvent::Preview(text) => {
            ui.set_import_preview(text.clone().into());
            push_log(ui, "── 预览 ──");
            for l in text.lines().take(60) {
                push_log(ui, l);
            }
        }
        UiEvent::SchemaPreview(text) => {
            ui.set_schema_preview(text.into());
            push_log(ui, "── 结构预览 ──");
        }
        UiEvent::ImportPlan(items) => {
            let n = items.len();
            if let Ok(mut s) = state.lock() {
                s.plan = items;
            }
            rebuild_plan(ui, state);
            push_log(ui, format!("已生成 {} 个导入任务", n));
        }
        UiEvent::BackupDir(dir) => {
            if ui.get_backup_dir().trim().is_empty() {
                ui.set_backup_dir(dir.clone().into());
                push_log(ui, format!("服务器默认备份目录：{}", dir));
            }
        }
        UiEvent::BackupInfo(text) => {
            ui.set_backup_info(text.clone().into());
            push_log(ui, "── 备份历史 ──");
            for l in text.lines().take(20) {
                push_log(ui, l);
            }
        }
        UiEvent::Done(sum) => {
            ui.set_running(false);
            ui.set_progress(1.0);
            ui.set_progress_text(sum.message.clone().into());
            ui.set_status_text(if sum.ok { "完成" } else { "已停止" }.into());
            push_log(
                ui,
                format!("{} {}", if sum.ok { "✓" } else { "⚠" }, sum.message),
            );
        }
        UiEvent::Failed(msg) => {
            ui.set_running(false);
            ui.set_connecting(false);
            ui.set_progress_text(format!("失败：{}", msg).into());
            ui.set_status_text("出错".into());
            push_log(ui, format!("✗ {}", msg));
        }
    }
}
