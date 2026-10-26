# Codex 每週額度監察

Windows Rust 程式，沿用本機 Codex CLI 的 ChatGPT 登入，透過官方 `account/rateLimits/read` 讀取 `codex` 額度。

程式、圖示及通知皆使用 Rust／原生 Windows API，沒有 Node.js、Electron 或 WebView。需要原生 Windows `codex.exe`；不要指定以 Node.js 啟動的 npm 包裝程式。

## 執行

需要 Windows 10／11，以及已透過 ChatGPT 帳戶登入的 Codex CLI。

在 Windows Git Bash 或 MSYS2 建置（需要 Rust Windows 工具鏈與 Visual Studio C++ Build Tools）：

```bash
bash build.sh
```

重新建置前，先右鍵系統匣圖示選「退出」，讓 Windows 釋放執行檔。

完成後雙擊 `target/release/usage-monitor.exe`。程式在背景執行，預設每分鐘檢查：

- 時鐘旁的系統匣圖示直接顯示**剩餘百分比**；一般為藍色，剩餘 ≤20% 為橙色、≤10% 為紅色。滑鼠停留可看已用／剩餘百分比及本機時區的重設時間。
- 右鍵圖示可「立即更新」或「退出」。若 Windows 收起圖示，可從 `^` 拖到時鐘旁。
- **剩餘由不足 100% 回升至 100% 時，優先發重設通知**，即使服務回傳的重設日期沒有改變。
- 啟動、到達已用 80%／90%／100%、週期更新時亦會通知。只有日期改變而未觀察到回滿，只顯示週期更新。
- 通知失敗會保留重設事件，下次重試；後續又用了額度也不會丟失已偵測的重設事件。
- 重設通知獨立保留在通知中心，一般用量提示不會覆蓋它。

```powershell
# 只檢查一次，並顯示通知
.\target\release\usage-monitor.exe --once

# 每 30 秒檢查
.\target\release\usage-monitor.exe --interval-seconds 30

# 指定 Codex CLI 或現有代理設定包裝檔
.\target\release\usage-monitor.exe --codex C:\Projects\openai\codex.cmd

# 測試 Windows 通知
.\target\release\usage-monitor.exe --test-notification
```

## 行為與限制

- 只監察 `codex` 的每週（10080 分鐘）時段。沒有每週資料時會回報無法讀取，不推算額度。
- 使用 CLI 的登入帳戶；請先執行 `codex login`。API key 登入不能提供 ChatGPT 訂閱的每週額度。
- 同一執行期間，每個週期各級別最多通知一次；跨過多個級別只發一次。重新啟動會再顯示目前額度。
- 重設通知依賴相鄰的成功讀取結果。若兩次檢查之間回滿後又立即使用，未讀到 100% 就無法確認回滿；程式關閉前的歷史不會保存。
- 讀取失敗會定期重試，首次失敗與恢復時通知。未成功送出的通知會重試。
- 首次通知會建立專屬開始功能表捷徑「Codex 每週額度監察」，供 Windows 識別通知來源。請允許此應用程式的 Windows 通知；勿擾／專注模式可能隱藏彈窗。
- 沒有設定開機自動執行。電腦休眠或程式關閉期間無法檢查；查詢中按退出，最遲需等當次查詢逾時（45 秒）。
- 閒置使用事件等待；資料不變不重畫圖示。每次查詢結束即清理 Codex 子程序，並停用其 Code Mode host／plugins／apps；查詢期間仍會短暫佔用額外 RAM／CPU。
- 不讀取、複製或記錄登入 token；由 Codex CLI 管理登入。使用本機 CLI 可能更新其正常快取及登入狀態。

資料介面：[OpenAI 官方 Codex App Server 文件](https://learn.chatgpt.com/docs/app-server)。CLI 介面可能隨版本變更。

## 開發檢查

```powershell
cargo test
cargo clippy --all-targets -- -D warnings
```

實際桌面驗證（先執行 `--test-notification`）：`cargo test -- --ignored`，檢查系統匣註冊／退出及本程式的通知記錄。
