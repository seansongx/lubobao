//! SQLite 存储：任务、动态和设置。用 `PRAGMA user_version` 做顺序迁移。

use std::path::Path;

use rusqlite::{Connection, OpenFlags, params, params_from_iter, types::Value as SqlValue};
use serde_json::{Map, Value};

use super::activity::{
    ActivityEntry, ActivityKind, ActivityPage, ActivityQuery, DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE,
    NewActivity,
};
use super::model::{LiveState, Monitoring, RecordState, Task, TaskConfig, TaskId, now_ms};
use crate::platforms::{Platform, Quality};

/// 每一项把数据库从版本 i 升到 i + 1。只追加，不修改已发布的迁移。
const MIGRATIONS: &[&str] = &[r#"
    CREATE TABLE tasks (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        url TEXT NOT NULL,
        platform TEXT NOT NULL,
        room_id TEXT NOT NULL,
        anchor_id TEXT,
        anchor_name TEXT NOT NULL,
        live_url TEXT,
        quality TEXT NOT NULL,
        notify_only INTEGER NOT NULL DEFAULT 0,
        note TEXT NOT NULL DEFAULT '',
        created_at INTEGER NOT NULL,
        monitoring TEXT NOT NULL DEFAULT 'enabled',
        record_state TEXT NOT NULL DEFAULT '{"state":"idle"}'
    );
    CREATE TABLE activity (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        at INTEGER NOT NULL,
        task_id INTEGER,
        anchor_name TEXT NOT NULL,
        kind TEXT NOT NULL,
        message TEXT NOT NULL,
        detail TEXT
    );
    CREATE INDEX activity_task ON activity (task_id, id);
    CREATE INDEX activity_kind ON activity (kind, id);
    CREATE TABLE settings (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
"#];

#[derive(Debug)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        StoreError(format!("数据库错误：{error}"))
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(error: serde_json::Error) -> Self {
        StoreError(format!("数据格式错误：{error}"))
    }
}

pub type StoreResult<T> = Result<T, StoreError>;

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> StoreResult<Store> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| StoreError(format!("无法创建数据目录：{error}")))?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let mut store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    /// 只读打开，不建文件、不迁移、不写入。文件不存在时返回一个空的内存库；数据库版本与
    /// 程序不一致（需要迁移）时报错。命令行程序在 `run` 没有运行时给只读命令用。
    pub fn open_read_only(path: &Path) -> StoreResult<Store> {
        if !path.exists() {
            let mut store = Store {
                conn: Connection::open_in_memory()?,
            };
            store.migrate()?;
            return Ok(store);
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let store = Store { conn };
        let version = store.schema_version()?;
        if version != MIGRATIONS.len() {
            return Err(StoreError(format!(
                "数据库版本 {version} 与当前程序的 {} 不一致，需要先以读写方式打开一次完成升级",
                MIGRATIONS.len()
            )));
        }
        Ok(store)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> StoreResult<Store> {
        let mut store = Store {
            conn: Connection::open_in_memory()?,
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn schema_version(&self) -> StoreResult<usize> {
        let version: i64 = self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        Ok(version as usize)
    }

    fn migrate(&mut self) -> StoreResult<()> {
        let current = self.schema_version()?;
        if current > MIGRATIONS.len() {
            return Err(StoreError(format!(
                "数据库版本 {current} 比当前程序支持的 {} 新，请升级录播宝",
                MIGRATIONS.len()
            )));
        }
        for (index, sql) in MIGRATIONS.iter().enumerate().skip(current) {
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", (index + 1) as i64)?;
            tx.commit()?;
        }
        Ok(())
    }

    /// 只读访问，以及不需要事务的单条写入。
    pub fn db(&self) -> Db<'_> {
        Db { conn: &self.conn }
    }

    /// 在一个事务里执行 `f`。`f` 返回错误时回滚，数据库保持原样。
    pub fn transaction<T>(&mut self, f: impl FnOnce(&Db<'_>) -> StoreResult<T>) -> StoreResult<T> {
        let tx = self.conn.transaction()?;
        let value = f(&Db { conn: &tx })?;
        tx.commit()?;
        Ok(value)
    }
}

/// 一行无法读取的任务，启动时跳过。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedRow {
    pub id: TaskId,
    pub reason: String,
}

/// 绑定到连接或事务上的读写操作。
pub struct Db<'a> {
    conn: &'a Connection,
}

impl Db<'_> {
    #[cfg(test)]
    pub fn execute_batch(&self, sql: &str) -> StoreResult<()> {
        Ok(self.conn.execute_batch(sql)?)
    }

    // ---- 任务 ----

    pub fn insert_task(&self, config: &TaskConfig, monitoring: Monitoring) -> StoreResult<TaskId> {
        self.conn.execute(
            "INSERT INTO tasks (url, platform, room_id, anchor_id, anchor_name, live_url, quality,
                notify_only, note, created_at, monitoring, record_state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                config.url,
                config.platform.key(),
                config.room_id,
                config.anchor_id,
                config.anchor_name,
                config.live_url,
                config.quality.code(),
                config.notify_only,
                config.note,
                config.created_at,
                monitoring.as_str(),
                serde_json::to_string(&RecordState::Idle)?,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn update_task_config(&self, id: TaskId, config: &TaskConfig) -> StoreResult<()> {
        self.conn.execute(
            "UPDATE tasks SET url = ?2, platform = ?3, room_id = ?4, anchor_id = ?5,
                anchor_name = ?6, live_url = ?7, quality = ?8, notify_only = ?9, note = ?10
             WHERE id = ?1",
            params![
                id,
                config.url,
                config.platform.key(),
                config.room_id,
                config.anchor_id,
                config.anchor_name,
                config.live_url,
                config.quality.code(),
                config.notify_only,
                config.note,
            ],
        )?;
        Ok(())
    }

    pub fn set_monitoring(&self, id: TaskId, monitoring: Monitoring) -> StoreResult<()> {
        self.conn.execute(
            "UPDATE tasks SET monitoring = ?2 WHERE id = ?1",
            params![id, monitoring.as_str()],
        )?;
        Ok(())
    }

    pub fn set_record_state(&self, id: TaskId, record: &RecordState) -> StoreResult<()> {
        self.conn.execute(
            "UPDATE tasks SET record_state = ?2 WHERE id = ?1",
            params![id, serde_json::to_string(record)?],
        )?;
        Ok(())
    }

    pub fn delete_task(&self, id: TaskId) -> StoreResult<()> {
        self.conn
            .execute("DELETE FROM tasks WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// 按 id 升序读出全部任务。直播状态不持久化，读出时为 `Unknown`。
    /// 无法识别的行（例如更新版本写入的未知平台）跳过并在第二个返回值中列出。
    pub fn load_tasks(&self) -> StoreResult<(Vec<Task>, Vec<SkippedRow>)> {
        let mut statement = self.conn.prepare(
            "SELECT id, url, platform, room_id, anchor_id, anchor_name, live_url, quality,
                notify_only, note, created_at, monitoring, record_state
             FROM tasks ORDER BY id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, TaskId>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, bool>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, i64>(10)?,
                row.get::<_, String>(11)?,
                row.get::<_, String>(12)?,
            ))
        })?;
        let mut tasks = Vec::new();
        let mut skipped = Vec::new();
        for row in rows {
            let (
                id,
                url,
                platform,
                room_id,
                anchor_id,
                anchor_name,
                live_url,
                quality,
                notify_only,
                note,
                created_at,
                monitoring,
                record_state,
            ) = row?;
            let platform: Platform = match serde_json::from_value(Value::String(platform.clone())) {
                Ok(platform) => platform,
                Err(_) => {
                    skipped.push(SkippedRow {
                        id,
                        reason: format!("未知平台 {platform}"),
                    });
                    continue;
                }
            };
            let record = serde_json::from_str(&record_state).unwrap_or(RecordState::Idle);
            tasks.push(Task {
                id,
                config: TaskConfig {
                    url,
                    platform,
                    room_id,
                    anchor_id,
                    anchor_name,
                    live_url,
                    quality: Quality::parse_lenient(&quality),
                    notify_only,
                    note,
                    created_at,
                },
                monitoring: Monitoring::parse(&monitoring),
                live: LiveState::Unknown,
                record,
                held: false,
            });
        }
        Ok((tasks, skipped))
    }

    // ---- 动态 ----

    pub fn insert_activity(&self, entry: &NewActivity) -> StoreResult<ActivityEntry> {
        let at = now_ms();
        let detail = entry
            .detail
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        self.conn.execute(
            "INSERT INTO activity (at, task_id, anchor_name, kind, message, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                at,
                entry.task_id,
                entry.anchor_name,
                entry.kind.as_str(),
                entry.message,
                detail
            ],
        )?;
        Ok(ActivityEntry {
            id: self.conn.last_insert_rowid(),
            at,
            task_id: entry.task_id,
            anchor_name: entry.anchor_name.clone(),
            kind: entry.kind,
            message: entry.message.clone(),
            detail: entry.detail.clone(),
        })
    }

    pub fn list_activity(&self, query: &ActivityQuery) -> StoreResult<ActivityPage> {
        let limit = query
            .limit
            .unwrap_or(DEFAULT_PAGE_SIZE)
            .clamp(1, MAX_PAGE_SIZE);
        let mut sql = String::from(
            "SELECT id, at, task_id, anchor_name, kind, message, detail FROM activity WHERE 1 = 1",
        );
        let mut args: Vec<SqlValue> = Vec::new();
        if let Some(task_id) = query.task_id {
            sql.push_str(" AND task_id = ?");
            args.push(SqlValue::Integer(task_id));
        }
        if let Some(before) = query.before {
            sql.push_str(" AND id < ?");
            args.push(SqlValue::Integer(before));
        }
        if !query.kinds.is_empty() {
            let marks = vec!["?"; query.kinds.len()].join(", ");
            if query.kinds.contains(&ActivityKind::Convert) {
                // 「转为 MP4」也包括转换的失败和提醒（detail.kind 为 convert），让一次转换的
                // 结局在这个筛选下看得到；它们仍分别出现在「失败」「提醒」里。
                sql.push_str(&format!(
                    " AND (kind IN ({marks}) OR (json_valid(detail) \
                     AND json_extract(detail, '$.kind') = 'convert'))"
                ));
            } else {
                sql.push_str(&format!(" AND kind IN ({marks})"));
            }
            args.extend(
                query
                    .kinds
                    .iter()
                    .map(|kind| SqlValue::Text(kind.as_str().to_string())),
            );
        }
        // 多取一条判断是否还有下一页。
        sql.push_str(" ORDER BY id DESC LIMIT ?");
        args.push(SqlValue::Integer(i64::from(limit) + 1));

        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(args), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<TaskId>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })?;
        let mut items = Vec::new();
        for row in rows {
            let (id, at, task_id, anchor_name, kind, message, detail) = row?;
            let Some(kind) = ActivityKind::parse(&kind) else {
                continue;
            };
            items.push(ActivityEntry {
                id,
                at,
                task_id,
                anchor_name,
                kind,
                message,
                detail: detail.and_then(|text| serde_json::from_str(&text).ok()),
            });
        }
        let next_before = if items.len() > limit as usize {
            items.truncate(limit as usize);
            items.last().map(|entry| entry.id)
        } else {
            None
        };
        Ok(ActivityPage { items, next_before })
    }

    // ---- 设置 ----

    /// 用户改过的设置项。
    pub fn load_settings(&self) -> StoreResult<Map<String, Value>> {
        let mut statement = self.conn.prepare("SELECT key, value FROM settings")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut map = Map::new();
        for row in rows {
            let (key, value) = row?;
            if let Ok(value) = serde_json::from_str(&value) {
                map.insert(key, value);
            }
        }
        Ok(map)
    }

    pub fn save_settings(&self, values: &Map<String, Value>) -> StoreResult<()> {
        for (key, value) in values {
            self.conn.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                params![key, serde_json::to_string(value)?],
            )?;
        }
        Ok(())
    }
}
