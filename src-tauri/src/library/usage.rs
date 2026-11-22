//! 录像目录占用的缓存。不依赖 Tauri，命令行程序也可以直接用。
//!
//! 递归遍历可能很慢（NAS、整块磁盘、几年的分段文件），所以：
//! - 结果带时间戳缓存，有效期内直接返回；
//! - 同一时间只有一次遍历，在独立线程里执行；遍历期间再来的请求返回上次的结果，并标明正在统计；
//! - 调用方可以等待一小段时间，遍历很快时一次请求就拿到新结果。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use super::{LibraryUsage, absolute_root, walk_usage};

/// 缓存有效期。
pub const DEFAULT_TTL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSnapshot {
    pub root: String,
    pub root_exists: bool,
    /// 最近一次针对这个根目录的统计结果；还没统计完过时为 None。
    pub usage: Option<LibraryUsage>,
    /// `usage` 的统计完成时间，Unix 毫秒。
    pub computed_at: Option<i64>,
    /// 正在统计，`usage` 可能已经过时。
    pub computing: bool,
}

struct Cached {
    root: PathBuf,
    usage: LibraryUsage,
    computed_at_ms: i64,
    computed_at: Instant,
}

#[derive(Default)]
struct State {
    cached: Option<Cached>,
    /// 正在遍历的根目录。
    running: Option<PathBuf>,
}

pub type Walker = fn(&Path) -> LibraryUsage;

pub struct UsageCache {
    state: Mutex<State>,
    finished: Condvar,
    walker: Walker,
    ttl: Duration,
}

impl Default for UsageCache {
    fn default() -> Self {
        UsageCache::with_walker(walk_usage, DEFAULT_TTL)
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

impl UsageCache {
    /// 测试时可以换成可控的遍历函数。
    pub fn with_walker(walker: Walker, ttl: Duration) -> Self {
        UsageCache {
            state: Mutex::new(State::default()),
            finished: Condvar::new(),
            walker,
            ttl,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 返回占用快照。缓存不是这个根目录的、已过期或 `refresh` 时，在没有遍历进行时启动一次；
    /// 然后最多等待 `wait`，等这个根目录的遍历结束。
    pub fn request(self: &Arc<Self>, root: &Path, refresh: bool, wait: Duration) -> UsageSnapshot {
        let root = absolute_root(root);
        let root_exists = root.is_dir();
        let mut state = self.lock();

        let fresh = |state: &State| {
            state
                .cached
                .as_ref()
                .is_some_and(|c| c.root == root && c.computed_at.elapsed() < self.ttl)
        };
        if root_exists && state.running.is_none() && (refresh || !fresh(&state)) {
            state.running = Some(root.clone());
            let this = Arc::clone(self);
            let target = root.clone();
            let spawned = std::thread::Builder::new()
                .name("library-usage".into())
                .spawn(move || {
                    let usage = (this.walker)(&target);
                    let mut state = this.lock();
                    state.cached = Some(Cached {
                        root: target,
                        usage,
                        computed_at_ms: now_ms(),
                        computed_at: Instant::now(),
                    });
                    state.running = None;
                    this.finished.notify_all();
                });
            if spawned.is_err() {
                state.running = None;
            }
        }

        let deadline = Instant::now() + wait;
        while state.running.as_deref() == Some(root.as_path()) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            state = self
                .finished
                .wait_timeout(state, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }

        let cached = state.cached.as_ref().filter(|c| c.root == root);
        // 正在遍历这个根目录；或者正在遍历别的根目录、这个根目录还没有新结果（下一次请求会启动）。
        let computing = root_exists
            && match state.running.as_deref() {
                Some(running) => running == root.as_path() || !fresh(&state),
                None => false,
            };
        UsageSnapshot {
            root: root.to_string_lossy().into_owned(),
            root_exists,
            usage: if root_exists {
                cached.map(|c| c.usage)
            } else {
                None
            },
            computed_at: if root_exists {
                cached.map(|c| c.computed_at_ms)
            } else {
                None
            },
            computing,
        }
    }
}
