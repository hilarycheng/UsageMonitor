use std::{
    cell::RefCell,
    ffi::c_void,
    mem::size_of,
    sync::mpsc::{self, Receiver, Sender, SyncSender},
    thread::{self, JoinHandle},
};

use anyhow::{Context, Result, anyhow, ensure};
use windows::{
    Win32::{
        Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{Shell::*, WindowsAndMessaging::*},
    },
    core::w,
};

const UPDATE: u32 = WM_APP + 1;
const TRAY_EVENT: u32 = WM_APP + 2;
const KEY_SELECT: u32 = NIN_SELECT | NINF_KEY;

pub enum TrayCommand {
    Refresh,
    Exit,
}

pub struct Tray {
    // HWND 只在建立它的 UI 執行緒使用；此數值僅供跨執行緒 PostMessageW。
    window: isize,
    updates: Sender<Update>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Clone, PartialEq)]
struct Update {
    percent: Option<f64>,
    tooltip: String,
}

impl Tray {
    pub fn start() -> Result<(Self, Receiver<TrayCommand>)> {
        let (updates, update_rx) = mpsc::channel();
        let (commands, command_rx) = mpsc::channel();
        let (ready, ready_rx) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("usage-tray".into())
            .spawn(move || {
                if let Err(error) = run_ui(update_rx, commands.clone(), &ready) {
                    eprintln!("系統匣錯誤：{error:#}");
                    let _ = ready.try_send(Err(format!("{error:#}")));
                }
                let _ = commands.send(TrayCommand::Exit);
            })
            .context("無法啟動系統匣執行緒")?;
        match ready_rx.recv() {
            Ok(Ok(window)) => Ok((
                Self {
                    window,
                    updates,
                    thread: Some(thread),
                },
                command_rx,
            )),
            result => {
                let _ = thread.join();
                Err(anyhow!(match result {
                    Ok(Err(error)) => error,
                    _ => "系統匣執行緒未能完成初始化".into(),
                }))
            }
        }
    }

    pub fn update(&self, percent: Option<f64>, tooltip: &str) -> Result<()> {
        if let Some(percent) = percent {
            ensure!(
                percent.is_finite() && percent >= 0.0,
                "系統匣用量必須是有效非負數"
            );
        }
        self.updates
            .send(Update {
                percent,
                tooltip: tooltip.into(),
            })
            .context("系統匣執行緒已結束")?;
        unsafe {
            PostMessageW(
                Some(HWND(self.window as *mut c_void)),
                UPDATE,
                WPARAM(0),
                LPARAM(0),
            )
        }
        .context("無法更新系統匣圖示")
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            let _ = PostMessageW(
                Some(HWND(self.window as *mut c_void)),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
            );
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct UiState {
    updates: Receiver<Update>,
    commands: Sender<TrayCommand>,
    current: RefCell<Update>,
    icon: RefCell<Option<Icon>>,
    taskbar_created: u32,
}

impl UiState {
    fn apply(&self, window: HWND, update: Update, add: bool) -> Result<()> {
        if !add && *self.current.borrow() == update {
            return Ok(());
        }
        let icon = draw_icon(update.percent)?;
        let mut data = icon_data(window);
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
        data.uCallbackMessage = TRAY_EVENT;
        data.hIcon = icon.0;
        data.szTip = tooltip_text(&update.tooltip);
        ensure!(
            unsafe { Shell_NotifyIconW(if add { NIM_ADD } else { NIM_MODIFY }, &data) }.as_bool(),
            "Windows 無法顯示系統匣圖示"
        );
        if add {
            data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            ensure!(
                unsafe { Shell_NotifyIconW(NIM_SETVERSION, &data) }.as_bool(),
                "Windows 無法設定系統匣事件版本"
            );
        }
        self.icon.replace(Some(icon));
        self.current.replace(update);
        Ok(())
    }
}

fn icon_data(window: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: window,
        uID: 1,
        ..Default::default()
    }
}

fn run_ui(
    updates: Receiver<Update>,
    commands: Sender<TrayCommand>,
    ready: &SyncSender<std::result::Result<isize, String>>,
) -> Result<()> {
    let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    ensure!(taskbar_created != 0, "無法註冊 Windows 工作列重啟事件");
    let initial = Update {
        percent: None,
        tooltip: "Codex 每週額度：正在讀取…".into(),
    };
    let state = Box::new(UiState {
        updates,
        commands,
        current: RefCell::new(initial.clone()),
        icon: RefCell::new(None),
        taskbar_created,
    });
    let window = UiWindow::new(&state)?;
    state.apply(window.handle, initial, true)?;
    ready
        .send(Ok(window.handle.0 as isize))
        .context("系統匣初始化接收端已結束")?;
    let mut message = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) }.0;
        ensure!(result != -1, "無法讀取 Windows 系統匣事件");
        if result == 0 {
            return Ok(());
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

struct UiWindow {
    handle: HWND,
    instance: HINSTANCE,
}

impl UiWindow {
    fn new(state: &UiState) -> Result<Self> {
        let instance = HINSTANCE(
            unsafe { GetModuleHandleW(None) }
                .context("無法取得程式模組")?
                .0,
        );
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: w!("CodexUsageMonitorTray"),
            ..Default::default()
        };
        ensure!(unsafe { RegisterClassW(&class) } != 0, "無法註冊系統匣視窗");
        let mut window = Self {
            handle: HWND::default(),
            instance,
        };
        // 隱藏的頂層視窗可接收 TaskbarCreated；message-only 視窗不會收到廣播。
        window.handle = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("CodexUsageMonitorTray"),
                w!("Codex 每週額度"),
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance),
                Some(state as *const UiState as *const c_void),
            )
        }
        .context("無法建立系統匣視窗")?;
        Ok(window)
    }
}

impl Drop for UiWindow {
    fn drop(&mut self) {
        unsafe {
            if !self.handle.0.is_null() {
                let _ = DestroyWindow(self.handle);
            }
            let _ = UnregisterClassW(w!("CodexUsageMonitorTray"), Some(self.instance));
        }
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if message == WM_NCCREATE {
            let create = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(window, GWLP_USERDATA, create.lpCreateParams as isize);
            return LRESULT(1);
        }
        let pointer = GetWindowLongPtrW(window, GWLP_USERDATA) as *const UiState;
        if pointer.is_null() {
            return DefWindowProcW(window, message, wparam, lparam);
        }
        let state = &*pointer;
        let result = match message {
            UPDATE => match state.updates.try_iter().last() {
                Some(update) => state.apply(window, update, false),
                None => Ok(()),
            },
            TRAY_EVENT => match (lparam.0 as u32) & 0xffff {
                NIN_SELECT | KEY_SELECT => state
                    .commands
                    .send(TrayCommand::Refresh)
                    .context("監察程式已結束"),
                WM_CONTEXTMENU => match show_menu(window) {
                    Ok(Some(TrayCommand::Refresh)) => state
                        .commands
                        .send(TrayCommand::Refresh)
                        .context("監察程式已結束"),
                    Ok(Some(TrayCommand::Exit)) => state
                        .commands
                        .send(TrayCommand::Exit)
                        .context("監察程式已結束"),
                    Ok(None) => Ok(()),
                    Err(error) => Err(error),
                },
                _ => Ok(()),
            },
            WM_CLOSE => {
                close_window(window);
                Ok(())
            }
            WM_DESTROY => {
                let _ = Shell_NotifyIconW(NIM_DELETE, &icon_data(window));
                state.icon.borrow_mut().take();
                PostQuitMessage(0);
                Ok(())
            }
            WM_NCDESTROY => {
                SetWindowLongPtrW(window, GWLP_USERDATA, 0);
                return DefWindowProcW(window, message, wparam, lparam);
            }
            message if message == state.taskbar_created => {
                let update = state.current.borrow().clone();
                state.apply(window, update, true)
            }
            _ => return DefWindowProcW(window, message, wparam, lparam),
        };
        if let Err(error) = result {
            eprintln!("系統匣錯誤：{error:#}");
            close_window(window);
        }
        LRESULT(0)
    }
}

fn close_window(window: HWND) {
    unsafe {
        let _ = DestroyWindow(window);
        PostQuitMessage(0);
    }
}

struct Menu(HMENU);

impl Drop for Menu {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyMenu(self.0);
        }
    }
}

fn show_menu(window: HWND) -> Result<Option<TrayCommand>> {
    unsafe {
        let menu = Menu(CreatePopupMenu().context("無法建立系統匣選單")?);
        AppendMenuW(menu.0, MF_STRING, 1, w!("立即更新")).context("無法加入更新選項")?;
        AppendMenuW(menu.0, MF_STRING, 2, w!("退出")).context("無法加入退出選項")?;
        let mut point = POINT::default();
        GetCursorPos(&mut point).context("無法取得選單位置")?;
        let _ = SetForegroundWindow(window);
        let selected = TrackPopupMenu(
            menu.0,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            None,
            window,
            None,
        )
        .0;
        let _ = PostMessageW(Some(window), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = Shell_NotifyIconW(NIM_SETFOCUS, &icon_data(window));
        Ok(match selected {
            1 => Some(TrayCommand::Refresh),
            2 => Some(TrayCommand::Exit),
            _ => None,
        })
    }
}

struct Icon(HICON);

impl Drop for Icon {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyIcon(self.0);
        }
    }
}

#[derive(Default)]
struct Drawing {
    dc: HDC,
    color: HBITMAP,
    mask: HBITMAP,
    font: HFONT,
    old_bitmap: HGDIOBJ,
    old_font: HGDIOBJ,
}

impl Drop for Drawing {
    fn drop(&mut self) {
        unsafe {
            for previous in [self.old_font, self.old_bitmap] {
                if !previous.0.is_null() {
                    SelectObject(self.dc, previous);
                }
            }
            for object in [
                HGDIOBJ(self.font.0),
                HGDIOBJ(self.mask.0),
                HGDIOBJ(self.color.0),
            ] {
                if !object.0.is_null() {
                    let _ = DeleteObject(object);
                }
            }
            if !self.dc.0.is_null() {
                let _ = DeleteDC(self.dc);
            }
        }
    }
}

fn select(dc: HDC, object: HGDIOBJ) -> Result<HGDIOBJ> {
    let previous = unsafe { SelectObject(dc, object) };
    ensure!(
        !previous.0.is_null() && previous.0 as isize != -1,
        "無法選取系統匣繪圖物件"
    );
    Ok(previous)
}

fn icon_style(used_percent: Option<f64>) -> (String, u32) {
    match used_percent {
        Some(used_percent) => (
            format!("{:.0}", (100.0 - used_percent).clamp(0.0, 100.0)),
            if used_percent >= 90.0 {
                0xffb32424
            } else if used_percent >= 80.0 {
                0xffa44e00
            } else {
                0xff245da8
            },
        ),
        None => ("?".into(), 0xff3b4351),
    }
}

fn draw_icon(percent: Option<f64>) -> Result<Icon> {
    let (label, background) = icon_style(percent);
    let mut drawing = Drawing::default();
    unsafe {
        drawing.dc = CreateCompatibleDC(None);
        ensure!(!drawing.dc.0.is_null(), "無法建立系統匣繪圖環境");
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: 32,
                biHeight: -32,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = std::ptr::null_mut();
        drawing.color = CreateDIBSection(
            Some(drawing.dc),
            &info,
            DIB_RGB_COLORS,
            &mut pixels,
            None,
            0,
        )
        .context("無法建立系統匣位圖")?;
        ensure!(!pixels.is_null(), "系統匣位圖沒有像素緩衝區");
        drawing.old_bitmap = select(drawing.dc, HGDIOBJ(drawing.color.0))?;
        std::slice::from_raw_parts_mut(pixels.cast::<u32>(), 32 * 32).fill(background);
        drawing.font = CreateFontW(
            if label.len() == 3 { -14 } else { -20 },
            0,
            0,
            0,
            FW_SEMIBOLD.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            u32::from(DEFAULT_PITCH.0 | FF_SWISS.0),
            w!("Segoe UI"),
        );
        ensure!(!drawing.font.0.is_null(), "無法建立系統匣字型");
        drawing.old_font = select(drawing.dc, HGDIOBJ(drawing.font.0))?;
        ensure!(
            SetTextColor(drawing.dc, COLORREF(0x00ffffff)).0 != CLR_INVALID,
            "無法設定系統匣文字顏色"
        );
        ensure!(
            SetBkMode(drawing.dc, TRANSPARENT) != 0,
            "無法設定系統匣文字背景"
        );
        let mut text: Vec<u16> = label.encode_utf16().collect();
        let mut rectangle = RECT {
            left: 4,
            top: 4,
            right: 28,
            bottom: 28,
        };
        ensure!(
            DrawTextW(
                drawing.dc,
                &mut text,
                &mut rectangle,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE
            ) > 0,
            "無法繪製系統匣用量"
        );
        ensure!(GdiFlush().as_bool(), "無法完成系統匣繪圖");
        for (index, pixel) in std::slice::from_raw_parts_mut(pixels.cast::<u32>(), 32 * 32)
            .iter_mut()
            .enumerate()
        {
            let x = (index % 32) as f32 + 0.5;
            let y = (index / 32) as f32 + 0.5;
            let dx = (8.0 - x).max(0.0).max(x - 24.0);
            let dy = (8.0 - y).max(0.0).max(y - 24.0);
            let alpha = ((8.5 - dx.hypot(dy)).clamp(0.0, 1.0) * 255.0).round() as u32;
            // Windows 圖示使用預乘 alpha，圓角邊緣才不會出現黑邊。
            let red = ((*pixel >> 16) & 255) * alpha / 255;
            let green = ((*pixel >> 8) & 255) * alpha / 255;
            let blue = (*pixel & 255) * alpha / 255;
            *pixel = (alpha << 24) | (red << 16) | (green << 8) | blue;
        }
        let mask = [0u8; 128];
        drawing.mask = CreateBitmap(32, 32, 1, 1, Some(mask.as_ptr().cast()));
        ensure!(!drawing.mask.0.is_null(), "無法建立系統匣圖示遮罩");
        select(drawing.dc, drawing.old_bitmap)?;
        drawing.old_bitmap = HGDIOBJ::default();
        CreateIconIndirect(&ICONINFO {
            fIcon: true.into(),
            hbmMask: drawing.mask,
            hbmColor: drawing.color,
            ..Default::default()
        })
        .map(Icon)
        .context("無法建立系統匣圖示")
    }
}

fn tooltip_text(text: &str) -> [u16; 128] {
    let mut result = [0u16; 128];
    let mut length = 0;
    for character in text.chars().take_while(|character| *character != '\0') {
        let width = character.len_utf16();
        if length + width >= result.len() {
            break;
        }
        character.encode_utf16(&mut result[length..length + width]);
        length += width;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tooltip_preserves_utf16_boundaries_and_terminator() {
        let text = format!("{}😀", "額".repeat(126));
        let tooltip = tooltip_text(&text);
        assert_eq!(
            String::from_utf16(&tooltip[..126]).unwrap(),
            "額".repeat(126)
        );
        assert_eq!(&tooltip[126..], &[0, 0]);
        assert_eq!(&tooltip_text("用量\0忽略")[..3], &[29992, 37327, 0]);
    }

    #[test]
    fn icon_marks_unknown_and_alert_thresholds() {
        assert_eq!(icon_style(None).0, "?");
        assert_eq!(icon_style(Some(0.0)), ("100".into(), 0xff245da8));
        assert_eq!(icon_style(Some(29.0)), ("71".into(), 0xff245da8));
        assert_eq!(icon_style(Some(80.0)), ("20".into(), 0xffa44e00));
        assert_eq!(icon_style(Some(90.0)), ("10".into(), 0xffb32424));
        assert_eq!(icon_style(Some(100.0)).0, "0");
        assert_eq!(icon_style(Some(101.0)).0, "0");
        assert_ne!(icon_style(Some(79.9)).1, icon_style(Some(80.0)).1);
        assert_ne!(icon_style(Some(89.9)).1, icon_style(Some(90.0)).1);
    }

    #[test]
    fn native_windows_icons_can_be_created_for_all_states() {
        for percent in [None, Some(0.0), Some(80.0), Some(90.0), Some(100.0)] {
            let icon = draw_icon(percent).unwrap();
            let mut info = ICONINFO::default();
            unsafe { GetIconInfo(icon.0, &mut info) }.unwrap();
            let drawing = Drawing {
                dc: unsafe { CreateCompatibleDC(None) },
                color: info.hbmColor,
                mask: info.hbmMask,
                ..Default::default()
            };
            let mut bitmap = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: 32,
                    biHeight: -32,
                    biPlanes: 1,
                    biBitCount: 32,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut pixels = [0u32; 32 * 32];
            assert_eq!(
                unsafe {
                    GetDIBits(
                        drawing.dc,
                        drawing.color,
                        0,
                        32,
                        Some(pixels.as_mut_ptr().cast()),
                        &mut bitmap,
                        DIB_RGB_COLORS,
                    )
                },
                32
            );
            for corner in [0, 31, 32 * 31, 32 * 32 - 1] {
                assert_eq!(pixels[corner], 0, "圓角外應完全透明");
            }
            assert_eq!(pixels[16 * 32 + 16] >> 24, 255);
            assert!(
                pixels.iter().any(|pixel| (1..255).contains(&(pixel >> 24))),
                "圓角邊緣應有平滑透明度"
            );
        }
    }

    #[test]
    #[ignore = "需要互動式 Windows 桌面"]
    fn native_tray_can_register_and_close() {
        let (tray, commands) = Tray::start().unwrap();
        let identity = NOTIFYICONIDENTIFIER {
            cbSize: size_of::<NOTIFYICONIDENTIFIER>() as u32,
            hWnd: HWND(tray.window as *mut c_void),
            uID: 1,
            ..Default::default()
        };
        assert!(unsafe { Shell_NotifyIconGetRect(&identity) }.is_ok());
        tray.update(Some(29.0), "Codex 每週額度：已用 29%，剩餘 71%")
            .unwrap();
        drop(tray);
        assert!(matches!(commands.recv(), Ok(TrayCommand::Exit)));
    }
}
