//! 配置持久化：只保存非敏感设置，密码永不落盘。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Persist {
    pub server: String,
    pub port: String,
    pub instance: String,
    pub auth_index: i32,
    pub username: String,
    pub database: String,
    pub encrypt_index: i32,
    pub trust_cert: bool,
    pub conn_timeout: String,
    pub export_dir: String,
    pub export_format: i32,
    pub export_scope: i32,
    pub export_what: i32,
    pub export_batch: String,
    pub export_null: String,
    pub export_delimiter: i32,
    pub export_header: bool,
    pub export_drop: bool,
    pub export_identity: bool,
    pub export_quote_all: bool,
    pub export_bom: bool,
    pub export_create_db: bool,
    pub export_schema_qualified: bool,
    pub export_prefix: String,
    pub export_where: String,
    pub export_limit: String,
    pub export_exclude: String,
    pub export_gzip: bool,
    pub excel_freeze: bool,
    pub excel_autofit: bool,
    pub excel_bold: bool,
    // ===== 备份 =====
    pub backup_dir: String,
    pub backup_file: String,
    pub backup_kind: i32,
    pub backup_compress: bool,
    pub backup_checksum: bool,
    pub backup_init: bool,
    pub backup_copy_only: bool,
    pub backup_desc: String,
    pub backup_stats: String,
    pub backup_verify: bool,
    // ===== 导入 =====
    pub import_path: String,
    pub import_format: i32,
    pub import_target: String,
    pub import_batch: String,
    pub import_truncate: bool,
    pub import_transaction: bool,
    pub import_skip_errors: bool,
    pub import_create_table: bool,
    pub import_column_mode: i32,
    pub import_null: String,
    pub import_header: bool,
    pub import_max_errors: String,
    pub import_recursive: bool,
    pub import_glob: String,
    pub import_strip_prefix: String,
    pub import_strip_suffix: String,
    pub import_schema: String,
    pub import_on_existing: i32,
    pub import_all_sheets: bool,
    // ===== 结构导出 =====
    pub schema_dir: String,
    pub schema_format: i32,
    pub schema_use_selection: bool,
    pub schema_samples: String,
    pub schema_max_len: String,
    pub schema_pk: bool,
    pub schema_fk: bool,
    pub schema_idx: bool,
    pub schema_checks: bool,
    pub schema_defaults: bool,
    pub schema_desc: bool,
    pub schema_rows: bool,
    pub schema_views: bool,
    pub schema_procs: bool,
    pub schema_triggers: bool,
    pub schema_seq: bool,
    pub schema_system: bool,
    pub schema_split: bool,
    pub schema_prompt: String,
    pub schema_filename: String,
    // ===== 界面 =====
    pub nav_index: i32,
    pub window_w: f32,
    pub window_h: f32,
}

impl Default for Persist {
    fn default() -> Self {
        Self {
            server: "localhost".into(),
            port: "1433".into(),
            instance: String::new(),
            auth_index: 0,
            username: "sa".into(),
            database: "master".into(),
            encrypt_index: 0,
            trust_cert: true,
            conn_timeout: "15".into(),
            export_dir: dirs::download_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
            export_format: 0,
            export_scope: 0,
            export_what: 0,
            export_batch: "1000".into(),
            export_null: "NULL".into(),
            export_delimiter: 0,
            export_header: true,
            export_drop: false,
            export_identity: false,
            export_quote_all: false,
            export_bom: false,
            export_create_db: false,
            export_schema_qualified: true,
            export_prefix: String::new(),
            export_where: String::new(),
            export_limit: "0".into(),
            export_exclude: String::new(),
            export_gzip: false,
            excel_freeze: true,
            excel_autofit: true,
            excel_bold: true,
            backup_dir: String::new(),
            backup_file: String::new(),
            backup_kind: 0,
            backup_compress: true,
            backup_checksum: true,
            backup_init: true,
            backup_copy_only: false,
            backup_desc: String::new(),
            backup_stats: "10".into(),
            backup_verify: true,
            import_path: String::new(),
            import_format: 0,
            import_target: String::new(),
            import_batch: "500".into(),
            import_truncate: false,
            import_transaction: true,
            import_skip_errors: false,
            import_create_table: false,
            import_column_mode: 0,
            import_null: String::new(),
            import_header: true,
            import_max_errors: "50".into(),
            import_recursive: true,
            import_glob: String::new(),
            import_strip_prefix: String::new(),
            import_strip_suffix: String::new(),
            import_schema: "dbo".into(),
            import_on_existing: 0,
            import_all_sheets: true,
            schema_dir: dirs::download_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
            schema_format: 0,
            schema_use_selection: false,
            schema_samples: "3".into(),
            schema_max_len: "60".into(),
            schema_pk: true,
            schema_fk: true,
            schema_idx: true,
            schema_checks: true,
            schema_defaults: true,
            schema_desc: true,
            schema_rows: true,
            schema_views: true,
            schema_procs: false,
            schema_triggers: false,
            schema_seq: true,
            schema_system: false,
            schema_split: false,
            schema_prompt: String::new(),
            schema_filename: String::new(),
            nav_index: 0,
            window_w: 1320.0,
            window_h: 880.0,
        }
    }
}

pub fn path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("mssql-transfer").join("config.json"))
}

pub fn load() -> Persist {
    let Some(p) = path() else {
        return Persist::default();
    };
    match std::fs::read_to_string(&p) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Persist::default(),
    }
}

pub fn save(cfg: &Persist) {
    let Some(p) = path() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(s) = serde_json::to_string_pretty(cfg) {
        let _ = std::fs::write(p, s);
    }
}
