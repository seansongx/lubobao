//! 重复任务检测。
//!
//! 同一平台下 `anchor_id` 相同，或者归一化后的链接相同（用户输入的链接、平台返回的
//! 规范直播间地址任意两两相同），视为同一个任务。`room_id` 不参与比较：同一主播在
//! 不同链接形式下 `room_id` 可能不同。

use reqwest::Url;

use super::model::{Task, TaskConfig, TaskId};
use crate::platforms::adapter_for;

use super::input_link;

/// 归一化链接（分享文字先取出其中的链接）：优先用平台适配器的规则，其次做通用处理（补 https、小写主机名、
/// 去掉片段和末尾斜杠、查询参数排序）。无法解析时返回去掉首尾空白的原文。
pub fn normalize_url(url: &str) -> String {
    let trimmed = input_link(url);
    if let Some(normalized) =
        adapter_for(trimmed).and_then(|adapter| adapter.normalize_url(trimmed))
    {
        return normalized;
    }
    generic_normalize(trimmed).unwrap_or_else(|| trimmed.to_string())
}

fn generic_normalize(url: &str) -> Option<String> {
    let with_scheme = if url.contains("://") {
        url.to_string()
    } else {
        format!("https://{url}")
    };
    let mut parsed = Url::parse(&with_scheme).ok()?;
    if parsed.scheme() == "http" {
        parsed.set_scheme("https").ok()?;
    }
    parsed.set_fragment(None);
    let mut pairs: Vec<(String, String)> = parsed
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    pairs.sort();
    if pairs.is_empty() {
        parsed.set_query(None);
    } else {
        parsed.query_pairs_mut().clear().extend_pairs(pairs);
    }
    let path = parsed.path().trim_end_matches('/').to_string();
    parsed.set_path(&path);
    Some(parsed.to_string().trim_end_matches('/').to_string())
}

fn config_urls(config: &TaskConfig) -> Vec<String> {
    let mut urls = vec![normalize_url(&config.url)];
    if let Some(live_url) = &config.live_url {
        urls.push(normalize_url(live_url));
    }
    urls
}

/// `candidate` 与哪些已有任务重复。`exclude` 用于编辑时排除任务自身。
pub fn find_duplicates<'a>(
    tasks: impl IntoIterator<Item = &'a Task>,
    candidate: &TaskConfig,
    exclude: Option<TaskId>,
) -> Vec<TaskId> {
    let candidate_urls = config_urls(candidate);
    tasks
        .into_iter()
        .filter(|task| Some(task.id) != exclude)
        .filter(|task| {
            let same_anchor = task.config.platform == candidate.platform
                && candidate.anchor_id.is_some()
                && task.config.anchor_id == candidate.anchor_id;
            same_anchor
                || config_urls(&task.config)
                    .iter()
                    .any(|url| candidate_urls.contains(url))
        })
        .map(|task| task.id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platforms::{Platform, Quality};
    use crate::tasks::model::{LiveState, Monitoring, RecordState};

    fn config(url: &str, anchor_id: Option<&str>, live_url: Option<&str>) -> TaskConfig {
        TaskConfig {
            url: url.into(),
            platform: Platform::Douyin,
            room_id: "r".into(),
            anchor_id: anchor_id.map(Into::into),
            anchor_name: "主播".into(),
            live_url: live_url.map(Into::into),
            quality: Quality::Od,
            notify_only: false,
            note: String::new(),
            created_at: 0,
        }
    }

    fn task(id: TaskId, config: TaskConfig) -> Task {
        Task {
            id,
            config,
            monitoring: Monitoring::Enabled,
            live: LiveState::Unknown,
            record: RecordState::Idle,
            held: false,
        }
    }

    #[test]
    fn normalizes_douyin_links_with_the_adapter_rules() {
        assert_eq!(
            normalize_url(" http://live.douyin.com/745964462470/?from=share&anchor_id=1 "),
            "https://live.douyin.com/745964462470"
        );
        assert_eq!(
            normalize_url("v.douyin.com/CeiU5cbX"),
            "https://v.douyin.com/CeiU5cbX/"
        );
    }

    #[test]
    fn normalizes_share_texts_to_the_link_inside() {
        assert_eq!(
            normalize_url("【主播的直播】 https://v.douyin.com/CeiU5cbX 复制此链接"),
            "https://v.douyin.com/CeiU5cbX/"
        );
    }

    #[test]
    fn equivalent_yy_and_acfun_links_normalize_the_same() {
        assert_eq!(
            normalize_url("https://www.yy.com/22490906/22490906"),
            normalize_url("yy.com/22490906")
        );
        assert_ne!(
            normalize_url("https://www.yy.com/22490906/1"),
            normalize_url("https://www.yy.com/22490906")
        );
        assert_eq!(
            normalize_url("https://live.acfun.cn/live/0179922"),
            normalize_url("https://live.acfun.cn/live/179922")
        );
        let tasks = vec![task(
            1,
            config("https://www.yy.com/22490906/22490906", None, None),
        )];
        let candidate = config("https://www.yy.com/22490906", None, None);
        assert_eq!(find_duplicates(&tasks, &candidate, None), vec![1]);
        let tasks = vec![task(
            2,
            config("https://live.acfun.cn/live/179922", None, None),
        )];
        let candidate = config("live.acfun.cn/live/0179922", None, None);
        assert_eq!(find_duplicates(&tasks, &candidate, None), vec![2]);
    }

    #[test]
    fn normalizes_other_links_generically() {
        assert_eq!(
            normalize_url("HTTP://Example.COM/room/1/?b=2&a=1#top"),
            "https://example.com/room/1?a=1&b=2"
        );
        assert_eq!(
            normalize_url("example.com/room/1"),
            "https://example.com/room/1"
        );
    }

    #[test]
    fn detects_duplicate_by_anchor_id() {
        let tasks = vec![task(
            1,
            config(
                "https://live.douyin.com/745964462470",
                Some("MS4w-sec"),
                None,
            ),
        )];
        let short = config("https://v.douyin.com/CeiU5cbX", Some("MS4w-sec"), None);
        assert_eq!(find_duplicates(&tasks, &short, None), vec![1]);

        let other = config("https://v.douyin.com/other", Some("MS4w-other"), None);
        assert!(find_duplicates(&tasks, &other, None).is_empty());
    }

    #[test]
    fn detects_duplicate_by_normalized_url() {
        let tasks = vec![task(
            1,
            config("https://live.douyin.com/745964462470", None, None),
        )];
        let same = config("live.douyin.com/745964462470?from=share", None, None);
        assert_eq!(find_duplicates(&tasks, &same, None), vec![1]);

        // 分类页进入的链接把房间号放在 live_web_rid 参数里。
        let category = config(
            "https://live.douyin.com/?anchor_id=1&category_name=all&live_web_rid=745964462470&page_type=main_category_page",
            None,
            None,
        );
        assert_eq!(find_duplicates(&tasks, &category, None), vec![1]);

        // 用户输入的短链与平台返回的规范地址相同也算重复。
        let via_live_url = config(
            "https://v.douyin.com/abc",
            None,
            Some("https://live.douyin.com/745964462470"),
        );
        assert_eq!(find_duplicates(&tasks, &via_live_url, None), vec![1]);
    }

    #[test]
    fn excludes_the_task_being_edited_and_ignores_missing_anchor_ids() {
        let tasks = vec![
            task(1, config("https://live.douyin.com/1", None, None)),
            task(2, config("https://live.douyin.com/2", None, None)),
        ];
        let edited = config("https://live.douyin.com/1", None, None);
        assert!(find_duplicates(&tasks, &edited, Some(1)).is_empty());
        // 双方都没有 anchor_id 时不能因为 None == None 判重。
        let unrelated = config("https://live.douyin.com/3", None, None);
        assert!(find_duplicates(&tasks, &unrelated, None).is_empty());
    }
}
