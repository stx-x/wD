//! 共享数据模型：连接配置、任务参数、后台事件。

use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AuthKind {
    Sql,
    Windows,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EncryptMode {
    Optional,
    Required,
}

#[derive(Clone, Debug)]
pub struct ConnSettings {
    pub server: String,
    pub port: u16,
    pub instance: String,
    pub auth: AuthKind,
    pub username: String,
    pub password: String,
    pub database: String,
    pub encrypt: EncryptMode,
    pub trust_cert: bool,
    pub timeout_secs: u64,
    pub app_name: String,
}

impl Default for ConnSettings {
    fn default() -> Self {
        Self {
            server: "localhost".into(),
            port: 1433,
            instance: String::new(),
            auth: AuthKind::Sql,
            username: "sa".into(),
            password: String::new(),
            database: "master".into(),
            encrypt: EncryptMode::Optional,
            trust_cert: true,
            timeout_secs: 15,
            app_name: "MSSQL Transfer Studio".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableRef {
    pub schema: String,
    pub name: String,
    pub kind: String,
}

impl TableRef {
    pub fn display(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }
    pub fn qualified(&self) -> String {
        format!("[{}].[{}]", self.schema, self.name)
    }
}

#[derive(Clone, Debug)]
pub struct ColumnMeta {
    pub name: String,
    pub type_name: String,
    pub max_length: i32,
    pub precision: i32,
    pub scale: i32,
    pub nullable: bool,
    pub is_identity: bool,
}

impl ColumnMeta {
    pub fn is_numeric(&self) -> bool {
        matches!(
            self.type_name.as_str(),
            "int"
                | "bigint"
                | "smallint"
                | "tinyint"
                | "decimal"
                | "numeric"
                | "float"
                | "real"
                | "money"
                | "smallmoney"
        )
    }
    pub fn is_bool(&self) -> bool {
        self.type_name == "bit"
    }
    #[allow(dead_code)]
    pub fn is_binary(&self) -> bool {
        matches!(
            self.type_name.as_str(),
            "binary" | "varbinary" | "image" | "timestamp" | "rowversion"
        )
    }
    #[allow(dead_code)]
    pub fn is_temporal(&self) -> bool {
        matches!(
            self.type_name.as_str(),
            "date" | "datetime" | "datetime2" | "smalldatetime" | "datetimeoffset" | "time"
        )
    }

    /// 用于 DDL 的完整类型声明
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
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExportFormat {
    Csv,
    Tsv,
    JsonArray,
    Ndjson,
    SqlInsert,
    SqlFull,
    Excel,
}

impl ExportFormat {
    pub fn from_index(i: i32) -> Self {
        match i {
            1 => Self::Tsv,
            2 => Self::JsonArray,
            3 => Self::Ndjson,
            4 => Self::SqlInsert,
            5 => Self::SqlFull,
            6 => Self::Excel,
            _ => Self::Csv,
        }
    }
    pub fn ext(&self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Tsv => "tsv",
            Self::JsonArray | Self::Ndjson => "json",
            Self::SqlInsert | Self::SqlFull => "sql",
            Self::Excel => "xlsx",
        }
    }
    pub fn is_sql(&self) -> bool {
        matches!(self, Self::SqlInsert | Self::SqlFull)
    }
    #[allow(dead_code)]
    pub fn is_json(&self) -> bool {
        matches!(self, Self::JsonArray | Self::Ndjson)
    }
    #[allow(dead_code)]
    pub fn is_delimited(&self) -> bool {
        matches!(self, Self::Csv | Self::Tsv)
    }
    pub fn is_excel(&self) -> bool {
        matches!(self, Self::Excel)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileScope {
    PerTable,
    SingleFile,
}

impl FileScope {
    pub fn from_index(i: i32) -> Self {
        if i == 1 {
            Self::SingleFile
        } else {
            Self::PerTable
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExportContent {
    SchemaAndData,
    SchemaOnly,
    DataOnly,
}

impl ExportContent {
    pub fn from_index(i: i32) -> Self {
        match i {
            1 => Self::SchemaOnly,
            2 => Self::DataOnly,
            _ => Self::SchemaAndData,
        }
    }
    pub fn with_schema(&self) -> bool {
        !matches!(self, Self::DataOnly)
    }
    pub fn with_data(&self) -> bool {
        !matches!(self, Self::SchemaOnly)
    }
}

#[derive(Clone, Debug)]
pub struct ExportSettings {
    pub dir: PathBuf,
    pub format: ExportFormat,
    pub scope: FileScope,
    pub content: ExportContent,
    pub batch: usize,
    pub null_text: String,
    pub delimiter: u8,
    pub header: bool,
    pub quote_all: bool,
    pub bom: bool,
    pub drop_table: bool,
    pub identity_insert: bool,
    pub create_database: bool,
    pub schema_qualified: bool,
    pub file_prefix: String,
    pub where_clause: String,
    /// 只导出前 N 行，0 表示不限
    pub limit_rows: u64,
    /// 排除的列，逗号分隔
    pub exclude_columns: String,
    /// CSV/JSON/SQL 输出 gzip
    pub gzip: bool,
    /// Excel：冻结首行
    pub excel_freeze: bool,
    /// Excel：自适应列宽
    pub excel_autofit: bool,
    /// Excel：表头加粗
    pub excel_bold_header: bool,
}

impl Default for ExportSettings {
    fn default() -> Self {
        Self {
            dir: PathBuf::new(),
            format: ExportFormat::Csv,
            scope: FileScope::PerTable,
            content: ExportContent::SchemaAndData,
            batch: 1000,
            null_text: "NULL".into(),
            delimiter: b',',
            header: true,
            quote_all: false,
            bom: false,
            drop_table: false,
            identity_insert: false,
            create_database: false,
            schema_qualified: true,
            file_prefix: String::new(),
            where_clause: String::new(),
            limit_rows: 0,
            exclude_columns: String::new(),
            gzip: false,
            excel_freeze: true,
            excel_autofit: true,
            excel_bold_header: true,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BackupKind {
    Full,
    Differential,
    Log,
}

impl BackupKind {
    pub fn from_index(i: i32) -> Self {
        match i {
            1 => Self::Differential,
            2 => Self::Log,
            _ => Self::Full,
        }
    }
    #[allow(dead_code)]
    pub fn keyword(&self) -> &'static str {
        match self {
            Self::Full => "DATABASE",
            Self::Differential => "DATABASE",
            Self::Log => "LOG",
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            Self::Full => "完整备份",
            Self::Differential => "差异备份",
            Self::Log => "事务日志备份",
        }
    }
    pub fn suffix(&self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Differential => "diff",
            Self::Log => "log",
        }
    }
}

#[derive(Clone, Debug)]
pub struct BackupSettings {
    /// 服务器端目录
    pub dir: String,
    pub file_name: String,
    pub kind: BackupKind,
    pub compress: bool,
    pub checksum: bool,
    pub init: bool,
    pub copy_only: bool,
    pub description: String,
    pub stats: u32,
    pub verify: bool,
}

impl Default for BackupSettings {
    fn default() -> Self {
        Self {
            dir: String::new(),
            file_name: String::new(),
            kind: BackupKind::Full,
            compress: true,
            checksum: true,
            init: true,
            copy_only: false,
            description: String::new(),
            stats: 10,
            verify: true,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImportFormat {
    Auto,
    Csv,
    Tsv,
    Json,
    Excel,
}

impl ImportFormat {
    pub fn from_index(i: i32) -> Self {
        match i {
            1 => Self::Csv,
            2 => Self::Tsv,
            3 => Self::Json,
            4 => Self::Excel,
            _ => Self::Auto,
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            Self::Auto => "自动",
            Self::Csv => "CSV",
            Self::Tsv => "TSV",
            Self::Json => "JSON",
            Self::Excel => "Excel",
        }
    }
    pub fn is_json(&self) -> bool {
        matches!(self, Self::Json)
    }
    /// 按扩展名推断格式
    pub fn detect(path: &std::path::Path) -> Self {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        match ext.as_str() {
            "csv" | "txt" => Self::Csv,
            "tsv" | "tab" => Self::Tsv,
            "json" | "jsonl" | "ndjson" => Self::Json,
            "xlsx" | "xlsm" | "xlsb" | "xls" | "ods" => Self::Excel,
            _ => Self::Csv,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColumnMode {
    ByName,
    ByPosition,
}

impl ColumnMode {
    pub fn from_index(i: i32) -> Self {
        if i == 1 {
            Self::ByPosition
        } else {
            Self::ByName
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OnExisting {
    Append,
    Truncate,
    Skip,
    Fail,
}

impl OnExisting {
    pub fn from_index(i: i32) -> Self {
        match i {
            1 => Self::Truncate,
            2 => Self::Skip,
            3 => Self::Fail,
            _ => Self::Append,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ImportSettings {
    pub path: PathBuf,
    pub format: ImportFormat,
    pub target: TableRef,
    pub batch: usize,
    pub truncate: bool,
    pub transaction: bool,
    pub skip_errors: bool,
    pub create_table: bool,
    pub column_mode: ColumnMode,
    pub null_text: String,
    pub has_header: bool,
    pub max_errors: u64,
    /// 递归子目录
    pub recursive: bool,
    /// 文件名通配符（如 `*_2024*.csv`），空表示全部
    pub glob: String,
    /// 文件名剥离前缀
    pub strip_prefix: String,
    /// 文件名剥离后缀
    pub strip_suffix: String,
    /// 目标 schema
    pub schema: String,
    /// 同名表已存在时的行为
    pub on_existing: OnExisting,
    /// Excel 导入所有 sheet（每个 sheet 一个表）
    pub all_sheets: bool,
}

impl Default for ImportSettings {
    fn default() -> Self {
        Self {
            path: PathBuf::new(),
            format: ImportFormat::Auto,
            target: TableRef {
                schema: "dbo".into(),
                name: String::new(),
                kind: "table".into(),
            },
            batch: 500,
            truncate: false,
            transaction: true,
            skip_errors: false,
            create_table: false,
            column_mode: ColumnMode::ByName,
            null_text: String::new(),
            has_header: true,
            max_errors: 50,
            recursive: true,
            glob: String::new(),
            strip_prefix: String::new(),
            strip_suffix: String::new(),
            schema: "dbo".into(),
            on_existing: OnExisting::Append,
            all_sheets: true,
        }
    }
}

/// 批量导入队列中的一项
#[derive(Clone, Debug)]
pub struct ImportPlanItem {
    pub path: PathBuf,
    pub format: ImportFormat,
    pub target: TableRef,
    pub sheet: Option<String>,
    pub enabled: bool,
    pub note: String,
}

/// 结构导出的输出格式
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SchemaFormat {
    /// 面向 AI/Agent 的完整上下文
    AiContext,
    /// 纯 Markdown 结构文档
    Markdown,
    /// 可执行 T-SQL DDL
    Ddl,
    /// 结构化 JSON
    Json,
    /// Mermaid ER 图
    Mermaid,
}

impl SchemaFormat {
    pub fn from_index(i: i32) -> Self {
        match i {
            1 => Self::Markdown,
            2 => Self::Ddl,
            3 => Self::Json,
            4 => Self::Mermaid,
            _ => Self::AiContext,
        }
    }
    #[allow(dead_code)]
    pub fn label(&self) -> &'static str {
        match self {
            Self::AiContext => "AI",
            Self::Markdown => "MD",
            Self::Ddl => "DDL",
            Self::Json => "JSON",
            Self::Mermaid => "ER",
        }
    }
    #[allow(dead_code)]
    pub fn ext(&self) -> &'static str {
        match self {
            Self::AiContext | Self::Markdown => "md",
            Self::Ddl => "sql",
            Self::Json => "json",
            Self::Mermaid => "mmd",
        }
    }
}

/// 结构导出配置
#[derive(Clone, Debug)]
pub struct SchemaSettings {
    pub dir: PathBuf,
    pub format: SchemaFormat,
    /// 只导出对象浏览里勾选的对象
    pub use_selection: bool,
    /// 每表采样行数，0 = 不采样
    pub sample_rows: u32,
    /// 单个样例值最大字符数
    pub max_value_len: usize,
    pub include_pk: bool,
    pub include_fk: bool,
    pub include_indexes: bool,
    pub include_checks: bool,
    pub include_defaults: bool,
    pub include_descriptions: bool,
    pub include_row_counts: bool,
    pub include_views: bool,
    pub include_procs: bool,
    pub include_triggers: bool,
    pub include_sequences: bool,
    pub include_system: bool,
    /// AI 格式下拆分为多文件（总览 + 每表一篇 + DDL + ER + JSON）
    pub split: bool,
    /// 自定义提示词，写在文档最前面
    pub prompt_header: String,
    /// 自定义文件名，空则自动命名
    pub file_name: String,
}

impl Default for SchemaSettings {
    fn default() -> Self {
        Self {
            dir: PathBuf::new(),
            format: SchemaFormat::AiContext,
            use_selection: false,
            sample_rows: 3,
            max_value_len: 60,
            include_pk: true,
            include_fk: true,
            include_indexes: true,
            include_checks: true,
            include_defaults: true,
            include_descriptions: true,
            include_row_counts: true,
            include_views: true,
            include_procs: false,
            include_triggers: false,
            include_sequences: true,
            include_system: false,
            split: false,
            prompt_header: String::new(),
            file_name: String::new(),
        }
    }
}

/// 单元格的通用表示
#[derive(Clone, Debug)]
pub enum Cell {
    Null,
    Text(String),
    Bytes(Vec<u8>),
}

impl Cell {
    pub fn as_display(&self, null_text: &str) -> String {
        match self {
            Cell::Null => null_text.to_string(),
            Cell::Text(s) => s.clone(),
            Cell::Bytes(b) => hex_encode(b),
        }
    }
    #[allow(dead_code)]
    pub fn is_null(&self) -> bool {
        matches!(self, Cell::Null)
    }
}

pub fn hex_encode(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2 + 2);
    s.push_str("0x");
    for byte in b {
        s.push_str(&format!("{:02X}", byte));
    }
    s
}

#[derive(Clone, Debug)]
pub struct Summary {
    pub ok: bool,
    pub message: String,
}

/// 后台线程 -> UI 的事件
#[derive(Clone, Debug)]
pub enum UiEvent {
    Status(String),
    Connected {
        info: String,
    },
    Disconnected,
    Databases(Vec<String>),
    Objects(Vec<(TableRef, i64)>),
    Progress {
        frac: f32,
        text: String,
        rows: u64,
        bytes: u64,
    },
    Log(String),
    Preview(String),
    /// 结构文档预览
    SchemaPreview(String),
    ImportPlan(Vec<ImportPlanItem>),
    /// 服务器端默认备份目录
    BackupDir(String),
    /// 备份历史文本
    BackupInfo(String),
    Done(Summary),
    Failed(String),
}

/// UI -> 后台线程的命令
#[allow(dead_code)]
pub enum Cmd {
    Connect(ConnSettings),
    Test(ConnSettings),
    Disconnect,
    ListDatabases(ConnSettings),
    LoadObjects {
        conn: ConnSettings,
        db: String,
    },
    Export {
        conn: ConnSettings,
        settings: ExportSettings,
        tables: Vec<TableRef>,
    },
    Backup {
        conn: ConnSettings,
        settings: BackupSettings,
    },
    /// 导出数据库/表结构（DDL、AI 上下文、ER 图等）
    DumpSchema {
        conn: ConnSettings,
        settings: SchemaSettings,
        tables: Vec<TableRef>,
    },
    /// 结构文档预览（不落盘）
    PreviewSchema {
        conn: ConnSettings,
        settings: SchemaSettings,
        tables: Vec<TableRef>,
    },
    /// 查询服务器端默认备份目录
    BackupDir {
        conn: ConnSettings,
    },
    /// 查询某库的备份历史
    BackupHistory {
        conn: ConnSettings,
        db: String,
    },
    ScanImport {
        settings: ImportSettings,
        pattern: String,
    },
    Import {
        conn: ConnSettings,
        settings: ImportSettings,
        items: Vec<ImportPlanItem>,
    },
    PreviewImport {
        settings: ImportSettings,
        path: PathBuf,
        sheet: Option<String>,
    },
    PreviewSql {
        conn: ConnSettings,
        settings: ExportSettings,
        tables: Vec<TableRef>,
    },
    Cancel,
}
