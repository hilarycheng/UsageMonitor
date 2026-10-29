use std::{
    env, fs,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, Sender},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use serde_json::Value;

#[derive(Clone, PartialEq, Debug)]
pub struct Tweet {
    pub id: String,
    pub timestamp_ms: i64,
    pub text: String,
}

fn state_dir() -> Result<PathBuf> {
    Ok(
        PathBuf::from(env::var_os("LOCALAPPDATA").context("找不到本機應用程式資料目錄")?)
            .join("UsageMonitor"),
    )
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 20
        && id.parse::<u64>().is_ok()
        && id.bytes().all(|b| b.is_ascii_digit())
}

fn save(path: &Path, bytes: &[u8]) -> Result<()> {
    use windows::{
        Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        },
        core::HSTRING,
    };
    fs::create_dir_all(path.parent().context("狀態檔路徑無效")?)?;
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&temporary, bytes).context("無法儲存貼文狀態")?;
    unsafe {
        MoveFileExW(
            &HSTRING::from(temporary.as_os_str()),
            &HSTRING::from(path.as_os_str()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .context("無法更新貼文狀態檔")
}

pub fn open(id: &str) -> Result<()> {
    use windows::{
        Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
        core::{HSTRING, w},
    };
    ensure!(valid_id(id), "貼文 ID 無效");
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            &HSTRING::from(format!("https://x.com/thsottiaux/status/{id}")),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
    ensure!(result.0 as isize > 32, "無法開啟貼文原文");
    save(&state_dir()?.join("tibo-read.txt"), id.as_bytes())
}

pub fn register_protocol() -> Result<()> {
    let reg = PathBuf::from(env::var_os("SystemRoot").context("找不到 Windows 目錄")?)
        .join("System32/reg.exe");
    let command = format!("\"{}\" --open-tibo \"%1\"", env::current_exe()?.display());
    for (key, name, value) in [
        (
            r"HKCU\Software\Classes\usage-monitor-tibo",
            "/ve",
            "URL:Usage Monitor Tibo",
        ),
        (
            r"HKCU\Software\Classes\usage-monitor-tibo",
            "URL Protocol",
            "",
        ),
        (
            r"HKCU\Software\Classes\usage-monitor-tibo\shell\open\command",
            "/ve",
            command.as_str(),
        ),
    ] {
        let mut process = Command::new(&reg);
        process.args(["add", key]);
        if name == "/ve" {
            process.arg("/ve");
        } else {
            process.args(["/v", name]);
        }
        let output = process
            .args(["/t", "REG_SZ", "/d", value, "/f"])
            .creation_flags(0x08000000)
            .output()?;
        ensure!(
            output.status.success(),
            "無法註冊貼文點擊入口：{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

struct Cache {
    tweet: Tweet,
    notified: bool,
}

impl Cache {
    fn load(directory: &Path) -> Result<Option<Self>> {
        let bytes = match fs::read(directory.join("tibo.json")) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let data: Value = serde_json::from_slice(&bytes).context("貼文狀態檔損壞")?;
        let id = data["id"]
            .as_str()
            .filter(|id| valid_id(id))
            .context("貼文狀態 ID 無效")?;
        Ok(Some(Self {
            tweet: Tweet {
                id: id.into(),
                timestamp_ms: data["timestamp_ms"]
                    .as_i64()
                    .filter(|v| *v > 0)
                    .context("貼文時間無效")?,
                text: data["text"].as_str().context("貼文文字無效")?.into(),
            },
            notified: data["notified"].as_bool().context("貼文通知狀態無效")?,
        }))
    }

    fn save(&self, directory: &Path) -> Result<()> {
        save(
            &directory.join("tibo.json"),
            &serde_json::to_vec(&serde_json::json!({
                "id": self.tweet.id, "timestamp_ms": self.tweet.timestamp_ms,
                "text": self.tweet.text, "notified": self.notified,
            }))?,
        )
    }

    fn unread(&self, directory: &Path) -> Result<bool> {
        match fs::read_to_string(directory.join("tibo-read.txt")) {
            Ok(id) => Ok(id != self.tweet.id),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(error) => Err(error.into()),
        }
    }
}

fn observe(cache: &mut Option<Cache>, tweet: Tweet, directory: &Path) -> Result<()> {
    if cache
        .as_ref()
        .is_none_or(|old| tweet.timestamp_ms > old.tweet.timestamp_ms)
    {
        let new = Cache {
            tweet,
            notified: false,
        };
        new.save(directory)?;
        *cache = Some(new);
    }
    Ok(())
}

pub struct Monitor {
    stop: Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl Monitor {
    pub fn start(update: impl Fn(Tweet, bool) -> Result<()> + Send + 'static) -> Result<Self> {
        register_protocol()?;
        let directory = state_dir()?;
        let mut cache = Cache::load(&directory)?;
        if let Some(current) = &mut cache {
            current.notified = false;
            save(&directory.join("tibo-read.txt"), b"")?;
            current.save(&directory)?;
        }
        let (stop, stopped) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("tibo-monitor".into())
            .spawn(move || {
                let run = || -> Result<()> {
                    let _winrt = crate::notify::WinRt::initialize()?;
                    let mut cache = cache;
                    let mut next_fetch = Instant::now();
                    let mut next_notice = Instant::now();
                    loop {
                        if let Some(current) = &cache {
                            update(current.tweet.clone(), current.unread(&directory)?)?;
                        }
                        if stopped.recv_timeout(Duration::from_secs(1))
                            != Err(mpsc::RecvTimeoutError::Timeout)
                        {
                            return Ok(());
                        }
                        if Instant::now() >= next_fetch {
                            match fetch() {
                                Ok(tweet) => {
                                    observe(&mut cache, tweet, &directory)?;
                                }
                                Err(error) => eprintln!("Tibo 貼文更新失敗：{error:#}"),
                            }
                            next_fetch = Instant::now() + Duration::from_secs(300);
                        }
                        if let Some(current) = &mut cache {
                            if !current.notified && Instant::now() >= next_notice {
                                if !current.unread(&directory)? {
                                    current.notified = true;
                                    current.save(&directory)?;
                                } else {
                                    match crate::notify::show_tibo(&current.tweet) {
                                        Ok(()) => {
                                            current.notified = true;
                                            current.save(&directory)?;
                                        }
                                        Err(error) => eprintln!("Tibo 通知失敗：{error:#}"),
                                    }
                                    next_notice = Instant::now() + Duration::from_secs(60);
                                }
                            }
                        }
                    }
                };
                if let Err(error) = run() {
                    eprintln!("Tibo 監察停止：{error:#}");
                }
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn fetch() -> Result<Tweet> {
    let curl = PathBuf::from(env::var_os("SystemRoot").context("找不到 Windows 目錄")?)
        .join("System32/curl.exe");
    let output = Command::new(curl)
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--max-time",
            "20",
            "https://x.com/thsottiaux",
        ])
        .creation_flags(0x08000000)
        .output()
        .context("無法執行 Windows curl.exe")?;
    ensure!(
        output.status.success(),
        "讀取 X 失敗：{}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    parse(&String::from_utf8(output.stdout).context("X 回應不是 UTF-8")?)
}

fn parse(html: &str) -> Result<Tweet> {
    let mut latest = None;
    // shortcut: 依賴 X 匿名頁面的內嵌資料格式；格式改動時須更新解析。
    for (index, _) in html.match_indices("value:") {
        let Some(Ok(encoded)) = serde_json::Deserializer::from_str(&html[index + 6..])
            .into_iter::<String>()
            .next()
        else {
            continue;
        };
        let Ok(data) = serde_json::from_str::<Value>(&encoded) else {
            continue;
        };
        if let Some(timeline) = data.pointer(
            "/data/user_result_by_screen_name/result/profile_user_originals_timeline/timeline",
        ) {
            collect(timeline, &mut latest);
        }
    }
    latest.context("X 頁面未提供可驗證的 Tibo 貼文；可能被限制、需要登入或頁面格式已變更")
}

fn collect(value: &Value, latest: &mut Option<Tweet>) {
    if value.get("__typename").and_then(Value::as_str) == Some("Tweet") {
        if value
            .pointer("/core/user_results/result/core/screen_name")
            .and_then(Value::as_str)
            != Some("thsottiaux")
        {
            return;
        }
        let Some(id) = value
            .get("rest_id")
            .and_then(Value::as_str)
            .filter(|id| valid_id(id))
        else {
            return;
        };
        let Some(timestamp_ms) = value
            .pointer("/details/created_at_ms")
            .and_then(Value::as_i64)
            .filter(|time| *time > 0)
        else {
            return;
        };
        let Some(text) = value
            .pointer("/note_tweet/note_tweet_results/result/text")
            .or_else(|| value.pointer("/details/full_text"))
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
        else {
            return;
        };
        if latest
            .as_ref()
            .is_none_or(|last| timestamp_ms > last.timestamp_ms)
        {
            *latest = Some(Tweet {
                id: id.into(),
                timestamp_ms,
                text: decode_entities(text),
            });
        }
        // 引用內容不屬於這則貼文，不能當成最新貼文候選。
        return;
    }
    match value {
        Value::Object(fields) => fields.values().for_each(|v| collect(v, latest)),
        Value::Array(items) => items.iter().for_each(|v| collect(v, latest)),
        _ => {}
    }
}

fn decode_entities(text: &str) -> String {
    let mut result = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        result.push_str(&rest[..start]);
        rest = &rest[start..];
        let entity = rest.find(';').and_then(|end| {
            let name = &rest[1..end];
            let decoded = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some('\u{a0}'),
                _ => name
                    .strip_prefix("#x")
                    .or_else(|| name.strip_prefix("#X"))
                    .and_then(|n| u32::from_str_radix(n, 16).ok())
                    .or_else(|| name.strip_prefix('#').and_then(|n| n.parse().ok()))
                    .and_then(char::from_u32),
            };
            decoded.map(|c| (end, c))
        });
        if let Some((end, c)) = entity {
            result.push(c);
            rest = &rest[end + 1..];
        } else {
            result.push('&');
            rest = &rest[1..];
        }
    }
    result.push_str(rest);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unread_survives_restart_and_only_current_post_click_clears_it() {
        let directory =
            PathBuf::from("target").join(format!("tibo-state-test-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let tweet = |id: &str, time| Tweet {
            id: id.into(),
            timestamp_ms: time,
            text: "文字".into(),
        };
        let mut cache = None;
        observe(&mut cache, tweet("1", 1000), &directory).unwrap();
        assert!(cache.as_ref().unwrap().unread(&directory).unwrap());
        assert!(!cache.as_ref().unwrap().notified);
        observe(&mut cache, tweet("2", 2000), &directory).unwrap();
        assert!(cache.as_ref().unwrap().unread(&directory).unwrap());
        assert!(!cache.as_ref().unwrap().notified);
        let mut cache = Cache::load(&directory).unwrap();
        observe(&mut cache, tweet("1", 1000), &directory).unwrap();
        assert_eq!(cache.as_ref().unwrap().tweet.id, "2");
        save(&directory.join("tibo-read.txt"), b"1").unwrap();
        assert!(cache.as_ref().unwrap().unread(&directory).unwrap());
        save(&directory.join("tibo-read.txt"), b"2").unwrap();
        assert!(
            !Cache::load(&directory)
                .unwrap()
                .unwrap()
                .unread(&directory)
                .unwrap()
        );
        fs::remove_file(directory.join("tibo.json")).unwrap();
        fs::remove_file(directory.join("tibo-read.txt")).unwrap();
        fs::remove_dir(directory).unwrap();
        for id in ["", "-1", "1/2", "1\" --once", "18446744073709551616"] {
            assert!(!valid_id(id));
        }
    }

    #[test]
    fn selects_latest_author_post_and_full_text_without_following_quotes() {
        let tweet = |author: &str, id: &str, time: i64| {
            json!({
                "__typename": "Tweet", "rest_id": id,
                "core": {"user_results": {"result": {"core": {"screen_name": author}}}},
                "details": {"created_at_ms": time, "full_text": "short"}
            })
        };
        let mut newest = tweet("thsottiaux", "2", 2000);
        newest["note_tweet"] = json!({"note_tweet_results": {"result": {"text": "完整 &lt;內容&gt; &amp;lt; &#x1F600;"}}});
        newest["quoted_tweet_results"] = tweet("thsottiaux", "9", 9000);
        let data = json!({"data": {"user_result_by_screen_name": {"result": {
            "profile_user_originals_timeline": {"timeline": [newest, tweet("other", "8", 8000), tweet("thsottiaux", "1", 1000)]}
        }}}});
        let html = format!(
            "<script>value:{}</script>",
            serde_json::to_string(&data.to_string()).unwrap()
        );
        let result = parse(&html).unwrap();
        assert_eq!(result.id, "2");
        assert_eq!(result.text, "完整 <內容> &lt; 😀");
        assert!(parse("<html>Log in</html>").is_err());
        assert!(parse("value:\"not JSON\"").is_err());
    }
}
