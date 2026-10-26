#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod codex;
mod notify;
mod tray;
mod usage;

use std::{
    env,
    ffi::OsString,
    os::windows::io::{FromRawHandle, OwnedHandle},
    path::PathBuf,
    sync::mpsc::RecvTimeoutError,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail, ensure};
use windows::{
    Win32::{
        Foundation::{ERROR_ALREADY_EXISTS, FILETIME, GetLastError, SYSTEMTIME},
        System::{
            Console::{ATTACH_PARENT_PROCESS, AttachConsole},
            Threading::CreateMutexW,
            Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime},
        },
        UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW},
    },
    core::{HSTRING, w},
};

const HELP: &str = "Codex 每週額度監察（Windows）

usage-monitor [--interval-seconds 秒數] [--codex 路徑] [--once]
usage-monitor --test-notification

預設每 60 秒檢查；系統匣顯示剩餘百分比。
剩餘回滿 100% 優先通知；啟動、達到 80%／90%／100%、週期更新亦會通知。
  --interval-seconds  最少 30 秒，最多 86400 秒
  --codex             指定已登入 ChatGPT 帳戶的 codex.exe／codex.cmd
  --once              讀取一次額度並發送通知後結束
  --test-notification  只測試 Windows 通知
  --help              顯示說明

右鍵系統匣圖示可立即更新或退出。沿用 Codex CLI 登入。";

#[derive(Default)]
struct Options {
    interval: Option<u64>,
    codex: Option<PathBuf>,
    once: bool,
    test_notification: bool,
    help: bool,
}

impl Options {
    fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self> {
        let mut options = Self::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("--help" | "-h") => options.help = true,
                Some("--once") => options.once = true,
                Some("--test-notification") => options.test_notification = true,
                Some("--codex") => {
                    options.codex = Some(args.next().context("--codex 需要路徑")?.into());
                }
                Some("--interval-seconds") => {
                    let value = args.next().context("--interval-seconds 需要秒數")?;
                    let seconds = value
                        .to_str()
                        .context("秒數格式錯誤")?
                        .parse::<u64>()
                        .context("秒數必須是正整數")?;
                    ensure!(
                        (30..=86400).contains(&seconds),
                        "檢查間隔必須介乎 30 至 86400 秒"
                    );
                    options.interval = Some(seconds);
                }
                _ => bail!("不支援的參數：{}；請使用 --help", arg.to_string_lossy()),
            }
        }
        ensure!(
            !(options.once && options.test_notification),
            "--once 與 --test-notification 不能同時使用"
        );
        Ok(options)
    }
}

fn find_codex(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        ensure!(
            path.is_file(),
            "指定的 Codex 路徑不是檔案：{}",
            path.display()
        );
        return path.canonicalize().context("無法解析 Codex 路徑");
    }
    if let Some(path) = env::var_os("PATH") {
        for directory in env::split_paths(&path).filter(|path| path.is_absolute()) {
            for name in ["codex.exe", "codex.cmd", "codex.bat"] {
                let candidate = directory.join(name);
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
    }
    if let Some(local) = env::var_os("LOCALAPPDATA") {
        let candidate = PathBuf::from(local).join("Programs/OpenAI/Codex/bin/codex.exe");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!("找不到 Codex CLI；請先安裝並以 ChatGPT 帳戶登入，或使用 --codex 指定路徑")
}

fn single_instance() -> Result<OwnedHandle> {
    let handle = unsafe { CreateMutexW(None, false, w!("Local\\CodexWeeklyUsageMonitor")) }
        .context("無法建立監察程式互斥鎖")?;
    let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    let guard = unsafe { OwnedHandle::from_raw_handle(handle.0) };
    ensure!(!already_running, "已有 Codex 每週額度監察程式執行中");
    Ok(guard)
}

fn local_reset_time(timestamp: i64) -> Result<String> {
    let ticks = u64::try_from(timestamp)
        .ok()
        .and_then(|value| value.checked_add(11_644_473_600))
        .and_then(|value| value.checked_mul(10_000_000))
        .context("重設時間超出有效範圍")?;
    let file_time = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    unsafe {
        FileTimeToSystemTime(&file_time, &mut utc)?;
        SystemTimeToTzSpecificLocalTime(None, &utc, &mut local)?;
    }
    Ok(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}（本機時間）",
        local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute
    ))
}

fn check(program: &std::path::Path) -> Result<(usage::WeeklyUsage, String)> {
    let usage = usage::parse_weekly(&codex::read_limits(program)?)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    usage::check_fresh(&usage, now)?;
    let reset = if usage.resets_at <= now {
        "服務尚未提供下一次重設時間".to_owned()
    } else {
        local_reset_time(usage.resets_at)?
    };
    let body = format!(
        "已用 {:.1}%，剩餘 {:.1}%\n重設：{}",
        usage.used_percent,
        (100.0 - usage.used_percent).max(0.0),
        reset
    );
    Ok((usage, body))
}

fn run() -> Result<()> {
    let options = Options::parse(env::args_os().skip(1))?;
    if options.help {
        println!("{HELP}");
        return Ok(());
    }
    if options.test_notification {
        notify::show(
            "Codex 額度監察：測試通知",
            "收到此訊息即表示 Windows 通知可用。",
        )?;
        println!("測試通知已交予 Windows。");
        return Ok(());
    }
    let program = find_codex(options.codex)?;
    if options.once {
        let (_, body) = check(&program)?;
        println!("{body}");
        notify::show("Codex 每週額度", &body)?;
        return Ok(());
    }
    let _instance = single_instance()?;
    let (tray, commands) = tray::Tray::start()?;
    let interval = options.interval.unwrap_or(60);
    println!(
        "Codex：{}\n每 {interval} 秒檢查每週額度；右鍵系統匣圖示可退出。",
        program.display()
    );
    let mut alerts = usage::Alerts::default();
    let mut error_notified = false;
    loop {
        let result = check(&program);
        if commands
            .try_iter()
            .any(|command| matches!(command, tray::TrayCommand::Exit))
        {
            return Ok(());
        }
        match result {
            Ok((usage, body)) => {
                println!("{body}");
                tray.update(Some(usage.used_percent), &format!("Codex 每週額度\n{body}"))?;
                let notice = alerts.observe(&usage);
                let title = notice
                    .map(usage::Notice::title)
                    .or_else(|| error_notified.then_some("Codex 每週額度：監察已恢復"));
                if let Some(title) = title {
                    let sent = if notice == Some(usage::Notice::Reset) {
                        notify::show_reset(&body)
                    } else {
                        notify::show(title, &body)
                    };
                    match sent {
                        Ok(()) => {
                            alerts.delivered(&usage);
                            error_notified = false;
                        }
                        Err(error) => {
                            eprintln!("通知失敗，下次檢查重試：{error:#}");
                            tray.update(
                                Some(usage.used_percent),
                                &format!(
                                    "Codex 每週額度\n{body}\nWindows 通知失敗，請檢查通知設定"
                                ),
                            )?;
                        }
                    }
                }
            }
            Err(error) => {
                eprintln!("讀取失敗（{interval} 秒後重試）：{error:#}");
                tray.update(
                    None,
                    &format!("Codex 額度更新失敗\n{error}\n{interval} 秒後重試"),
                )?;
                if !error_notified {
                    match notify::show(
                        "Codex 額度監察暫時無法更新",
                        "請檢查網絡與 Codex CLI 登入狀態。程式會定期重試；詳細原因見系統匣提示。",
                    ) {
                        Ok(()) => error_notified = true,
                        Err(error) => eprintln!("通知失敗：{error:#}"),
                    }
                }
            }
        }
        match commands.recv_timeout(Duration::from_secs(interval)) {
            Ok(tray::TrayCommand::Refresh) => {
                // 合併連按更新，避免向服務連續查詢。
                for command in commands.try_iter() {
                    if matches!(command, tray::TrayCommand::Exit) {
                        return Ok(());
                    }
                }
            }
            Ok(tray::TrayCommand::Exit) | Err(RecvTimeoutError::Disconnected) => return Ok(()),
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

fn main() {
    let console = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) }.is_ok() || cfg!(debug_assertions);
    let command_line = env::args_os().skip(1).any(|arg| {
        matches!(
            arg.to_str(),
            Some("--once" | "--test-notification" | "--help" | "-h")
        )
    });
    if let Err(error) = run() {
        eprintln!("錯誤：{error:#}");
        if !console && !command_line {
            unsafe {
                MessageBoxW(
                    None,
                    &HSTRING::from(format!("{error:#}")),
                    w!("Codex 額度監察"),
                    MB_OK | MB_ICONERROR,
                );
            }
        }
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_intervals_and_conflicting_modes() {
        for args in [
            vec!["--interval-seconds", "0"],
            vec!["--interval-seconds", "abc"],
            vec!["--interval-seconds"],
            vec!["--once", "--test-notification"],
            vec!["--unknown"],
        ] {
            assert!(Options::parse(args.into_iter().map(OsString::from)).is_err());
        }
        let parsed =
            Options::parse(["--interval-seconds", "60", "--once"].map(OsString::from)).unwrap();
        assert_eq!(parsed.interval, Some(60));
        assert!(parsed.once);
    }

    #[test]
    fn rejects_reset_timestamp_overflow() {
        assert!(local_reset_time(i64::MAX).is_err());
        assert!(local_reset_time(-1).is_err());
    }
}
