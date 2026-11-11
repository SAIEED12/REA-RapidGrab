//! Windows system tray icon, context menu and balloon notifications.

#[cfg(windows)]
mod imp {
    use std::cell::RefCell;
    use std::mem::size_of;
    use std::ptr;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use eframe::egui;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{
        CreateBitmap, DeleteObject, HBITMAP,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
        NIM_MODIFY, NIM_SETVERSION, NOTIFYICONDATAW, NOTIFYICON_VERSION_4, NIIF_INFO, NIIF_WARNING,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
        DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW,
        GetSystemMetrics, PostMessageW, PostQuitMessage, RegisterClassW, SetForegroundWindow,
        TrackPopupMenu, TranslateMessage, HICON, ICONINFO, MF_CHECKED, MF_SEPARATOR, MF_STRING,
        MF_UNCHECKED, MSG, SM_CXSMICON, SM_CYSMICON, TPM_BOTTOMALIGN, TPM_LEFTALIGN,
        TPM_RIGHTBUTTON, WM_APP, WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONDBLCLK,
        WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPED,
    };

    use crate::icon;

    const WM_TRAYICON: u32 = WM_APP + 1;
    const WM_TRAY_EXIT: u32 = WM_APP + 2;
    const WM_TRAY_BALLOON: u32 = WM_APP + 3;
    const ID_SHOW: usize = 1001;
    const ID_TOGGLE_QUEUE: usize = 1002;
    const ID_OPEN_DOWNLOADS: usize = 1003;
    const ID_EXIT: usize = 1004;

    #[derive(Debug, Clone)]
    pub enum TrayCommand {
        Show,
        ToggleQueue,
        OpenDownloads,
        Exit,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum BalloonKind {
        Info,
        Warning,
    }

    #[derive(Clone)]
    pub struct TrayHandle {
        hwnd: isize,
        alive: Arc<AtomicBool>,
    }

    unsafe impl Send for TrayHandle {}
    unsafe impl Sync for TrayHandle {}

    impl TrayHandle {
        pub fn notify(&self, title: &str, message: &str, kind: BalloonKind) {
            if !self.alive.load(Ordering::Relaxed) || self.hwnd == 0 {
                return;
            }
            let payload = Box::new((to_wide_trunc(title, 63), to_wide_trunc(message, 255), kind));
            unsafe {
                PostMessageW(self.hwnd as HWND, WM_TRAY_BALLOON, 0, Box::into_raw(payload) as LPARAM);
            }
        }

        pub fn shutdown(&self) {
            if !self.alive.load(Ordering::Relaxed) || self.hwnd == 0 {
                return;
            }
            unsafe {
                PostMessageW(self.hwnd as HWND, WM_TRAY_EXIT, 0, 0);
            }
        }
    }

    pub fn start(
        ctx: egui::Context,
        tx: crossbeam_channel::Sender<TrayCommand>,
        queue_paused: Arc<AtomicBool>,
    ) -> Option<TrayHandle> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<isize>();
        let alive = Arc::new(AtomicBool::new(true));
        let alive_thread = alive.clone();
        std::thread::Builder::new()
            .name("rapidgrab-tray".into())
            .spawn(move || unsafe {
                run_tray(ctx, tx, queue_paused, alive_thread, ready_tx);
            })
            .ok()?;
        let hwnd = ready_rx.recv().ok().unwrap_or(0);
        if hwnd == 0 {
            alive.store(false, Ordering::Relaxed);
            return None;
        }
        Some(TrayHandle { hwnd, alive })
    }

    fn to_wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn to_wide_trunc(text: &str, max_chars: usize) -> Vec<u16> {
        let mut v: Vec<u16> = text.encode_utf16().take(max_chars).collect();
        v.push(0);
        v
    }

    fn copy_wide(dst: &mut [u16], src: &[u16]) {
        let n = src.len().min(dst.len());
        dst[..n].copy_from_slice(&src[..n]);
        if n < dst.len() {
            dst[n..].fill(0);
        } else if let Some(last) = dst.last_mut() {
            *last = 0;
        }
    }

    unsafe fn create_hicon() -> HICON {
        let logo = icon::logo();
        let w = GetSystemMetrics(SM_CXSMICON).max(16) as u32;
        let h = GetSystemMetrics(SM_CYSMICON).max(16) as u32;
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let sx = (x as u64 * logo.width as u64 / w as u64) as u32;
                let sy = (y as u64 * logo.height as u64 / h as u64) as u32;
                let si = ((sy.min(logo.height - 1) * logo.width + sx.min(logo.width - 1)) * 4) as usize;
                let di = ((y * w + x) * 4) as usize;
                let r = logo.rgba[si];
                let g = logo.rgba[si + 1];
                let b = logo.rgba[si + 2];
                let a = logo.rgba[si + 3];
                // Premultiplied for better icon edges.
                bgra[di] = (b as u32 * a as u32 / 255) as u8;
                bgra[di + 1] = (g as u32 * a as u32 / 255) as u8;
                bgra[di + 2] = (r as u32 * a as u32 / 255) as u8;
                bgra[di + 3] = a;
            }
        }
        let color: HBITMAP = CreateBitmap(w as i32, h as i32, 1, 32, bgra.as_ptr() as *const _);
        if color.is_null() {
            return std::ptr::null_mut();
        }
        let mask_bits = vec![0u8; (w * h / 8).max(4) as usize];
        let mask: HBITMAP = CreateBitmap(w as i32, h as i32, 1, 1, mask_bits.as_ptr() as *const _);
        let mut ii = ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let hicon = CreateIconIndirect(&mut ii);
        if !color.is_null() {
            DeleteObject(color as _);
        }
        if !mask.is_null() {
            DeleteObject(mask as _);
        }
        hicon
    }

    unsafe fn add_icon(hwnd: HWND, hicon: HICON) -> NOTIFYICONDATAW {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = WM_TRAYICON;
        nid.hIcon = hicon;
        copy_wide(&mut nid.szTip, &to_wide("RapidGrab"));
        Shell_NotifyIconW(NIM_ADD, &nid);
        nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        Shell_NotifyIconW(NIM_SETVERSION, &nid);
        nid
    }

    unsafe fn show_menu(hwnd: HWND, queue_paused: bool) {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        let show = to_wide("Show RapidGrab");
        AppendMenuW(menu, MF_STRING, ID_SHOW, show.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        let label = if queue_paused { "Start Queue" } else { "Stop Queue" };
        let toggle = to_wide(label);
        let flags = MF_STRING | if queue_paused { MF_UNCHECKED } else { MF_CHECKED };
        AppendMenuW(menu, flags, ID_TOGGLE_QUEUE, toggle.as_ptr());
        let open = to_wide("Open Downloads folder");
        AppendMenuW(menu, MF_STRING, ID_OPEN_DOWNLOADS, open.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        let exit = to_wide("Exit");
        AppendMenuW(menu, MF_STRING, ID_EXIT, exit.as_ptr());
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        SetForegroundWindow(hwnd);
        TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_LEFTALIGN,
            pt.x,
            pt.y,
            0,
            hwnd,
            ptr::null(),
        );
        // Required so the menu dismisses correctly.
        PostMessageW(hwnd, 0, 0, 0);
        DestroyMenu(menu);
    }

    unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        match msg {
            WM_TRAYICON => {
                let event = (l as u32) & 0xffff;
                if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                    let paused = TRAY_STATE.with(|s| {
                        s.borrow()
                            .as_ref()
                            .map(|st| st.queue_paused.load(Ordering::Relaxed))
                            .unwrap_or(false)
                    });
                    show_menu(hwnd, paused);
                    return 0;
                } else if event == WM_LBUTTONDBLCLK {
                    send_cmd(TrayCommand::Show);
                    return 0;
                }
            }
            WM_COMMAND => {
                let id = (w & 0xffff) as usize;
                match id {
                    ID_SHOW => send_cmd(TrayCommand::Show),
                    ID_TOGGLE_QUEUE => send_cmd(TrayCommand::ToggleQueue),
                    ID_OPEN_DOWNLOADS => send_cmd(TrayCommand::OpenDownloads),
                    ID_EXIT => send_cmd(TrayCommand::Exit),
                    _ => {}
                }
                return 0;
            }
            WM_TRAY_BALLOON => {
                if l != 0 {
                    let payload = Box::from_raw(l as *mut (Vec<u16>, Vec<u16>, BalloonKind));
                    let (title, msg_text, kind) = *payload;
                    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
                    nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
                    nid.hWnd = hwnd;
                    nid.uID = 1;
                    nid.uFlags = NIF_INFO;
                    copy_wide(&mut nid.szInfoTitle, &title);
                    copy_wide(&mut nid.szInfo, &msg_text);
                    nid.dwInfoFlags = match kind {
                        BalloonKind::Info => NIIF_INFO,
                        BalloonKind::Warning => NIIF_WARNING,
                    };
                    Shell_NotifyIconW(NIM_MODIFY, &nid);
                }
                return 0;
            }
            WM_TRAY_EXIT => {
                DestroyWindow(hwnd);
                return 0;
            }
            WM_DESTROY => {
                remove_icon(hwnd);
                PostQuitMessage(0);
                return 0;
            }
            _ => {}
        }
        DefWindowProcW(hwnd, msg, w, l)
    }

    unsafe fn remove_icon(hwnd: HWND) {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        Shell_NotifyIconW(NIM_DELETE, &nid);
    }

    struct TrayState {
        tx: crossbeam_channel::Sender<TrayCommand>,
        ctx: egui::Context,
        queue_paused: Arc<AtomicBool>,
    }

    thread_local! {
        static TRAY_STATE: RefCell<Option<TrayState>> = const { RefCell::new(None) };
    }

    fn send_cmd(cmd: TrayCommand) {
        TRAY_STATE.with(|s| {
            if let Some(state) = s.borrow().as_ref() {
                let _ = state.tx.send(cmd);
                state.ctx.request_repaint();
            }
        });
    }

    unsafe fn run_tray(
        ctx: egui::Context,
        tx: crossbeam_channel::Sender<TrayCommand>,
        queue_paused: Arc<AtomicBool>,
        alive: Arc<AtomicBool>,
        ready_tx: std::sync::mpsc::Sender<isize>,
    ) {
        TRAY_STATE.with(|s| {
            *s.borrow_mut() = Some(TrayState {
                tx,
                ctx,
                queue_paused: queue_paused.clone(),
            });
        });

        let class_name = to_wide("RapidGrabTrayWindow");
        let hinstance = GetModuleHandleW(ptr::null());
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            class_name.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance,
            ptr::null(),
        );
        // Hand the handle back before entering the message loop: `start()`
        // waits for it, and the loop only exits when the window is destroyed.
        let _ = ready_tx.send(if hwnd.is_null() { 0 } else { hwnd as isize });
        if hwnd.is_null() {
            alive.store(false, Ordering::Relaxed);
            TRAY_STATE.with(|s| *s.borrow_mut() = None);
            return;
        }
        let hicon = create_hicon();
        let _nid = add_icon(hwnd, hicon);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if !hicon.is_null() {
            DestroyIcon(hicon);
        }
        alive.store(false, Ordering::Relaxed);
        TRAY_STATE.with(|s| *s.borrow_mut() = None);
    }
}

#[cfg(windows)]
pub use imp::{start, BalloonKind, TrayCommand, TrayHandle};

#[cfg(not(windows))]
#[derive(Debug, Clone)]
pub enum TrayCommand {
    Show,
    ToggleQueue,
    OpenDownloads,
    Exit,
}

#[cfg(not(windows))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BalloonKind {
    Info,
    Warning,
}

#[cfg(not(windows))]
#[derive(Clone)]
pub struct TrayHandle;

#[cfg(not(windows))]
impl TrayHandle {
    pub fn notify(&self, _title: &str, _message: &str, _kind: BalloonKind) {}
    pub fn shutdown(&self) {}
}

#[cfg(not(windows))]
pub fn start(
    _ctx: eframe::egui::Context,
    _tx: crossbeam_channel::Sender<TrayCommand>,
    _queue_paused: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Option<TrayHandle> {
    None
}
