//! 后台工作线程：承载 tokio 运行时，串行执行数据库任务。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use slint::Weak;
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::db::{backup, conn, export, import, meta, schema, xlsx_out};
use crate::model::{Cmd, ExportFormat, UiEvent};
use crate::{AppWindow, SharedState};

pub struct Bridge {
    tx: UnboundedSender<Cmd>,
    cancel: Arc<AtomicBool>,
}

impl Bridge {
    pub fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

fn emit(ui: &Weak<AppWindow>, state: &SharedState, ev: UiEvent) {
    let w = ui.clone();
    let st = state.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(u) = w.upgrade() {
            crate::apply_event(&u, &st, ev);
        }
    });
}

fn make_ctx(
    ui: &Weak<AppWindow>,
    state: &SharedState,
    cancel: &Arc<AtomicBool>,
) -> export::TaskCtx {
    let w = ui.clone();
    let st = state.clone();
    export::TaskCtx {
        cancel: cancel.clone(),
        emit: Arc::new(move |ev| emit(&w, &st, ev)),
    }
}

pub fn spawn(ui: Weak<AppWindow>, state: SharedState) -> Bridge {
    let (tx, mut rx) = mpsc::unbounded_channel::<Cmd>();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_task = cancel.clone();
    let ui_task = ui.clone();
    let state_task = state.clone();

    std::thread::Builder::new()
        .name("mssql-worker".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    emit(
                        &ui_task,
                        &state_task,
                        UiEvent::Failed(format!("运行时初始化失败：{}", e)),
                    );
                    return;
                }
            };
            rt.block_on(async move {
                while let Some(cmd) = rx.recv().await {
                    handle(cmd, &ui_task, &state_task, &cancel_task).await;
                }
            });
        })
        .expect("无法启动后台线程");

    Bridge { tx, cancel }
}

async fn handle(cmd: Cmd, ui: &Weak<AppWindow>, state: &SharedState, cancel: &Arc<AtomicBool>) {
    match cmd {
        Cmd::Connect(cs) => {
            emit(ui, state, UiEvent::Status("正在连接…".into()));
            match conn::connect(&cs).await {
                Ok(mut c) => {
                    let info = conn::server_info(&mut c).await.unwrap_or_default();
                    let dbs = meta::list_databases(&mut c).await.unwrap_or_default();
                    let bdir = backup::default_backup_dir(&mut c).await;
                    drop(c);
                    emit(ui, state, UiEvent::Connected { info });
                    emit(ui, state, UiEvent::Databases(dbs));
                    if let Some(d) = bdir {
                        emit(ui, state, UiEvent::BackupDir(d));
                    }
                    emit(ui, state, UiEvent::Log(format!("✓ 已连接 {}", cs.server)));
                }
                Err(e) => {
                    emit(ui, state, UiEvent::Status("连接失败".into()));
                    emit(ui, state, UiEvent::Failed(format!("{:#}", e)));
                }
            }
        }
        Cmd::Test(cs) => {
            emit(ui, state, UiEvent::Status("正在测试连接…".into()));
            match conn::connect(&cs).await {
                Ok(mut c) => {
                    let info = conn::server_info(&mut c).await.unwrap_or_default();
                    drop(c);
                    emit(ui, state, UiEvent::Status("连接正常".into()));
                    emit(ui, state, UiEvent::Log(format!("✓ 连接测试成功：{}", info)));
                    emit(ui, state, UiEvent::Connected { info });
                }
                Err(e) => {
                    emit(ui, state, UiEvent::Status("连接失败".into()));
                    emit(ui, state, UiEvent::Failed(format!("{:#}", e)));
                }
            }
        }
        Cmd::Disconnect => {
            emit(ui, state, UiEvent::Disconnected);
            emit(ui, state, UiEvent::Log("已断开连接".into()));
        }
        Cmd::ListDatabases(cs) => match conn::connect(&cs).await {
            Ok(mut c) => {
                if let Ok(dbs) = meta::list_databases(&mut c).await {
                    emit(ui, state, UiEvent::Databases(dbs));
                }
            }
            Err(e) => emit(ui, state, UiEvent::Failed(format!("{:#}", e))),
        },
        Cmd::LoadObjects { conn: cs, db } => {
            let mut cs = cs;
            cs.database = db;
            emit(ui, state, UiEvent::Status("正在读取对象列表…".into()));
            match conn::connect(&cs).await {
                Ok(mut c) => match meta::list_objects(&mut c).await {
                    Ok(objs) => {
                        emit(
                            ui,
                            state,
                            UiEvent::Log(format!("读取到 {} 个表 / 视图", objs.len())),
                        );
                        emit(ui, state, UiEvent::Objects(objs));
                        emit(ui, state, UiEvent::Status("就绪".into()));
                    }
                    Err(e) => emit(ui, state, UiEvent::Failed(format!("{:#}", e))),
                },
                Err(e) => emit(ui, state, UiEvent::Failed(format!("{:#}", e))),
            }
        }
        Cmd::Export {
            conn: cs,
            settings,
            tables,
        } => {
            cancel.store(false, Ordering::Relaxed);
            emit(ui, state, UiEvent::Status("导出中…".into()));
            let mut c = match conn::connect(&cs).await {
                Ok(c) => c,
                Err(e) => {
                    emit(ui, state, UiEvent::Failed(format!("{:#}", e)));
                    return;
                }
            };
            let ctx = make_ctx(ui, state, cancel);
            let res = if settings.format == ExportFormat::Excel {
                xlsx_out::run(&mut c, &settings, &tables, &ctx).await
            } else {
                export::run(&mut c, &settings, &tables, &cs.database, &ctx).await
            };
            match res {
                Ok(s) => emit(ui, state, UiEvent::Done(s)),
                Err(e) => emit(ui, state, UiEvent::Failed(format!("{:#}", e))),
            }
        }
        Cmd::Backup { conn: cs, settings } => {
            cancel.store(false, Ordering::Relaxed);
            emit(ui, state, UiEvent::Status("备份中…".into()));
            let mut c = match conn::connect(&cs).await {
                Ok(c) => c,
                Err(e) => {
                    emit(ui, state, UiEvent::Failed(format!("{:#}", e)));
                    return;
                }
            };
            let ctx = make_ctx(ui, state, cancel);
            match backup::run(&mut c, &settings, &cs.database, &ctx).await {
                Ok(s) => emit(ui, state, UiEvent::Done(s)),
                Err(e) => emit(ui, state, UiEvent::Failed(format!("{:#}", e))),
            }
        }
        Cmd::BackupDir { conn: cs } => match conn::connect(&cs).await {
            Ok(mut c) => {
                if let Some(d) = backup::default_backup_dir(&mut c).await {
                    emit(ui, state, UiEvent::BackupDir(d));
                }
            }
            Err(e) => emit(ui, state, UiEvent::Failed(format!("{:#}", e))),
        },
        Cmd::DumpSchema {
            conn: cs,
            settings,
            tables,
        } => {
            cancel.store(false, Ordering::Relaxed);
            emit(ui, state, UiEvent::Status("正在导出结构…".into()));
            let mut c = match conn::connect(&cs).await {
                Ok(c) => c,
                Err(e) => {
                    emit(ui, state, UiEvent::Failed(format!("{:#}", e)));
                    return;
                }
            };
            let ctx = make_ctx(ui, state, cancel);
            match schema::run(&mut c, &settings, &tables, &ctx).await {
                Ok(s) => emit(ui, state, UiEvent::Done(s)),
                Err(e) => emit(ui, state, UiEvent::Failed(format!("{:#}", e))),
            }
        }
        Cmd::PreviewSchema {
            conn: cs,
            settings,
            tables,
        } => {
            emit(ui, state, UiEvent::Status("正在生成结构预览…".into()));
            match conn::connect(&cs).await {
                Ok(mut c) => match schema::preview(&mut c, &settings, &tables, 120).await {
                    Ok(t) => {
                        emit(ui, state, UiEvent::SchemaPreview(t));
                        emit(ui, state, UiEvent::Status("就绪".into()));
                    }
                    Err(e) => {
                        emit(
                            ui,
                            state,
                            UiEvent::SchemaPreview(format!("预览失败：{:#}", e)),
                        );
                        emit(ui, state, UiEvent::Status("就绪".into()));
                    }
                },
                Err(e) => emit(ui, state, UiEvent::Failed(format!("{:#}", e))),
            }
        }
        Cmd::BackupHistory { conn: cs, db } => {
            emit(ui, state, UiEvent::Status("正在读取备份历史…".into()));
            match conn::connect(&cs).await {
                Ok(mut c) => match backup::history(&mut c, &db).await {
                    Ok(t) => {
                        emit(ui, state, UiEvent::BackupInfo(t));
                        emit(ui, state, UiEvent::Status("就绪".into()));
                    }
                    Err(e) => emit(
                        ui,
                        state,
                        UiEvent::BackupInfo(format!("读取备份历史失败：{:#}", e)),
                    ),
                },
                Err(e) => emit(ui, state, UiEvent::Failed(format!("{:#}", e))),
            }
        }
        Cmd::ScanImport { settings, pattern } => {
            emit(ui, state, UiEvent::Status("正在扫描源文件…".into()));
            match import::scan_plan(&settings, &pattern) {
                Ok(items) => {
                    emit(
                        ui,
                        state,
                        UiEvent::Log(format!("扫描完成：{} 个导入任务", items.len())),
                    );
                    emit(ui, state, UiEvent::ImportPlan(items));
                    emit(ui, state, UiEvent::Status("就绪".into()));
                }
                Err(e) => {
                    emit(ui, state, UiEvent::Failed(format!("{:#}", e)));
                    emit(ui, state, UiEvent::Status("就绪".into()));
                }
            }
        }
        Cmd::Import {
            conn: cs,
            settings,
            items,
        } => {
            cancel.store(false, Ordering::Relaxed);
            emit(ui, state, UiEvent::Status("导入中…".into()));
            let mut c = match conn::connect(&cs).await {
                Ok(c) => c,
                Err(e) => {
                    emit(ui, state, UiEvent::Failed(format!("{:#}", e)));
                    return;
                }
            };
            let ctx = make_ctx(ui, state, cancel);
            match import::run(&mut c, &settings, &items, &ctx).await {
                Ok(s) => emit(ui, state, UiEvent::Done(s)),
                Err(e) => emit(ui, state, UiEvent::Failed(format!("{:#}", e))),
            }
        }
        Cmd::PreviewImport {
            settings,
            path,
            sheet,
        } => {
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                import::preview(&path, &settings, sheet.as_deref(), 12)
            }));
            match res {
                Ok(Ok(t)) => emit(ui, state, UiEvent::Preview(t)),
                Ok(Err(e)) => emit(ui, state, UiEvent::Preview(format!("预览失败：{:#}", e))),
                Err(_) => emit(
                    ui,
                    state,
                    UiEvent::Preview("预览失败：无法解析文件".to_string()),
                ),
            }
        }
        Cmd::PreviewSql {
            conn: cs,
            settings,
            tables,
        } => {
            emit(ui, state, UiEvent::Status("正在生成预览…".into()));
            match conn::connect(&cs).await {
                Ok(mut c) => match import::preview_sql(&mut c, &settings, &tables, 20).await {
                    Ok(t) => emit(ui, state, UiEvent::Preview(t)),
                    Err(e) => emit(ui, state, UiEvent::Preview(format!("预览失败：{:#}", e))),
                },
                Err(e) => emit(ui, state, UiEvent::Failed(format!("{:#}", e))),
            }
        }
        Cmd::Cancel => {
            cancel.store(true, Ordering::Relaxed);
            emit(ui, state, UiEvent::Log("⏹ 已请求取消…".into()));
        }
    }
}
