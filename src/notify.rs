use std::{
    env,
    ffi::OsString,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::PathBuf,
    sync::{Mutex, mpsc},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use windows::{
    Data::Xml::Dom::XmlDocument,
    Foundation::TypedEventHandler,
    UI::Notifications::{
        NotificationSetting, ToastFailedEventArgs, ToastNotification, ToastNotificationManager,
        ToastNotifier,
    },
    Win32::{
        Foundation::{ERROR_NOT_FOUND, PROPERTYKEY},
        System::{
            Com::{
                CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree, IPersistFile, STGM_READ,
                StructuredStorage::{PROPVARIANT, PVCHF_DEFAULT, PropVariantChangeType},
            },
            Variant::VT_LPWSTR,
            WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize},
        },
        UI::Shell::{
            FOLDERID_Programs, IShellLinkW, KF_FLAG_CREATE,
            PropertiesSystem::{IPropertyStore, PSGetPropertyKeyFromName},
            SHCNE_CREATE, SHCNE_UPDATEITEM, SHCNF_FLUSH, SHCNF_PATHW, SHChangeNotify,
            SHGetKnownFolderPath, SLGP_RAWPATH, ShellLink,
        },
    },
    core::{BSTR, HRESULT, HSTRING, Interface, PCWSTR, w},
};

const APP_ID: &str = "UsageMonitor.CodexWeekly";
static SHORTCUT_READY: Mutex<bool> = Mutex::new(false);

struct WinRt;

impl Drop for WinRt {
    fn drop(&mut self) {
        unsafe { RoUninitialize() };
    }
}

pub fn show(title: &str, body: &str) -> Result<()> {
    send(title, body, "usage")
}

pub fn show_reset(body: &str) -> Result<()> {
    send("Codex 每週額度已重設！", body, "reset")
}

fn send(title: &str, body: &str, tag: &str) -> Result<()> {
    unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.context("無法初始化 Windows 通知服務")?;
    let _winrt = WinRt;
    ensure_shortcut()?;
    let notifier = notifier()?;

    let xml = format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
        escape_xml(title),
        escape_xml(body)
    );
    let document = XmlDocument::new().context("無法建立通知內容")?;
    document
        .LoadXml(&HSTRING::from(xml))
        .context("通知內容不是有效的 XML")?;
    let toast =
        ToastNotification::CreateToastNotification(&document).context("無法建立 Windows 通知")?;
    toast
        .SetTag(&HSTRING::from(tag))
        .context("無法設定通知標籤")?;
    toast
        .SetGroup(&HSTRING::from("usageMonitor"))
        .context("無法設定通知群組")?;
    let (failed, receiver) = mpsc::sync_channel(1);
    let token = toast
        .Failed(
            &TypedEventHandler::<ToastNotification, ToastFailedEventArgs>::new(move |_, args| {
                let error = args.ok().and_then(ToastFailedEventArgs::ErrorCode);
                let _ = failed.try_send(error);
                Ok(())
            }),
        )
        .context("無法監察 Windows 通知發送結果")?;
    let sent = notifier.Show(&toast).context("無法發送 Windows 通知");
    let failure = if sent.is_ok() {
        receiver.recv_timeout(Duration::from_secs(1)).ok()
    } else {
        None
    };
    let _ = toast.RemoveFailed(token);
    sent?;
    if let Some(error) = failure {
        return Err(windows::core::Error::from_hresult(error?)).context("Windows 拒絕顯示通知");
    }
    Ok(())
}

fn ensure_shortcut() -> Result<()> {
    let mut ready = SHORTCUT_READY
        .lock()
        .map_err(|_| anyhow::anyhow!("通知捷徑初始化鎖定失敗"))?;
    if *ready {
        return Ok(());
    }
    let executable = env::current_exe().context("無法取得程式路徑")?;
    let executable_path = executable
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let programs = unsafe {
        let allocated = SHGetKnownFolderPath(&FOLDERID_Programs, KF_FLAG_CREATE, None)
            .context("無法取得開始功能表路徑")?;
        let path = PathBuf::from(OsString::from_wide(allocated.as_wide()));
        CoTaskMemFree(Some(allocated.0.cast()));
        path
    };
    let shortcut = programs.join("Codex 每週額度監察.lnk");
    let shortcut_path = shortcut
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let exists = shortcut.try_exists().context("無法檢查通知捷徑")?;

    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
            .context("無法建立通知捷徑物件")?;
        let persist: IPersistFile = link.cast().context("無法取得捷徑儲存介面")?;
        let properties: IPropertyStore = link.cast().context("無法取得捷徑屬性介面")?;
        let mut key = PROPERTYKEY::default();
        PSGetPropertyKeyFromName(w!("System.AppUserModel.ID"), &mut key)
            .context("無法取得 Windows 通知身分屬性")?;
        if exists {
            persist
                .Load(PCWSTR(shortcut_path.as_ptr()), STGM_READ)
                .context("無法讀取現有通知捷徑")?;
            let mut target = vec![0u16; 32768];
            link.GetPath(&mut target, std::ptr::null_mut(), SLGP_RAWPATH.0 as u32)
                .context("無法讀取通知捷徑目標")?;
            let length = target
                .iter()
                .position(|character| *character == 0)
                .unwrap_or(target.len());
            let target = PathBuf::from(OsString::from_wide(&target[..length]));
            let same_target = target.canonicalize().ok()
                == Some(executable.canonicalize().context("無法解析程式路徑")?);
            let existing_id = properties.GetValue(&key).context("無法讀取捷徑通知身分")?;
            if same_target && BSTR::try_from(&existing_id).is_ok_and(|value| value == APP_ID) {
                *ready = true;
                return Ok(());
            }
        }
        link.SetPath(PCWSTR(executable_path.as_ptr()))
            .context("無法設定通知捷徑目標")?;
        link.SetArguments(w!("")).context("無法設定通知捷徑參數")?;
        let mut app_id = PROPVARIANT::default();
        PropVariantChangeType(
            &mut app_id,
            &PROPVARIANT::from(APP_ID),
            PVCHF_DEFAULT,
            VT_LPWSTR,
        )
        .context("無法建立通知身分屬性")?;
        properties
            .SetValue(&key, &app_id)
            .context("無法設定通知身分")?;
        properties.Commit().context("無法儲存通知身分")?;
        persist
            .Save(PCWSTR(shortcut_path.as_ptr()), true)
            .context("無法儲存開始功能表通知捷徑")?;
        SHChangeNotify(
            if exists {
                SHCNE_UPDATEITEM
            } else {
                SHCNE_CREATE
            },
            SHCNF_PATHW | SHCNF_FLUSH,
            Some(shortcut_path.as_ptr().cast()),
            None,
        );
    }
    *ready = true;
    Ok(())
}

fn notifier() -> Result<ToastNotifier> {
    let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))
        .context("無法建立 Windows 通知發送器")?;
    match notifier.Setting() {
        Ok(setting) => ensure!(
            setting == NotificationSetting::Enabled,
            "Windows 已停用此通知（{setting:?}）；請檢查系統及「Codex 每週額度監察」的通知設定"
        ),
        // 設定記錄不存在時仍嘗試 Show，保留實際發送錯誤。
        Err(error) if error.code() == HRESULT::from_win32(ERROR_NOT_FOUND.0) => {}
        Err(error) => {
            return Err(error).context("無法讀取 Windows 通知設定");
        }
    }
    Ok(notifier)
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_markup_and_preserves_chinese() {
        assert_eq!(
            escape_xml("用量 <80%> & \"額度\" '剩餘'"),
            "用量 &lt;80%&gt; &amp; &quot;額度&quot; &apos;剩餘&apos;"
        );
        assert_eq!(escape_xml("&lt;\n重設時間"), "&amp;lt;\n重設時間");
    }

    #[test]
    #[ignore = "需先以 release 程式發出測試通知"]
    fn sent_notification_is_in_windows_history() {
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.unwrap();
        let _winrt = WinRt;
        let notifications = ToastNotificationManager::History()
            .unwrap()
            .GetHistoryWithId(&HSTRING::from(APP_ID))
            .unwrap();
        assert!(
            notifications.into_iter().any(|toast| {
                toast.Group().is_ok_and(|group| group == "usageMonitor")
                    && toast.Tag().is_ok_and(|tag| tag == "usage")
            }),
            "測試通知未出現在 Windows 通知記錄"
        );
    }
}
