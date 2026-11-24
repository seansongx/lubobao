//! HLS master playlist parsing shared by adapters whose platform hands out a
//! master playlist (Twitch, YouTube). StreamGet's `get_play_url_list` reads
//! the variant URLs and their `BANDWIDTH` and sorts them best first; this
//! also keeps the resolution and name so the variants can be shown.

use reqwest::Url;

/// One `#EXT-X-STREAM-INF` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct MasterVariant {
    /// Absolute URL of the media playlist.
    pub url: String,
    pub bandwidth: u64,
    pub resolution: Option<(u32, u32)>,
    pub frame_rate: Option<f64>,
    /// `NAME` of the `#EXT-X-MEDIA` group named by `VIDEO`, else the `VIDEO`
    /// group id itself.
    pub name: Option<String>,
    /// Audio only: Twitch's `audio_only` group, or no resolution and only
    /// audio codecs.
    pub audio_only: bool,
}

impl MasterVariant {
    /// Name to show: the group name, else `{height}p`, else the bandwidth.
    pub fn label(&self) -> String {
        if let Some(name) = &self.name {
            return name.clone();
        }
        match self.resolution {
            Some((_, height)) => format!("{height}p"),
            None => format!("{} kbps", self.bandwidth / 1000),
        }
    }
}

/// Split an attribute list (`KEY=VALUE,KEY="a,b"`) into pairs, unquoting
/// values.
pub fn attributes(list: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut rest = list.trim();
    while !rest.is_empty() {
        let Some(eq) = rest.find('=') else {
            break;
        };
        let key = rest[..eq].trim().to_ascii_uppercase();
        let after = &rest[eq + 1..];
        let (value, next) = if let Some(quoted) = after.strip_prefix('"') {
            match quoted.find('"') {
                Some(end) => (&quoted[..end], &quoted[end + 1..]),
                None => (quoted, ""),
            }
        } else {
            match after.find(',') {
                Some(end) => (&after[..end], &after[end..]),
                None => (after, ""),
            }
        };
        pairs.push((key, value.to_string()));
        rest = next.trim_start_matches(',').trim_start();
    }
    pairs
}

fn attribute<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}

/// The variants of a master playlist, highest `BANDWIDTH` first (StreamGet
/// sorts the same way). Relative URIs are resolved against `base`. A media
/// playlist (no `#EXT-X-STREAM-INF`) gives an empty list.
pub fn parse_master(body: &str, base: &Url) -> Vec<MasterVariant> {
    let mut group_names: Vec<(String, String)> = Vec::new();
    let mut pending: Option<Vec<(String, String)>> = None;
    let mut variants = Vec::new();
    for line in body.lines().map(str::trim) {
        if let Some(list) = line.strip_prefix("#EXT-X-MEDIA:") {
            let pairs = attributes(list);
            if let (Some(group), Some(name)) =
                (attribute(&pairs, "GROUP-ID"), attribute(&pairs, "NAME"))
            {
                group_names.push((group.to_string(), name.to_string()));
            }
        } else if let Some(list) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending = Some(attributes(list));
        } else if line.is_empty() || line.starts_with('#') {
            continue;
        } else if let Some(pairs) = pending.take() {
            let Ok(url) = base.join(line) else {
                continue;
            };
            let resolution = attribute(&pairs, "RESOLUTION").and_then(|value| {
                let (width, height) = value.split_once(['x', 'X'])?;
                Some((width.trim().parse().ok()?, height.trim().parse().ok()?))
            });
            let video = attribute(&pairs, "VIDEO").map(str::to_string);
            let name = video.as_ref().map(|group| {
                group_names
                    .iter()
                    .rev()
                    .find(|(id, _)| id == group)
                    .map_or_else(|| group.clone(), |(_, name)| name.clone())
            });
            let codecs = attribute(&pairs, "CODECS").unwrap_or_default();
            let audio_codecs_only = !codecs.is_empty()
                && codecs
                    .split(',')
                    .all(|codec| codec.trim().starts_with("mp4a") || codec.trim() == "ac-3");
            variants.push(MasterVariant {
                url: url.to_string(),
                bandwidth: attribute(&pairs, "BANDWIDTH")
                    .and_then(|value| value.parse().ok())
                    .unwrap_or_default(),
                resolution,
                frame_rate: attribute(&pairs, "FRAME-RATE").and_then(|value| value.parse().ok()),
                audio_only: video.as_deref() == Some("audio_only")
                    || (resolution.is_none() && audio_codecs_only),
                name,
            });
        }
    }
    variants.sort_by_key(|variant| std::cmp::Reverse(variant.bandwidth));
    variants
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_keep_quoted_commas() {
        assert_eq!(
            attributes(r#"BANDWIDTH=1,CODECS="avc1.4D401F,mp4a.40.2",VIDEO="720p60""#),
            vec![
                ("BANDWIDTH".to_string(), "1".to_string()),
                ("CODECS".to_string(), "avc1.4D401F,mp4a.40.2".to_string()),
                ("VIDEO".to_string(), "720p60".to_string()),
            ]
        );
    }

    #[test]
    fn variants_are_sorted_by_bandwidth_and_named_by_group() {
        let body = "#EXTM3U\n\
            #EXT-X-MEDIA:TYPE=VIDEO,GROUP-ID=\"chunked\",NAME=\"1080p60 (source)\"\n\
            #EXT-X-STREAM-INF:BANDWIDTH=500,RESOLUTION=640x360,CODECS=\"avc1.4D401F,mp4a.40.2\",VIDEO=\"360p30\"\n\
            low.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=9000,RESOLUTION=1920x1080,CODECS=\"avc1.64002A,mp4a.40.2\",VIDEO=\"chunked\",FRAME-RATE=60.000\n\
            https://cdn.example/source.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=160,CODECS=\"mp4a.40.2\"\n\
            audio.m3u8\n";
        let base = Url::parse("https://host.example/master/list.m3u8?x=1").unwrap();
        let variants = parse_master(body, &base);
        assert_eq!(variants.len(), 3);
        assert_eq!(variants[0].url, "https://cdn.example/source.m3u8");
        assert_eq!(variants[0].label(), "1080p60 (source)");
        assert_eq!(variants[0].frame_rate, Some(60.0));
        assert_eq!(variants[1].url, "https://host.example/master/low.m3u8");
        assert_eq!(variants[1].label(), "360p30");
        assert_eq!(variants[1].resolution, Some((640, 360)));
        assert!(variants[2].audio_only);
        assert!(!variants[0].audio_only);
        assert_eq!(variants[2].label(), "0 kbps");
    }

    #[test]
    fn media_playlists_have_no_variants() {
        let base = Url::parse("https://host.example/a.m3u8").unwrap();
        assert!(parse_master("#EXTM3U\n#EXTINF:2.0,\nseg1.ts\n", &base).is_empty());
    }
}
