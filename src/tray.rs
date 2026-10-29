use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    mem::size_of,
    sync::mpsc::{self, Receiver, Sender, SyncSender},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, ensure};
use windows::{
    Win32::{
        Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{Shell::*, WindowsAndMessaging::*},
    },
    core::{HSTRING, w},
};

const UPDATE: u32 = WM_APP + 1;
const TRAY_EVENT: u32 = WM_APP + 2;
const REMIND: u32 = WM_APP + 3;
const KEY_SELECT: u32 = NIN_SELECT | NINF_KEY;
const HOVER_TIMER: usize = 3;

pub enum TrayCommand {
    Refresh,
    Exit,
}

pub struct Tray {
    // HWND 只在建立它的 UI 執行緒使用；此數值僅供跨執行緒 PostMessageW。
    window: isize,
    updates: Sender<UpdateMessage>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Clone, PartialEq)]
struct Update {
    percent: Option<f64>,
    tooltip: String,
    imminent: bool,
}

enum UpdateMessage {
    Usage(Update),
    Tweet(crate::tibo::Tweet, bool),
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

    pub fn update(&self, percent: Option<f64>, tooltip: &str, imminent: bool) -> Result<()> {
        if let Some(percent) = percent {
            ensure!(
                percent.is_finite() && percent >= 0.0,
                "系統匣用量必須是有效非負數"
            );
        }
        self.updates
            .send(UpdateMessage::Usage(Update {
                percent,
                tooltip: tooltip.into(),
                imminent,
            }))
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

    pub fn remind(&self) -> Result<()> {
        unsafe {
            PostMessageW(
                Some(HWND(self.window as *mut c_void)),
                REMIND,
                WPARAM(0),
                LPARAM(0),
            )
        }
        .context("無法顯示 Banked Reset 提醒")
    }

    pub fn tweet_updates(
        &self,
    ) -> impl Fn(crate::tibo::Tweet, bool) -> Result<()> + Send + 'static {
        let updates = self.updates.clone();
        let window = self.window;
        move |tweet, unread| {
            updates
                .send(UpdateMessage::Tweet(tweet, unread))
                .context("系統匣已結束")?;
            unsafe {
                PostMessageW(
                    Some(HWND(window as *mut c_void)),
                    UPDATE,
                    WPARAM(0),
                    LPARAM(0),
                )
            }
            .context("無法更新貼文提示")
        }
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
    updates: Receiver<UpdateMessage>,
    commands: Sender<TrayCommand>,
    current: RefCell<Update>,
    icon: RefCell<Option<Icon>>,
    tooltip_window: RefCell<HWND>,
    reminder_window: RefCell<HWND>,
    taskbar_created: u32,
    window: Cell<HWND>,
    tweet: RefCell<Option<(crate::tibo::Tweet, bool)>>,
    hover: RefCell<Hover>,
    hover_delay: Duration,
}

#[derive(Default)]
struct Hover(Option<(POINT, Instant)>);

impl Hover {
    fn ready(&mut self, point: Option<POINT>, now: Instant, delay: Duration) -> bool {
        let Some(point) = point else {
            self.0 = None;
            return false;
        };
        match self.0 {
            Some((last, since)) if last.x == point.x && last.y == point.y => {
                now.duration_since(since) >= delay
            }
            _ => {
                self.0 = Some((point, now));
                false
            }
        }
    }
}

fn on_icon(window: HWND, point: POINT) -> bool {
    let identity = NOTIFYICONIDENTIFIER {
        cbSize: size_of::<NOTIFYICONIDENTIFIER>() as u32,
        hWnd: window,
        uID: 1,
        ..Default::default()
    };
    unsafe { Shell_NotifyIconGetRect(&identity) }.is_ok_and(|rect| {
        point.x >= rect.left && point.x < rect.right && point.y >= rect.top && point.y < rect.bottom
    })
}

fn track_hover(window: HWND, state: &UiState) -> Result<()> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point) }?;
    let inside = on_icon(window, point);
    let ready =
        state
            .hover
            .borrow_mut()
            .ready(inside.then_some(point), Instant::now(), state.hover_delay);
    let popup = *state.tooltip_window.borrow();
    unsafe {
        if !inside {
            let _ = KillTimer(Some(window), HOVER_TIMER);
            if IsWindowVisible(popup).as_bool() {
                ensure!(
                    SetTimer(Some(popup), 2, 250, None) != 0,
                    "無法設定提示關閉計時"
                );
            }
        } else {
            let _ = KillTimer(Some(popup), 2);
            ensure!(
                SetTimer(Some(window), HOVER_TIMER, 50, None) != 0,
                "無法設定滑鼠停留計時"
            );
            if ready && !IsWindowVisible(popup).as_bool() {
                show_popup(popup, false)?;
            }
        }
    }
    Ok(())
}

impl UiState {
    fn open_tweet(&self) -> Result<()> {
        let id = self
            .tweet
            .borrow()
            .as_ref()
            .map(|(tweet, _)| tweet.id.clone());
        if let Some(id) = id {
            crate::tibo::open(&id)?;
            if let Some((_, unread)) = self.tweet.borrow_mut().as_mut() {
                *unread = false;
            }
            let update = self.current.borrow().clone();
            self.apply(self.window.get(), update, false)?;
        }
        Ok(())
    }

    fn apply(&self, window: HWND, update: Update, add: bool) -> Result<()> {
        let unread = self
            .tweet
            .borrow()
            .as_ref()
            .is_some_and(|(_, unread)| *unread);
        let icon = draw_icon(update.percent, unread)?;
        let mut data = icon_data(window);
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        data.uCallbackMessage = TRAY_EVENT;
        data.hIcon = icon.0;
        data.szTip = tooltip_text(&if unread {
            format!("Tibo 有未讀貼文\n{}", update.tooltip)
        } else {
            update.tooltip.clone()
        });
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
        for popup in [
            *self.tooltip_window.borrow(),
            *self.reminder_window.borrow(),
        ] {
            if !popup.0.is_null() {
                layout_popup(popup, self)?;
            }
        }
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
    updates: Receiver<UpdateMessage>,
    commands: Sender<TrayCommand>,
    ready: &SyncSender<std::result::Result<isize, String>>,
) -> Result<()> {
    let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    ensure!(taskbar_created != 0, "無法註冊 Windows 工作列重啟事件");
    let initial = Update {
        percent: None,
        tooltip: "Codex 每週額度：正在讀取…".into(),
        imminent: false,
    };
    let mut hover_time = 0u32;
    unsafe {
        SystemParametersInfoW(
            SPI_GETMOUSEHOVERTIME,
            0,
            Some((&mut hover_time as *mut u32).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS::default(),
        )
    }
    .context("無法讀取 Windows 滑鼠停留時間")?;
    let state = Box::new(UiState {
        updates,
        commands,
        current: RefCell::new(initial.clone()),
        icon: RefCell::new(None),
        tooltip_window: RefCell::new(HWND::default()),
        reminder_window: RefCell::new(HWND::default()),
        taskbar_created,
        window: Cell::new(HWND::default()),
        tweet: RefCell::new(None),
        hover: RefCell::new(Hover::default()),
        hover_delay: Duration::from_millis(u64::from(hover_time)),
    });
    let window = UiWindow::new(&state)?;
    state.window.set(window.handle);
    *state.tooltip_window.borrow_mut() = create_popup(&state)?;
    *state.reminder_window.borrow_mut() = create_popup(&state)?;
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
        let popup_class = WNDCLASSW {
            lpfnWndProc: Some(popup_proc),
            hInstance: instance,
            lpszClassName: w!("CodexUsageMonitorPopup"),
            hbrBackground: unsafe { GetSysColorBrush(COLOR_INFOBK) },
            hCursor: unsafe { LoadCursorW(None, IDC_ARROW)? },
            ..Default::default()
        };
        ensure!(
            unsafe { RegisterClassW(&popup_class) } != 0,
            "無法註冊提示視窗"
        );
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
            let _ = UnregisterClassW(w!("CodexUsageMonitorPopup"), Some(self.instance));
        }
    }
}

const POPUP_USAGE: i32 = 100;
const POPUP_OPEN_TIBO: i32 = 101;
const POPUP_TIBO_TEXT: i32 = 102;

fn usage_text(state: &UiState) -> String {
    let current = state.current.borrow();
    if current.imminent {
        format!(
            "⚠ Banked Reset 即將到期，請留意使用期限\r\n{}",
            current.tooltip
        )
    } else {
        current.tooltip.clone()
    }
}

fn text_height(window: HWND, text: &str, width: i32) -> Result<i32> {
    unsafe {
        let dc = GetDC(Some(window));
        ensure!(!dc.0.is_null(), "無法量度提示文字");
        let old = SelectObject(dc, GetStockObject(DEFAULT_GUI_FONT));
        let mut text: Vec<u16> = text.encode_utf16().collect();
        let mut bounds = RECT {
            right: width.max(1),
            ..Default::default()
        };
        DrawTextW(
            dc,
            &mut text,
            &mut bounds,
            DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX,
        );
        SelectObject(dc, old);
        ReleaseDC(Some(window), dc);
        Ok(bounds.bottom.max(16) + 2)
    }
}

fn create_popup(state: &UiState) -> Result<HWND> {
    let instance = HINSTANCE(unsafe { GetModuleHandleW(None) }?.0);
    unsafe {
        let window = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
            w!("CodexUsageMonitorPopup"),
            w!("Codex 額度提醒"),
            WS_POPUP | WS_BORDER,
            0,
            0,
            460,
            200,
            None,
            None,
            Some(instance),
            Some(state as *const UiState as *const c_void),
        )
        .context("無法建立額度提示視窗")?;
        let controls = [
            (POPUP_USAGE, w!("STATIC"), WINDOW_STYLE::default()),
            (
                POPUP_OPEN_TIBO,
                w!("BUTTON"),
                WS_TABSTOP | WINDOW_STYLE(BS_MULTILINE as u32),
            ),
            (
                POPUP_TIBO_TEXT,
                w!("EDIT"),
                WS_TABSTOP
                    | WS_VSCROLL
                    | WINDOW_STYLE((ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32),
            ),
        ];
        for (id, class, style) in controls {
            let control = match CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class,
                w!(""),
                WS_CHILD | WS_VISIBLE | style,
                0,
                0,
                1,
                1,
                Some(window),
                Some(HMENU(id as *mut c_void)),
                Some(instance),
                None,
            ) {
                Ok(control) => control,
                Err(error) => {
                    let _ = DestroyWindow(window);
                    return Err(error).context("無法建立原生文字控制項");
                }
            };
            SendMessageW(
                control,
                WM_SETFONT,
                Some(WPARAM(GetStockObject(DEFAULT_GUI_FONT).0 as usize)),
                Some(LPARAM(1)),
            );
            if id == POPUP_TIBO_TEXT
                && !SetWindowSubclass(control, Some(tweet_text_proc), 1, 0).as_bool()
            {
                let _ = DestroyWindow(window);
                anyhow::bail!("無法設定貼文點擊處理");
            }
        }
        Ok(window)
    }
}

fn layout_popup(window: HWND, state: &UiState) -> Result<()> {
    unsafe {
        let mut rect = RECT::default();
        GetClientRect(window, &mut rect)?;
        let usage = usage_text(state);
        let height = text_height(window, &usage, rect.right - 20)?
            .min(rect.bottom - 16)
            .max(1);
        let label = GetDlgItem(Some(window), POPUP_USAGE)?;
        SetWindowTextW(label, &HSTRING::from(usage))?;
        MoveWindow(label, 10, 8, rect.right - 20, height, true)?;
        let button = GetDlgItem(Some(window), POPUP_OPEN_TIBO)?;
        let text = GetDlgItem(Some(window), POPUP_TIBO_TEXT)?;
        let tweet = state.tweet.borrow();
        let visible = window == *state.tooltip_window.borrow() && tweet.is_some();
        if let Some((tweet, unread)) = tweet.as_ref().filter(|_| visible) {
            let time = crate::local_reset_time(tweet.timestamp_ms / 1000)?;
            SetWindowTextW(
                button,
                &HSTRING::from(format!(
                    "Tibo{} · 點擊開啟 X\r\n{time}",
                    if *unread { "（未讀）" } else { "" }
                )),
            )?;
            let content = tweet.text.replace("\r\n", "\n").replace('\n', "\r\n");
            let mut existing = vec![0u16; GetWindowTextLengthW(text) as usize + 1];
            let length = GetWindowTextW(text, &mut existing) as usize;
            if existing[..length] != content.encode_utf16().collect::<Vec<_>>() {
                SetWindowTextW(text, &HSTRING::from(content))?;
            }
            MoveWindow(button, 10, height + 16, rect.right - 20, 48, true)?;
            MoveWindow(
                text,
                10,
                height + 72,
                rect.right - 20,
                (rect.bottom - height - 82).max(1),
                true,
            )?;
        }
        let _ = ShowWindow(button, if visible { SW_SHOWNA } else { SW_HIDE });
        let _ = ShowWindow(text, if visible { SW_SHOWNA } else { SW_HIDE });
        Ok(())
    }
}

unsafe extern "system" fn tweet_text_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    unsafe {
        let result = DefSubclassProc(window, message, wparam, lparam);
        if message == WM_LBUTTONUP {
            if let Ok(parent) = GetParent(window) {
                SendMessageW(
                    parent,
                    WM_COMMAND,
                    Some(WPARAM(POPUP_OPEN_TIBO as usize)),
                    None,
                );
            }
        } else if message == WM_NCDESTROY {
            let _ = RemoveWindowSubclass(window, Some(tweet_text_proc), 1);
        }
        result
    }
}

fn show_popup(window: HWND, reminder: bool) -> Result<()> {
    unsafe {
        let mut point = POINT::default();
        GetCursorPos(&mut point).context("無法取得提示位置")?;
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        ensure!(
            GetMonitorInfoW(monitor, &mut info).as_bool(),
            "無法取得螢幕範圍"
        );
        let area = info.rcWork;
        let state = &*(GetWindowLongPtrW(window, GWLP_USERDATA) as *const UiState);
        let width = 400.min(area.right - area.left - 20).max(1);
        let usage_height = text_height(window, &usage_text(state), width - 20)?;
        let body_height = if reminder {
            None
        } else {
            state
                .tweet
                .borrow()
                .as_ref()
                .map(|(tweet, _)| {
                    text_height(
                        window,
                        &tweet.text,
                        width - 24 - GetSystemMetrics(SM_CXVSCROLL),
                    )
                })
                .transpose()?
        };
        let content_height = usage_height + body_height.map_or(16, |height| 88 + height);
        let height = if reminder {
            200
        } else {
            content_height
                .min(500)
                .min(area.bottom - area.top - 20)
                .max(1)
        };
        let x = if reminder {
            area.right - width - 10
        } else {
            point.x.min(area.right - width).max(area.left)
        };
        let y = if reminder {
            area.bottom - 210
        } else {
            (point.y - height - 10).max(area.top)
        };
        SetWindowPos(
            window,
            Some(HWND_TOPMOST),
            x,
            y,
            width,
            height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
        .context("無法顯示額度提示")?;
        if reminder {
            ensure!(
                SetTimer(Some(window), 1, 15_000, None) != 0,
                "無法設定提醒關閉時間"
            );
        }
        Ok(())
    }
}

unsafe extern "system" fn popup_proc(
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
        match message {
            WM_CTLCOLORSTATIC => {
                let dc = HDC(wparam.0 as *mut c_void);
                SetTextColor(dc, COLORREF(GetSysColor(COLOR_INFOTEXT)));
                SetBkColor(dc, COLORREF(GetSysColor(COLOR_INFOBK)));
                LRESULT(GetSysColorBrush(COLOR_INFOBK).0 as isize)
            }
            WM_SIZE if GetDlgItem(Some(window), POPUP_USAGE).is_ok() => {
                if let Err(error) = layout_popup(window, &*pointer) {
                    eprintln!("提示排版失敗：{error:#}");
                }
                LRESULT(0)
            }
            WM_COMMAND if (wparam.0 & 0xffff) == POPUP_OPEN_TIBO as usize => {
                if let Err(error) = (&*pointer).open_tweet() {
                    eprintln!("開啟貼文失敗：{error:#}");
                }
                let _ = ShowWindow(window, SW_HIDE);
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let _ = KillTimer(Some(window), 1);
                let _ = KillTimer(Some(window), 2);
                let _ = ShowWindow(window, SW_HIDE);
                LRESULT(0)
            }
            WM_TIMER => {
                if wparam.0 == 2 {
                    let mut point = POINT::default();
                    let mut bounds = RECT::default();
                    let _ = GetCursorPos(&mut point);
                    let _ = GetWindowRect(window, &mut bounds);
                    if point.x >= bounds.left
                        && point.x < bounds.right
                        && point.y >= bounds.top
                        && point.y < bounds.bottom
                    {
                        return LRESULT(0);
                    }
                    if on_icon((&*pointer).window.get(), point) {
                        return LRESULT(0);
                    }
                }
                let _ = KillTimer(Some(window), wparam.0);
                let _ = ShowWindow(window, SW_HIDE);
                LRESULT(0)
            }
            WM_NCDESTROY => {
                SetWindowLongPtrW(window, GWLP_USERDATA, 0);
                DefWindowProcW(window, message, wparam, lparam)
            }
            _ => DefWindowProcW(window, message, wparam, lparam),
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
        #[cfg(test)]
        if message == WM_APP + 4 {
            return LRESULT(if wparam.0 == 0 {
                state.tooltip_window.borrow().0 as isize
            } else {
                state.reminder_window.borrow().0 as isize
            });
        }
        #[cfg(test)]
        if message == WM_APP + 5 {
            return LRESULT(match wparam.0 {
                0 => isize::from(
                    state
                        .tweet
                        .borrow()
                        .as_ref()
                        .is_some_and(|(_, unread)| *unread),
                ),
                _ => 0,
            });
        }
        let result = match message {
            WM_TIMER if wparam.0 == HOVER_TIMER => track_hover(window, state),
            UPDATE => {
                let mut result = Ok(());
                for message in state.updates.try_iter() {
                    let update = match message {
                        UpdateMessage::Usage(update) => {
                            if *state.current.borrow() == update {
                                continue;
                            }
                            update
                        }
                        UpdateMessage::Tweet(tweet, unread) => {
                            if state.tweet.borrow().as_ref() == Some(&(tweet.clone(), unread)) {
                                continue;
                            }
                            state.tweet.replace(Some((tweet, unread)));
                            state.current.borrow().clone()
                        }
                    };
                    if let Err(error) = state.apply(window, update, false) {
                        result = Err(error);
                        break;
                    }
                }
                result
            }
            REMIND => {
                if let Err(error) = show_popup(*state.reminder_window.borrow(), true) {
                    eprintln!("Banked Reset 提示失敗：{error:#}");
                }
                Ok(())
            }
            TRAY_EVENT => match (lparam.0 as u32) & 0xffff {
                NIN_POPUPOPEN | NIN_POPUPCLOSE | WM_MOUSEMOVE => track_hover(window, state),
                NIN_SELECT | KEY_SELECT => state
                    .commands
                    .send(TrayCommand::Refresh)
                    .context("監察程式已結束"),
                WM_CONTEXTMENU => match show_menu(window, state) {
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
                let _ = DestroyWindow(*state.tooltip_window.borrow());
                let _ = DestroyWindow(*state.reminder_window.borrow());
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

fn show_menu(window: HWND, state: &UiState) -> Result<Option<TrayCommand>> {
    unsafe {
        let menu = Menu(CreatePopupMenu().context("無法建立系統匣選單")?);
        AppendMenuW(menu.0, MF_STRING, 1, w!("立即更新")).context("無法加入更新選項")?;
        if state.tweet.borrow().is_some() {
            AppendMenuW(menu.0, MF_STRING, 3, w!("開啟 Tibo 最新貼文"))
                .context("無法加入貼文選項")?;
        }
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
            3 => {
                if let Err(error) = state.open_tweet() {
                    eprintln!("開啟貼文失敗：{error:#}");
                }
                None
            }
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

fn draw_icon(percent: Option<f64>, unread: bool) -> Result<Icon> {
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
            if unread && (x - 26.0).hypot(y - 6.0) <= 5.5 {
                *pixel = if (x - 26.0).hypot(y - 6.0) <= 4.0 {
                    0xffff4545
                } else {
                    0xffffffff
                };
            }
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
    fn hover_requires_stationary_pointer_and_resets_on_move_or_leave() {
        let mut hover = Hover::default();
        let start = Instant::now();
        let delay = Duration::from_millis(400);
        let point = POINT { x: 10, y: 20 };
        assert!(!hover.ready(None, start, delay));
        assert!(!hover.ready(Some(point), start, delay));
        assert!(!hover.ready(Some(point), start + Duration::from_millis(399), delay));
        let moved = POINT { x: 11, y: 20 };
        assert!(!hover.ready(Some(moved), start + delay, delay));
        assert!(!hover.ready(Some(moved), start + Duration::from_millis(799), delay));
        assert!(hover.ready(Some(moved), start + delay * 2, delay));
        assert!(!hover.ready(None, start + delay * 3, delay));
        assert!(!hover.ready(Some(moved), start + delay * 4, delay));
    }

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
            for unread in [false, true] {
                let icon = draw_icon(percent, unread).unwrap();
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
                assert_eq!(pixels[6 * 32 + 26] == 0xffff4545, unread);
                assert!(
                    pixels.iter().any(|pixel| (1..255).contains(&(pixel >> 24))),
                    "圓角邊緣應有平滑透明度"
                );
            }
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
        tray.update(
            Some(29.0),
            "Codex 每週額度\nBanked Reset：1 次\n最早到期：30/10",
            true,
        )
        .unwrap();
        let window = HWND(tray.window as *mut c_void);
        unsafe {
            SendMessageW(window, UPDATE, Some(WPARAM(0)), Some(LPARAM(0)));
            let tweet = crate::tibo::Tweet {
                id: "123".into(),
                timestamp_ms: 1793238566000,
                text: "完整貼文測試。\n".repeat(100),
            };
            tray.tweet_updates()(tweet.clone(), true).unwrap();
            SendMessageW(window, UPDATE, Some(WPARAM(0)), None);
            assert_eq!(SendMessageW(window, WM_APP + 5, Some(WPARAM(0)), None).0, 1);
            let first =
                HWND(SendMessageW(window, WM_APP + 4, Some(WPARAM(0)), None).0 as *mut c_void);
            let second =
                HWND(SendMessageW(window, WM_APP + 4, Some(WPARAM(1)), None).0 as *mut c_void);
            SendMessageW(
                window,
                TRAY_EVENT,
                Some(WPARAM(0)),
                Some(LPARAM(NIN_POPUPOPEN as isize)),
            );
            assert!(
                !IsWindowVisible(first).as_bool(),
                "附近區域事件不能立即顯示提示"
            );
            show_popup(first, false).unwrap();
            assert_ne!(
                IsWindowVisible(first).as_bool(),
                IsWindowVisible(second).as_bool()
            );
            let _ = UpdateWindow(first);
            let text = GetDlgItem(Some(first), POPUP_TIBO_TEXT).unwrap();
            let mut displayed = vec![0u16; GetWindowTextLengthW(text) as usize + 1];
            let length = GetWindowTextW(text, &mut displayed) as usize;
            assert_eq!(
                String::from_utf16(&displayed[..length]).unwrap(),
                tweet.text.replace('\n', "\r\n")
            );
            assert_ne!(GetWindowLongW(text, GWL_STYLE) & ES_READONLY, 0);
            SendMessageW(
                text,
                windows::Win32::UI::Controls::EM_LINESCROLL,
                None,
                Some(LPARAM(3)),
            );
            assert!(
                SendMessageW(
                    text,
                    windows::Win32::UI::Controls::EM_GETFIRSTVISIBLELINE,
                    None,
                    None
                )
                .0 > 0
            );
            let mut large = RECT::default();
            GetWindowRect(first, &mut large).unwrap();
            assert_eq!(large.right - large.left, 400);
            let mut short = tweet;
            short.text = "短文".into();
            tray.tweet_updates()(short, false).unwrap();
            SendMessageW(window, UPDATE, Some(WPARAM(0)), None);
            show_popup(first, false).unwrap();
            let mut compact = RECT::default();
            GetWindowRect(first, &mut compact).unwrap();
            assert!(
                compact.bottom - compact.top < large.bottom - large.top,
                "短文應縮小提示高度"
            );
            assert_eq!(SendMessageW(window, WM_APP + 5, Some(WPARAM(0)), None).0, 0);
            let label = GetDlgItem(Some(first), POPUP_USAGE).unwrap();
            let mut warning = vec![0u16; GetWindowTextLengthW(label) as usize + 1];
            let length = GetWindowTextW(label, &mut warning) as usize;
            assert!(
                String::from_utf16(&warning[..length])
                    .unwrap()
                    .contains("即將到期")
            );
            let dc = GetDC(Some(text));
            let brush = SendMessageW(
                first,
                WM_CTLCOLORSTATIC,
                Some(WPARAM(dc.0 as usize)),
                Some(LPARAM(text.0 as isize)),
            );
            assert_eq!(brush.0, GetSysColorBrush(COLOR_INFOBK).0 as isize);
            assert_eq!(GetTextColor(dc).0, GetSysColor(COLOR_INFOTEXT));
            ReleaseDC(Some(text), dc);
            SendMessageW(
                window,
                TRAY_EVENT,
                Some(WPARAM(0)),
                Some(LPARAM(NIN_POPUPCLOSE as isize)),
            );
            assert!(IsWindowVisible(first).as_bool());
            SendMessageW(first, WM_LBUTTONUP, None, Some(LPARAM(0)));
            assert!(!IsWindowVisible(first).as_bool() && !IsWindowVisible(second).as_bool());
            let mut delay = 0u32;
            SystemParametersInfoW(
                SPI_GETMOUSEHOVERTIME,
                0,
                Some((&mut delay as *mut u32).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS::default(),
            )
            .unwrap();
            if (100..=2000).contains(&delay) {
                struct RestoreCursor(POINT);
                impl Drop for RestoreCursor {
                    fn drop(&mut self) {
                        unsafe {
                            let _ = SetCursorPos(self.0.x, self.0.y);
                        }
                    }
                }
                let mut original = POINT::default();
                GetCursorPos(&mut original).unwrap();
                let _restore = RestoreCursor(original);
                let icon = Shell_NotifyIconGetRect(&identity).unwrap();
                let x = (icon.left + icon.right) / 2;
                let y = (icon.top + icon.bottom) / 2;
                let outside_x = if icon.left >= 4 {
                    icon.left - 4
                } else {
                    icon.right + 4
                };
                SetCursorPos(outside_x, y).unwrap();
                SendMessageW(
                    window,
                    TRAY_EVENT,
                    None,
                    Some(LPARAM(NIN_POPUPOPEN as isize)),
                );
                thread::sleep(Duration::from_millis(u64::from(delay + 150)));
                assert!(!IsWindowVisible(first).as_bool(), "只停在附近不能顯示提示");
                SetCursorPos(x, y).unwrap();
                SendMessageW(
                    window,
                    TRAY_EVENT,
                    None,
                    Some(LPARAM(WM_MOUSEMOVE as isize)),
                );
                assert!(!IsWindowVisible(first).as_bool());
                thread::sleep(Duration::from_millis(u64::from(delay / 2)));
                SetCursorPos(x + 1, y).unwrap();
                SendMessageW(
                    window,
                    TRAY_EVENT,
                    None,
                    Some(LPARAM(WM_MOUSEMOVE as isize)),
                );
                thread::sleep(Duration::from_millis(u64::from(delay * 3 / 4)));
                assert!(
                    !IsWindowVisible(first).as_bool(),
                    "移動後應重新等待完整停留時間"
                );
                thread::sleep(Duration::from_millis(u64::from(delay / 2 + 150)));
                assert!(IsWindowVisible(first).as_bool(), "停留後應顯示提示");
                let mut bounds = RECT::default();
                GetWindowRect(first, &mut bounds).unwrap();
                SetCursorPos(bounds.right - 20, bounds.bottom - 15).unwrap();
                SendMessageW(
                    window,
                    TRAY_EVENT,
                    None,
                    Some(LPARAM(NIN_POPUPCLOSE as isize)),
                );
                thread::sleep(Duration::from_millis(350));
                assert!(IsWindowVisible(first).as_bool(), "移入提示後應保持開啟");
                SetCursorPos(outside_x, y).unwrap();
                SendMessageW(
                    window,
                    TRAY_EVENT,
                    None,
                    Some(LPARAM(NIN_POPUPOPEN as isize)),
                );
                thread::sleep(Duration::from_millis(350));
                assert!(!IsWindowVisible(first).as_bool(), "離開圖示及提示後應關閉");
            }
            SendMessageW(window, REMIND, Some(WPARAM(0)), Some(LPARAM(0)));
            assert_ne!(
                IsWindowVisible(first).as_bool(),
                IsWindowVisible(second).as_bool()
            );
        }
        drop(tray);
        assert!(matches!(commands.recv(), Ok(TrayCommand::Exit)));
    }
}
