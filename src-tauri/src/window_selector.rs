/// VoiceDown - 窗口选择器模块
///
/// 通过 Win32 EnumWindows API 枚举系统所有可见顶层窗口，
/// 返回窗口句柄、标题和进程名，供用户选择目标窗口。

use serde::Serialize;
use windows::core::PWSTR;
use windows::Win32::Foundation::{BOOL, CloseHandle, HANDLE, HWND, LPARAM};
use windows::Win32::System::ProcessStatus::K32GetModuleBaseNameW;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
    PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetAncestor, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
    IsWindowVisible, GA_ROOT,
};

/// 窗口信息（序列化为 JSON 传给前端）
#[derive(Serialize, Clone, Debug)]
pub struct WindowInfo {
    /// 窗口句柄 (usize)
    pub hwnd: usize,
    /// 窗口标题
    pub title: String,
    /// 进程名（如 "firefox.exe", "chrome.exe"）
    pub process_name: String,
    /// 进程 ID
    pub pid: u32,
}

/// 枚举系统所有可见顶层窗口
pub fn list_visible_windows() -> Vec<WindowInfo> {
    let mut windows: Vec<WindowInfo> = Vec::new();

    let lparam = LPARAM(&raw const windows as isize);

    unsafe {
        let _ = EnumWindows(Some(enum_window_callback), lparam);
    }

    // 去重：同一进程+标题只保留一个
    windows.dedup_by(|a, b| a.title == b.title && a.process_name == b.process_name);

    // 按进程名排序
    windows.sort_by(|a, b| {
        a.process_name
            .to_lowercase()
            .cmp(&b.process_name.to_lowercase())
    });

    windows
}

unsafe extern "system" fn enum_window_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let windows: &mut Vec<WindowInfo> = unsafe { &mut *(lparam.0 as *mut Vec<WindowInfo>) };

    // 跳过不可见窗口
    if IsWindowVisible(hwnd).as_bool() == false {
        return BOOL(1);
    }

    // 跳过非顶层窗口（子窗口、弹出菜单等）
    if GetAncestor(hwnd, GA_ROOT) != hwnd {
        return BOOL(1);
    }

    // 获取窗口标题
    let title = get_window_title(hwnd);
    if title.is_empty() {
        return BOOL(1);
    }

    // 获取进程 ID 和进程名
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));

    let process_name = get_process_name(pid);

    windows.push(WindowInfo {
        hwnd: hwnd.0 as usize,
        title,
        process_name,
        pid,
    });

    BOOL(1)
}

fn get_window_title(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len == 0 {
            return String::new();
        }
        let mut buf = vec![0u16; (len + 1) as usize];
        GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..len as usize])
    }
}

fn get_process_name(pid: u32) -> String {
    if pid == 0 {
        return "System".to_string();
    }

    unsafe {
        let handle = OpenProcess(
            PROCESS_QUERY_INFORMATION | PROCESS_VM_READ,
            false,
            pid,
        );

        match handle {
            Ok(h) => {
                let name = query_process_name(h);
                let _ = CloseHandle(h);
                name
            }
            Err(_) => {
                format!("pid_{}", pid)
            }
        }
    }
}

unsafe fn query_process_name(handle: HANDLE) -> String {
    // 首选：K32GetModuleBaseNameW（需要 PROCESS_QUERY_INFORMATION | PROCESS_VM_READ）
    let mut name_buf = [0u16; 260];
    let len = K32GetModuleBaseNameW(handle, None, &mut name_buf);

    if len > 0 {
        String::from_utf16_lossy(&name_buf[..len as usize])
    } else {
        // 备用：QueryFullProcessImageNameW 取完整路径后提取文件名
        let mut buf = [0u16; 260];
        let mut size = 260u32;
        if QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            PWSTR(buf.as_mut_ptr()),
            &mut size,
        )
        .is_ok()
        {
            let full = String::from_utf16_lossy(&buf[..size as usize]);
            if let Some(pos) = full.rfind('\\') {
                full[pos + 1..].to_string()
            } else {
                full
            }
        } else {
            "unknown".to_string()
        }
    }
}
