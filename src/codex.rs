use anyhow::{Result, anyhow, bail, ensure};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::windows::{
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::CommandExt,
    },
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::HANDLE,
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
                Thread32Next,
            },
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
            Threading::{
                CREATE_NO_WINDOW, CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
            },
        },
    },
    core::PCWSTR,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_MESSAGE_BYTES: u64 = 1024 * 1024;

pub fn read_limits(program: &Path) -> Result<Value> {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let mut server = AppServer::start(program)?;
    server.send(&json!({
        "id": 1,
        "method": "initialize",
        "params": {"clientInfo": {
            "name": "usage_monitor",
            "version": env!("CARGO_PKG_VERSION")
        }}
    }))?;
    server.response(1, "initialize", deadline)?;
    server.send(&json!({"method": "initialized", "params": {}}))?;
    server.send(&json!({"id": 2, "method": "account/rateLimits/read"}))?;
    server.response(2, "account/rateLimits/read", deadline)
}

struct AppServer {
    child: Child,
    job: Option<OwnedHandle>,
    output: Option<Receiver<Result<Value>>>,
    reader: Option<JoinHandle<()>>,
}

impl AppServer {
    fn start(program: &Path) -> Result<Self> {
        let job = unsafe {
            let handle = CreateJobObjectW(None, PCWSTR::null())
                .map_err(|_| anyhow!("無法建立 Codex 程序管理物件。"))?;
            OwnedHandle::from_raw_handle(handle.0)
        };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        unsafe {
            SetInformationJobObject(
                HANDLE(job.as_raw_handle()),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .map_err(|_| anyhow!("無法設定 Codex 程序清理。"))?;
        }

        // 先暫停再加入 Job，避免 .cmd 在受管理前啟動子程序。
        let child = Command::new(program)
            // 額度查詢不需要 Code Mode host、外掛或應用整合。
            .args([
                "--disable",
                "code_mode_host",
                "--disable",
                "plugins",
                "--disable",
                "apps",
            ])
            .arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW.0 | CREATE_SUSPENDED.0)
            .spawn()
            .map_err(|_| anyhow!("無法啟動 Codex；請確認 Codex CLI 路徑及安裝。"))?;
        let mut server = Self {
            child,
            job: Some(job),
            output: None,
            reader: None,
        };
        unsafe {
            AssignProcessToJobObject(
                HANDLE(server.job.as_ref().unwrap().as_raw_handle()),
                HANDLE(server.child.as_raw_handle()),
            )
            .map_err(|_| anyhow!("無法管理 Codex 子程序，已取消查詢。"))?;
        }
        resume_process(server.child.id())?;

        let stdout = server
            .child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("無法讀取 Codex 輸出。"))?;
        let (sender, receiver) = mpsc::sync_channel(16);
        server.output = Some(receiver);
        server.reader = Some(
            thread::Builder::new()
                .name("codex-output".into())
                .spawn(move || {
                    let mut reader = BufReader::new(stdout);
                    loop {
                        let message = match read_message(&mut reader) {
                            Ok(Some(message)) => Ok(message),
                            Ok(None) => Err(anyhow!(
                                "Codex 已關閉連線；請確認 CLI 可正常啟動並已登入 ChatGPT。"
                            )),
                            Err(error) => Err(error),
                        };
                        let failed = message.is_err();
                        if sender.send(message).is_err() || failed {
                            break;
                        }
                    }
                })
                .map_err(|_| anyhow!("無法啟動 Codex 輸出讀取。"))?,
        );
        Ok(server)
    }

    fn send(&mut self, message: &Value) -> Result<()> {
        let stdin = self
            .child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("Codex 輸入連線已關閉。"))?;
        writeln!(stdin, "{message}")
            .and_then(|()| stdin.flush())
            .map_err(|_| anyhow!("無法傳送 Codex 查詢。"))
    }

    fn response(&self, id: u64, method: &str, deadline: Instant) -> Result<Value> {
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| anyhow!("Codex 額度查詢超過 45 秒；請檢查網絡及登入狀態。"))?;
            let message = self
                .output
                .as_ref()
                .ok_or_else(|| anyhow!("Codex 輸出連線未建立。"))?
                .recv_timeout(remaining)
                .map_err(|error| match error {
                    RecvTimeoutError::Timeout => {
                        anyhow!("Codex 額度查詢超過 45 秒；請檢查網絡及登入狀態。")
                    }
                    RecvTimeoutError::Disconnected => anyhow!("Codex 輸出連線已關閉。"),
                })??;
            if let Some(result) = response_result(message, id, method)? {
                return Ok(result);
            }
        }
    }
}

impl Drop for AppServer {
    fn drop(&mut self) {
        self.child.stdin.take();
        self.job.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.output.take();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn resume_process(process_id: u32) -> Result<()> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0)
            .map_err(|_| anyhow!("無法取得 Codex 執行緒。"))?;
        let snapshot = OwnedHandle::from_raw_handle(snapshot.0);
        let mut entry = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        Thread32First(HANDLE(snapshot.as_raw_handle()), &mut entry)
            .map_err(|_| anyhow!("無法取得 Codex 執行緒。"))?;
        loop {
            if entry.th32OwnerProcessID == process_id {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID)
                    .map_err(|_| anyhow!("無法開啟 Codex 執行緒。"))?;
                let thread = OwnedHandle::from_raw_handle(thread.0);
                ensure!(
                    ResumeThread(HANDLE(thread.as_raw_handle())) != u32::MAX,
                    "無法恢復 Codex 執行緒。"
                );
                return Ok(());
            }
            if Thread32Next(HANDLE(snapshot.as_raw_handle()), &mut entry).is_err() {
                bail!("找不到 Codex 執行緒。");
            }
        }
    }
}

fn read_message(reader: &mut impl BufRead) -> Result<Option<Value>> {
    let mut line = Vec::new();
    let length = reader
        .take(MAX_MESSAGE_BYTES + 1)
        .read_until(b'\n', &mut line)
        .map_err(|_| anyhow!("無法讀取 Codex 回覆。"))?;
    if length == 0 {
        return Ok(None);
    }
    ensure!(
        length as u64 <= MAX_MESSAGE_BYTES,
        "Codex 回覆超出大小限制。"
    );
    serde_json::from_slice(&line)
        .map(Some)
        .map_err(|_| anyhow!("Codex 回覆不是有效 JSON；請確認 CLI wrapper 沒有額外標準輸出。"))
}

fn response_result(mut message: Value, id: u64, method: &str) -> Result<Option<Value>> {
    let object = message
        .as_object_mut()
        .ok_or_else(|| anyhow!("Codex 回覆格式不正確。"))?;
    if object.contains_key("method") {
        ensure!(object["method"].is_string(), "Codex 通知格式不正確。");
        ensure!(
            !object.contains_key("id"),
            "Codex 要求額外驗證；請先使用 Codex CLI 重新登入 ChatGPT。"
        );
        return Ok(None);
    }
    let response_id = object
        .get("id")
        .ok_or_else(|| anyhow!("Codex 回覆缺少查詢識別碼。"))?;
    if response_id.as_u64() != Some(id) {
        return Ok(None);
    }
    if let Some(error) = object.get("error") {
        if let Some(code) = error.get("code").and_then(Value::as_i64) {
            bail!("Codex {method} 失敗（錯誤碼 {code}）；請確認已使用 ChatGPT 帳號登入 CLI。");
        }
        bail!("Codex {method} 失敗；請確認已使用 ChatGPT 帳號登入 CLI。");
    }
    let result = object
        .remove("result")
        .ok_or_else(|| anyhow!("Codex 回覆缺少查詢結果。"))?;
    ensure!(result.is_object(), "Codex 查詢結果格式不正確。");
    Ok(Some(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn accepts_matching_response_and_ignores_interleaving() {
        assert_eq!(
            response_result(json!({"id": 2, "result": {"rateLimits": {}}}), 2, "read").unwrap(),
            Some(json!({"rateLimits": {}}))
        );
        assert!(
            response_result(json!({"id": 1, "result": {}}), 2, "read")
                .unwrap()
                .is_none()
        );
        assert!(
            response_result(
                json!({"method": "account/updated", "params": {}}),
                2,
                "read"
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn rpc_errors_never_expose_server_messages_or_data() {
        let error = response_result(
            json!({"id": 2, "error": {
                "code": -32000,
                "message": "secret-token",
                "data": {"token": "secret-token"}
            }}),
            2,
            "account/rateLimits/read",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("-32000"));
        assert!(!error.contains("secret-token"));
        assert!(response_result(json!({"id": 2, "result": null}), 2, "read").is_err());
        assert!(response_result(json!({"id": 2}), 2, "read").is_err());
        assert!(response_result(json!([]), 2, "read").is_err());
    }

    #[test]
    fn rejects_invalid_json_and_oversized_messages() {
        assert!(read_message(&mut Cursor::new(b"secret-token\n")).is_err());
        assert!(
            read_message(&mut Cursor::new(vec![b' '; MAX_MESSAGE_BYTES as usize + 1])).is_err()
        );
        assert!(read_message(&mut Cursor::new(b"")).unwrap().is_none());
        assert_eq!(
            read_message(&mut Cursor::new(b"{\"id\":2}\n")).unwrap(),
            Some(json!({"id": 2}))
        );
    }
}
