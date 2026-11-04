//! 系统托盘 + 关窗隐藏(仅 Windows)。
//!
//! 实现方式:子类化主窗口 WndProc 拦截 WM_CLOSE —— winit 收不到关闭事件,
//! 也就不会触发 eframe「粘滞 close_requested → 每帧 CancelClose → 每帧强制重绘」
//! 的死循环(那种做法后台 CPU 会被打满)。
//! 托盘左键单击/菜单「显示主窗口」直接 ShowWindow;菜单「退出」放行 WM_CLOSE 真正退出。

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::OnceLock;

use eframe::egui;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    self as wm, CallWindowProcW, EnumWindows, GetWindowLongPtrW, GetWindowRect,
    GetWindowThreadProcessId, GetWindowTextLengthW, GetWindowTextW, SendMessageW,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow,
};

/// 隐藏(移屏外)前的窗口位置,恢复时用;初始 [100,100,0,0]
static SAVED_POS: OnceLock<std::sync::Mutex<[i32; 2]>> = OnceLock::new();

fn saved_pos() -> &'static std::sync::Mutex<[i32; 2]> {
    SAVED_POS.get_or_init(|| std::sync::Mutex::new([100, 100]))
}

/// 托盘「退出」置位后,放行 WM_CLOSE,程序真正退出
pub static FORCE_EXIT: AtomicBool = AtomicBool::new(false);
/// 窗口刚被隐藏到托盘(UI 线程轮询记录一次日志)
pub static HIDDEN_ONCE: AtomicBool = AtomicBool::new(false);
/// 当前处于隐藏状态(UI 线程据此跳过 UI 构建,避免隐藏窗口无限重绘)
pub static HIDDEN: AtomicBool = AtomicBool::new(false);

static CTX: OnceLock<egui::Context> = OnceLock::new();
static HWND_CACHE: AtomicIsize = AtomicIsize::new(0);
static OLD_WNDPROC: AtomicIsize = AtomicIsize::new(0);

// ---------- 主窗口子类化:拦截 WM_CLOSE ----------

unsafe extern "system" fn subclass_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == wm::WM_CLOSE && !FORCE_EXIT.load(Ordering::SeqCst) {
        // 关窗 = 移到屏幕外 + 摘掉任务栏按钮。
        // 不能用 SW_HIDE:eframe 0.27/winit 0.29 在窗口隐藏时事件循环会自旋,打满一个核。
        let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        GetWindowRect(hwnd, &mut r);
        if r.left > -29000 {
            // 只记录屏幕内的位置(避免重复关窗把 -30000 存进去)
            *saved_pos().lock().unwrap() = [r.left, r.top];
        }
        let ex = GetWindowLongPtrW(hwnd, wm::GWL_EXSTYLE);
        // 先隐藏 → 改样式 → 移屏外 → 再显示:任务栏只在窗口重新显示时按样式
        // 决定建不建按钮,TOOLWINDOW 会让按钮消失。瞬时 SW_HIDE(毫秒级)
        // 不会触发 eframe 隐藏自旋问题
        ShowWindow(hwnd, wm::SW_HIDE);
        SetWindowLongPtrW(
            hwnd,
            wm::GWL_EXSTYLE,
            ex | wm::WS_EX_TOOLWINDOW as isize,
        );
        SetWindowPos(
            hwnd,
            0,
            -30000,
            -30000,
            0,
            0,
            wm::SWP_NOSIZE | wm::SWP_NOZORDER | wm::SWP_NOACTIVATE,
        );
        ShowWindow(hwnd, wm::SW_SHOW);
        // 双保险:直接让任务栏移除按钮(TOOLWINDOW 样式只影响之后新建的按钮)
        taskbar::delete_tab(hwnd);
        HIDDEN.store(true, Ordering::SeqCst);
        HIDDEN_ONCE.store(true, Ordering::SeqCst);
        if let Some(ctx) = CTX.get() {
            ctx.request_repaint(); // 唤醒 UI 线程记日志
        }
        return 0; // 吞掉 WM_CLOSE,不传给 winit(否则 eframe 会退出)
    }
    let old: wm::WNDPROC = std::mem::transmute(OLD_WNDPROC.load(Ordering::SeqCst));
    CallWindowProcW(old, hwnd, msg, wp, lp)
}

unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> i32 {
    let want_pid = lparam as u32;
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, &mut pid);
    if pid != want_pid {
        return 1; // 别的进程,继续
    }
    let len = GetWindowTextLengthW(hwnd);
    if len <= 0 {
        return 1;
    }
    let mut buf = vec![0u16; len as usize + 1];
    GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    let title = String::from_utf16_lossy(&buf);
    if title.contains("CF Hosts") {
        HWND_CACHE.store(hwnd as isize, Ordering::SeqCst);
        return 0; // 找到,停止枚举
    }
    1
}

/// 在本进程内定位主窗口(按进程 id + 标题前缀)
fn find_main_window() -> Option<isize> {
    unsafe {
        EnumWindows(Some(enum_cb), std::process::id() as LPARAM);
        let h = HWND_CACHE.load(Ordering::SeqCst);
        (h != 0).then_some(h)
    }
}

/// 子类化主窗口;失败仅影响「关窗隐藏」便利性,不阻塞 GUI
fn subclass_main_window() {
    std::thread::spawn(|| {
        // 窗口可能尚未完成创建,重试最多 10 秒
        for _ in 0..100 {
            if let Some(h) = find_main_window() {
                unsafe {
                    let old = SetWindowLongPtrW(
                        h as HWND,
                        wm::GWLP_WNDPROC,
                        subclass_proc as usize as isize,
                    );
                    if old != 0 {
                        OLD_WNDPROC.store(old, Ordering::SeqCst);
                        return;
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        eprintln!("托盘:未能定位主窗口句柄,关窗将直接退出");
    });
}

// ---------- 任务栏按钮控制(windows-sys 未暴露 vtable,手工声明 COM 接口) ----------

mod taskbar {
    use windows_sys::core::GUID;
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
    };

    // CLSID_TaskbarList / IID_ITaskbarList,来自 Shell SDK 文档
    const CLSID_TASKBARLIST: GUID = GUID {
        data1: 0x56FDF344,
        data2: 0xFD6D,
        data3: 0x11D0,
        data4: [0x95, 0x8A, 0x00, 0x60, 0x97, 0xC9, 0xA0, 0x90],
    };
    const IID_ITASKBARLIST: GUID = GUID {
        data1: 0x56FDF342,
        data2: 0xFD6D,
        data3: 0x11D0,
        data4: [0x95, 0x8A, 0x00, 0x60, 0x97, 0xC9, 0xA0, 0x90],
    };

    /// ITaskbarList vtable:IUnknown(3) + HrInit + AddTab + DeleteTab + ActivateTab + SetActiveAlt
    #[repr(C)]
    struct Vtbl {
        _iunknown: [usize; 3],
        hr_init: usize,
        add_tab: usize,
        delete_tab: usize,
        _activate_tab: usize,
        _set_active_alt: usize,
    }
    #[repr(C)]
    struct Itf {
        vtbl: *const Vtbl,
    }

    unsafe fn call(hwnd: HWND, add: bool) {
        // 线程可能已初始化 COM(忽略返回值)
        CoInitializeEx(std::ptr::null_mut(), COINIT_APARTMENTTHREADED as u32);
        let mut itf: *mut Itf = std::ptr::null_mut();
        let hr = CoCreateInstance(
            &CLSID_TASKBARLIST,
            std::ptr::null_mut(),
            CLSCTX_ALL,
            &IID_ITASKBARLIST,
            &mut itf as *mut *mut Itf as *mut *mut core::ffi::c_void,
        );
        if hr != 0 || itf.is_null() {
            return;
        }
        let v = (*itf).vtbl;
        let hr_init: unsafe extern "system" fn(*mut Itf) -> i32 =
            std::mem::transmute((*v).hr_init);
        hr_init(itf);
        let method: unsafe extern "system" fn(*mut Itf, HWND) -> i32 = std::mem::transmute(if add {
            (*v).add_tab
        } else {
            (*v).delete_tab
        });
        method(itf, hwnd);
        let release: unsafe extern "system" fn(*mut Itf) -> usize =
            std::mem::transmute((*v)._iunknown[2]);
        release(itf);
    }

    /// 让任务栏移除该窗口的按钮(窗口保持可见)
    pub unsafe fn delete_tab(hwnd: HWND) {
        call(hwnd, false);
    }

    /// 恢复任务栏按钮
    pub unsafe fn add_tab(hwnd: HWND) {
        call(hwnd, true);
    }
}

// ---------- 托盘 ----------

fn tray_show_window() {
    let h = HWND_CACHE.load(Ordering::SeqCst);
    HIDDEN.store(false, Ordering::SeqCst);
    if let Some(ctx) = CTX.get() {
        ctx.request_repaint(); // 立即恢复完整 UI 绘制
    }
    if h == 0 {
        return;
    }
    let [x, y] = *saved_pos().lock().unwrap();
    unsafe {
        // 清 TOOLWINDOW → 移回原位 → hide/show 切换让任务栏重建按钮
        let ex = GetWindowLongPtrW(h as HWND, wm::GWL_EXSTYLE);
        SetWindowLongPtrW(
            h as HWND,
            wm::GWL_EXSTYLE,
            ex & !(wm::WS_EX_TOOLWINDOW as isize),
        );
        SetWindowPos(
            h as HWND,
            0,
            x,
            y,
            0,
            0,
            wm::SWP_NOSIZE | wm::SWP_NOZORDER | wm::SWP_NOACTIVATE,
        );
        ShowWindow(h as HWND, wm::SW_HIDE);
        ShowWindow(h as HWND, wm::SW_SHOW);
        taskbar::add_tab(h as HWND);
        SetForegroundWindow(h as HWND);
    }
}

fn tray_quit() {
    FORCE_EXIT.store(true, Ordering::SeqCst);
    let h = HWND_CACHE.load(Ordering::SeqCst);
    if h != 0 {
        // 放行一帧 WM_CLOSE:winit → eframe 默认退出流程
        unsafe { SendMessageW(h as HWND, wm::WM_CLOSE, 0, 0) };
    } else {
        std::process::exit(0);
    }
}

/// 创建托盘图标与菜单 + 子类化主窗口;失败仅影响托盘便利性,不阻塞 GUI
pub fn init(ctx: &egui::Context) -> Result<(), String> {
    let _ = CTX.set(ctx.clone());
    use tray_icon::menu::{Menu, MenuEvent, MenuItem};
    use tray_icon::{TrayIconBuilder, TrayIconEvent};

    let menu = Menu::new();
    let show = MenuItem::with_id("show", "显示主窗口", true, None);
    let quit = MenuItem::with_id("quit", "退出", true, None);
    menu.append(&show).map_err(|e| e.to_string())?;
    menu.append(&quit).map_err(|e| e.to_string())?;

    let icon = tray_icon::Icon::from_rgba(crate::app::icon_rgba_64(), 64, 64)
        .map_err(|e| e.to_string())?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("cf-hosts-gui · 后台运行中")
        .with_icon(icon)
        .build()
        .map_err(|e| e.to_string())?;
    let _ = KEEP_ALIVE.set(KeepAliveTray(tray));

    // 事件走全局 handler(tray-icon 与 eframe 各自持有事件循环,不共享 winit 事件)
    MenuEvent::set_event_handler(Some(|e: MenuEvent| match e.id().as_ref() {
        "show" => tray_show_window(),
        "quit" => tray_quit(),
        _ => {}
    }));
    TrayIconEvent::set_event_handler(Some(|e: TrayIconEvent| {
        // 左键单击恢复窗口(双击会连发两次 Click,同样生效;右键留给菜单)
        if let tray_icon::TrayIconEvent::Click {
            button: tray_icon::MouseButton::Left,
            button_state: tray_icon::MouseButtonState::Up,
            ..
        } = e
        {
            tray_show_window();
        }
    }));

    subclass_main_window();

    // 窗口隐藏后 winit 可能不再派发重绘:心跳保证内置定时器照常调度
    let heartbeat = ctx.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(30));
        heartbeat.request_repaint();
    });
    Ok(())
}

/// TrayIcon 必须保活,否则托盘图标立即消失。
/// Windows 实现内部是 Rc,构造后我们不再跨线程调用它(事件走全局 handler),
/// 包一层 Send/Sync 仅用于塞进 static。
struct KeepAliveTray(#[allow(dead_code)] tray_icon::TrayIcon);
unsafe impl Send for KeepAliveTray {}
unsafe impl Sync for KeepAliveTray {}
static KEEP_ALIVE: OnceLock<KeepAliveTray> = OnceLock::new();
