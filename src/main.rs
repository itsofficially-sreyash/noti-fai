//! ballpop: a bouncing-ball notification for when an AI agent finishes.
//!
//! There is no resident daemon. `ballpop notify` hands its event to a running
//! host (via WM_COPYDATA) or starts a detached one, then returns at once so
//! the caller (e.g. a Claude Code hook) is never blocked. The host exits as
//! soon as its last ball is dismissed, so between notifications nothing runs.

#![windows_subsystem = "windows"]

mod native;
mod watch;

use std::cell::RefCell;
use std::collections::HashMap;
use std::mem::{size_of, zeroed};
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::rc::Rc;

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Graphics::GdiPlus as gp;
use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Performance::*;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::Accessibility::*;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const HOST_CLASS: &str = "BallPopHost";
const BALL_CLASS: &str = "BallPopBall";
const MUTEX_NAME: &str = "Local\\BallPopHost";
const COPYDATA_MAGIC: usize = 0xBA11;
/// Asks the host to drop the balls owned by a callback window.
const COPYDATA_REMOVE: usize = 0xBA12;
const MAX_BALLS: usize = 6;
const TIMER_ID: usize = 1;
const BALL_DIP: u32 = 64;
// Speed kept on each bounce. Height scales with speed squared, so 1/sqrt(2)
// gives bounce heights of h, h/2, h/4, ...
const RESTITUTION: f32 = std::f32::consts::FRAC_1_SQRT_2;
const PIXEL_FORMAT_32BPP_PARGB: i32 = 0xE200B;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("notify") => unsafe { notify(&args[1..]) },
        Some("host") => unsafe { host(args.get(1).map_or("", String::as_str)) },
        Some("watch") => unsafe { watch::run() },
        // Chrome/Edge start native messaging hosts with the caller's origin.
        Some(origin) if origin.starts_with("chrome-extension://") => unsafe { native::run() },
        Some("quit") => unsafe {
            let host = find_host();
            if !host.is_null() {
                PostMessageW(host, WM_CLOSE, 0, 0);
            }
        },
        _ => unsafe { usage() },
    }
}

unsafe fn usage() {
    let text = wide(
        "ballpop notify [--agent NAME] [--icon IMAGE] [--hwnd WINDOW] [--always]\n\
         ballpop watch   (turn desktop AI app notifications into balls)\n\
         ballpop quit\n\n\
         --agent   claude, chatgpt, grok, gemini, ... (picks icons\\NAME.png,\n\
         \x20         else the installed app's own logo)\n\
         --icon    image to show on the ball (png, ico, jpg, bmp)\n\
         --hwnd    window to focus on click (default: the caller's window)\n\
         --always  show even if that window is already in front",
    );
    MessageBoxW(null_mut(), text.as_ptr(), wide("ballpop").as_ptr(), MB_OK);
}

// ---------------------------------------------------------------- client

unsafe fn notify(args: &[String]) {
    let mut agent = String::from("agent");
    let mut icon = String::new();
    let mut target: HWND = null_mut();
    let mut always = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--agent" if i + 1 < args.len() => {
                i += 1;
                agent = args[i]
                    .to_lowercase()
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                    .collect();
            }
            "--icon" if i + 1 < args.len() => {
                i += 1;
                icon = args[i].replace(['\t', '"'], "");
            }
            "--hwnd" if i + 1 < args.len() => {
                i += 1;
                target = args[i].parse::<isize>().unwrap_or(0) as HWND;
            }
            "--always" => always = true,
            _ => {}
        }
        i += 1;
    }
    if target.is_null() {
        target = find_caller_window();
    }
    deliver(&agent, target, &icon, null_mut(), always);
}

/// Shows a ball: hands it to the running host or starts one. `callback`, if
/// set, is a window that gets "click" or "dismiss" back (browser tabs).
unsafe fn deliver(agent: &str, target: HWND, icon: &str, callback: HWND, always: bool) {
    if !always && !target.is_null() && GetAncestor(GetForegroundWindow(), GA_ROOT) == target {
        return; // the user is already looking at it
    }
    let payload = format!("{agent}\t{}\t{icon}\t{}", target as isize, callback as isize);
    let running = find_host();
    if running.is_null() || !send_payload(running, COPYDATA_MAGIC, &payload) {
        spawn_host(&payload);
    }
}

/// The main window of a running app whose exe name starts with `agent`
/// (Claude.exe, ChatGPT.exe, ...). Windowless processes are skipped.
unsafe fn find_app_window(agent: &str) -> HWND {
    let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
    if snap == INVALID_HANDLE_VALUE {
        return null_mut();
    }
    let mut found: HWND = null_mut();
    let mut entry: PROCESSENTRY32W = zeroed();
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    let mut more = Process32FirstW(snap, &mut entry) != 0;
    while more && found.is_null() {
        let end = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
        if String::from_utf16_lossy(&entry.szExeFile[..end]).to_lowercase().starts_with(agent) {
            let mut search = (entry.th32ProcessID, null_mut::<core::ffi::c_void>());
            EnumWindows(Some(match_main_window), &mut search as *mut _ as LPARAM);
            found = search.1;
        }
        more = Process32NextW(snap, &mut entry) != 0;
    }
    CloseHandle(snap);
    found
}

/// Starts `ballpop host <payload>` fully detached: no console, no inherited
/// handles (so a hook's output pipes close), outside the caller's job object.
unsafe fn spawn_host(payload: &str) {
    let Ok(exe) = std::env::current_exe() else { return };
    let mut cmd = wide(&format!("\"{}\" host \"{payload}\"", exe.display()));
    let mut si: STARTUPINFOW = zeroed();
    si.cb = size_of::<STARTUPINFOW>() as u32;
    let mut pi: PROCESS_INFORMATION = zeroed();
    for flags in [DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB, DETACHED_PROCESS] {
        let ok = CreateProcessW(
            null(), cmd.as_mut_ptr(), null(), null(), 0, flags,
            null(), null(), &si, &mut pi,
        );
        if ok != 0 {
            CloseHandle(pi.hProcess);
            CloseHandle(pi.hThread);
            return;
        }
    }
}

/// Becomes the host, or forwards to one that won the race to start.
unsafe fn host(payload: &str) {
    let mutex = CreateMutexW(null(), 0, wide(MUTEX_NAME).as_ptr());
    for _ in 0..100 {
        let wait = WaitForSingleObject(mutex, 0);
        if wait == WAIT_OBJECT_0 || wait == WAIT_ABANDONED {
            run_host(mutex, &payload);
            return;
        }
        let host = find_host();
        if !host.is_null() && send_payload(host, COPYDATA_MAGIC, payload) {
            return;
        }
        Sleep(30); // a host is starting up or shutting down
    }
}

unsafe fn find_host() -> HWND {
    let class = wide(HOST_CLASS);
    FindWindowExW(HWND_MESSAGE, null_mut(), class.as_ptr(), null())
}

unsafe fn send_payload(to: HWND, kind: usize, payload: &str) -> bool {
    let data: Vec<u16> = payload.encode_utf16().collect();
    let cds = COPYDATASTRUCT {
        dwData: kind,
        cbData: (data.len() * 2) as u32,
        lpData: data.as_ptr() as *mut _,
    };
    let mut result = 0usize;
    let ok = SendMessageTimeoutW(
        to,
        WM_COPYDATA,
        0,
        &cds as *const _ as LPARAM,
        SMTO_ABORTIFHUNG,
        2000,
        &mut result,
    );
    ok != 0 && result == 1
}

/// Walks up the process tree (hook -> claude -> shell -> terminal/IDE/app)
/// and returns the first ancestor that owns a visible top-level window.
unsafe fn find_caller_window() -> HWND {
    let mut parents = HashMap::new();
    let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
    if snap != INVALID_HANDLE_VALUE {
        let mut entry: PROCESSENTRY32W = zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut entry) != 0 {
            loop {
                parents.insert(entry.th32ProcessID, entry.th32ParentProcessID);
                if Process32NextW(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
    let mut pid = GetCurrentProcessId();
    for _ in 0..32 {
        match parents.get(&pid) {
            Some(&parent) if parent != 0 && parent != pid => pid = parent,
            _ => break,
        }
        let mut search = (pid, null_mut::<core::ffi::c_void>());
        EnumWindows(Some(match_main_window), &mut search as *mut _ as LPARAM);
        if !search.1.is_null() {
            return search.1;
        }
    }
    null_mut()
}

unsafe extern "system" fn match_main_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let search = &mut *(lparam as *mut (u32, HWND));
    let mut pid = 0;
    GetWindowThreadProcessId(hwnd, &mut pid);
    // Only cheap local checks: anything that messages the window (like
    // reading its title) can stall on another app that is hung.
    let mut rect: RECT = zeroed();
    if pid == search.0
        && IsWindowVisible(hwnd) != 0
        && GetWindow(hwnd, GW_OWNER).is_null()
        && GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW == 0
        && GetWindowRect(hwnd, &mut rect) != 0
        && rect.right > rect.left
    {
        search.1 = hwnd;
        return 0;
    }
    1
}

// ------------------------------------------------------------------ host

struct Ball {
    hwnd: HWND,
    target: HWND,
    callback: HWND,
    slot: usize,
    dc: HDC,
    bmp: HBITMAP,
    bits: *mut u32,
    sprite: Rc<Vec<u32>>,
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    angle: f32,
    gravity: f32,
    floor: f32,
    left: f32,
    right: f32,
    friction: f32,
    /// Where this ball comes to rest; each slot gets its own spot.
    rest_x: f32,
    min_bounce: f32,
    rolling: bool,
    moving: bool,
}

impl Ball {
    unsafe fn destroy(self) {
        if !self.callback.is_null() {
            send_payload(self.callback, COPYDATA_MAGIC, "dismiss");
        }
        DestroyWindow(self.hwnd);
        DeleteDC(self.dc);
        DeleteObject(self.bmp);
    }
}

struct Host {
    hwnd: HWND,
    inst: HINSTANCE,
    size: i32,
    mask: Vec<f32>,
    light: Vec<f32>,
    shine: Vec<f32>,
    sprites: HashMap<String, Rc<Vec<u32>>>,
    balls: Vec<Ball>,
    gdiplus: usize,
    icon_dir: PathBuf,
    freq: i64,
    last_tick: i64,
    timer_on: bool,
}

thread_local! {
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
}

/// Runs `f` on the host unless it is already borrowed further up the stack
/// (a message delivered re-entrantly); callers treat `None` as "try later".
fn with_host<R>(f: impl FnOnce(&mut Host) -> R) -> Option<R> {
    HOST.with(|cell| cell.try_borrow_mut().ok()?.as_mut().map(f))
}

unsafe fn run_host(mutex: HANDLE, first: &str) {
    SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    let inst = GetModuleHandleW(null());
    let host_class = wide(HOST_CLASS);
    let ball_class = wide(BALL_CLASS);
    let mut wc: WNDCLASSW = zeroed();
    wc.hInstance = inst;
    wc.lpfnWndProc = Some(host_proc);
    wc.lpszClassName = host_class.as_ptr();
    RegisterClassW(&wc);
    wc.lpfnWndProc = Some(ball_proc);
    wc.lpszClassName = ball_class.as_ptr();
    wc.hCursor = LoadCursorW(null_mut(), IDC_HAND);
    RegisterClassW(&wc);

    let hwnd = CreateWindowExW(
        0, host_class.as_ptr(), null(), 0, 0, 0, 0, 0,
        HWND_MESSAGE, null_mut(), inst, null(),
    );
    let size = (BALL_DIP * GetDpiForSystem() / 96) as i32;
    let (mask, light, shine) = ball_shading(size as usize);
    let mut freq = 0;
    QueryPerformanceFrequency(&mut freq);
    let icon_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("icons")))
        .unwrap_or_default();
    HOST.with(|cell| {
        *cell.borrow_mut() = Some(Host {
            hwnd, inst, size, mask, light, shine,
            sprites: HashMap::new(),
            balls: Vec::new(),
            gdiplus: 0,
            icon_dir,
            freq,
            last_tick: 0,
            timer_on: false,
        })
    });

    let hook = SetWinEventHook(
        EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND,
        null_mut(), Some(on_foreground), 0, 0, WINEVENT_OUTOFCONTEXT,
    );
    with_host(|h| h.add_ball(first));

    let mut msg: MSG = zeroed();
    while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    UnhookWinEvent(hook);
    ReleaseMutex(mutex);
    CloseHandle(mutex);
}

impl Host {
    unsafe fn add_ball(&mut self, payload: &str) {
        let mut fields = payload.split('\t');
        let agent = fields.next().unwrap_or("agent");
        let target = fields.next().and_then(|t| t.parse::<isize>().ok()).unwrap_or(0) as HWND;
        let icon = fields.next().unwrap_or("");
        let callback = fields.next().and_then(|t| t.parse::<isize>().ok()).unwrap_or(0) as HWND;
        if self.balls.len() >= MAX_BALLS {
            self.balls.remove(0).destroy();
        }
        let slot = (0..).find(|s| self.balls.iter().all(|b| b.slot != *s)).unwrap();

        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(MonitorFromWindow(target, MONITOR_DEFAULTTOPRIMARY), &mut mi);
        let work = mi.rcWork;
        let (w, h) = ((work.right - work.left) as f32, (work.bottom - work.top) as f32);
        let r = self.size as f32 * 0.5;

        let drop = h * 0.55;
        let fall_time = 0.6;
        let gravity = 2.0 * drop / (fall_time * fall_time);
        let floor = work.bottom as f32 - r;

        let sprite = self.sprite(agent, icon);
        let n = self.size;
        let screen = GetDC(null_mut());
        let dc = CreateCompatibleDC(screen);
        ReleaseDC(null_mut(), screen);
        let mut bmi: BITMAPINFO = zeroed();
        bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = n;
        bmi.bmiHeader.biHeight = -n; // top-down
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB;
        let mut bits = null_mut();
        let bmp = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        SelectObject(dc, bmp);

        let spacing = self.size as f32 + 14.0;
        let x = work.left as f32 + r + 16.0 + slot as f32 * spacing;
        let (vx, friction) = (w * 0.11, w * 0.07);
        // Nominal travel: bouncing time (a geometric series of hops) at vx,
        // then a friction stop. Frame timing can add or drop a hop, so the
        // roll is retuned to land exactly here (see `step`).
        let hops = fall_time * (1.0 + 2.0 * RESTITUTION / (1.0 - RESTITUTION));
        let last_spot = work.right as f32 - r - (MAX_BALLS - 1 - slot) as f32 * spacing;
        let rest_x = (x + vx * hops + vx * vx / (2.0 * friction)).min(last_spot);
        let y = floor - drop;
        let title = wide(agent);
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            wide(BALL_CLASS).as_ptr(), title.as_ptr(), WS_POPUP,
            (x - r) as i32, (y - r) as i32, n, n,
            null_mut(), null_mut(), self.inst, null(),
        );
        let ball = Ball {
            hwnd, target, callback, slot, dc, bmp, bits: bits as *mut u32, sprite,
            x, y, vx: w * 0.11, vy: 0.0, angle: 0.0,
            gravity, floor,
            left: work.left as f32 + r,
            right: work.right as f32 - r,
            friction,
            rest_x,
            min_bounce: h * 0.07,
            rolling: false,
            moving: true,
        };
        self.draw(&ball);
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        self.balls.push(ball);

        if !self.timer_on {
            QueryPerformanceCounter(&mut self.last_tick);
            SetTimer(self.hwnd, TIMER_ID, 15, None);
            self.timer_on = true;
        }
    }

    unsafe fn tick(&mut self) {
        let mut now = 0;
        QueryPerformanceCounter(&mut now);
        let dt = ((now - self.last_tick) as f32 / self.freq as f32).min(0.05);
        self.last_tick = now;
        let r = self.size as f32 * 0.5;
        let mut any_moving = false;
        for i in 0..self.balls.len() {
            if !self.balls[i].moving {
                continue;
            }
            step(&mut self.balls[i], dt, r);
            self.draw(&self.balls[i]);
            any_moving |= self.balls[i].moving;
        }
        if !any_moving {
            // Everything is at rest: stop waking up entirely.
            KillTimer(self.hwnd, TIMER_ID);
            self.timer_on = false;
        }
    }

    /// Rotates the sprite into the window bitmap and applies fixed lighting,
    /// so the logo spins while the highlight stays put like on a real ball.
    unsafe fn draw(&self, b: &Ball) {
        let n = self.size as usize;
        let out = std::slice::from_raw_parts_mut(b.bits, n * n);
        let c = (n as f32 - 1.0) * 0.5;
        let (sin, cos) = b.angle.sin_cos();
        for py in 0..n {
            let dy = py as f32 - c;
            for px in 0..n {
                let i = py * n + px;
                let m = self.mask[i];
                if m <= 0.0 {
                    out[i] = 0;
                    continue;
                }
                let dx = px as f32 - c;
                let u = cos * dx + sin * dy + c;
                let v = -sin * dx + cos * dy + c;
                out[i] = shade(sample(&b.sprite, n, u, v), self.light[i], self.shine[i], m);
            }
        }
        let r = self.size as f32 * 0.5;
        let pos = POINT { x: (b.x - r).round() as i32, y: (b.y - r).round() as i32 };
        let size = SIZE { cx: self.size, cy: self.size };
        let origin = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        UpdateLayeredWindow(b.hwnd, null_mut(), &pos, &size, b.dc, &origin, 0, &blend, ULW_ALPHA);
    }

    /// Picks the ball's face: the bundled icons\<agent>.* (high-res), then an
    /// explicit --icon, then the logo of the agent's installed Windows app,
    /// then a plain colour.
    unsafe fn sprite(&mut self, agent: &str, icon: &str) -> Rc<Vec<u32>> {
        let key = format!("{agent}\t{icon}");
        if let Some(s) = self.sprites.get(&key) {
            return s.clone();
        }
        let n = self.size as usize;
        let icon_dir = self.icon_dir.clone();
        let candidates: Vec<PathBuf> = ["png", "ico", "jpg", "bmp"]
            .iter()
            .map(|ext| icon_dir.join(format!("{agent}.{ext}")))
            .chain(std::iter::once(PathBuf::from(icon)).filter(|_| !icon.is_empty()))
            .filter(|p| p.exists())
            .chain(installed_app_logo(agent))
            .collect();
        let mut face = None;
        for path in candidates {
            face = self.load_icon(&path, n);
            if face.is_some() {
                break;
            }
        }
        let s = Rc::new(face.unwrap_or_else(|| default_sprite(agent, n)));
        self.sprites.insert(key, s.clone());
        s
    }

    /// Decodes an image with GDI+ (started only when first needed). Icons
    /// that are opaque where the ball's circle passes (full-bleed squares,
    /// rounded app tiles) fill the ball; others sit on a white disc.
    unsafe fn load_icon(&mut self, path: &std::path::Path, n: usize) -> Option<Vec<u32>> {
        if self.gdiplus == 0 {
            let mut input: gp::GdiplusStartupInput = zeroed();
            input.GdiplusVersion = 1;
            gp::GdiplusStartup(&mut self.gdiplus, &input, null_mut());
        }
        let wpath = wide(path.to_str()?);
        let mut img: *mut gp::GpBitmap = null_mut();
        if gp::GdipCreateBitmapFromFile(wpath.as_ptr(), &mut img) != 0 || img.is_null() {
            return None;
        }
        let (mut w, mut h) = (0u32, 0u32);
        gp::GdipGetImageWidth(img as *mut gp::GpImage, &mut w);
        gp::GdipGetImageHeight(img as *mut gp::GpImage, &mut h);
        // Points just inside the inscribed circle, on the diagonals.
        let full_bleed = [(0.16, 0.16), (0.84, 0.16), (0.16, 0.84), (0.84, 0.84)].iter().all(|(fx, fy)| {
            let mut argb = 0u32;
            gp::GdipBitmapGetPixel(img, (w as f32 * fx) as i32, (h as f32 * fy) as i32, &mut argb);
            argb >> 24 > 200
        });
        let mut buf = vec![if full_bleed { 0 } else { 0xFFFF_FFFF }; n * n];
        let mut canvas: *mut gp::GpBitmap = null_mut();
        gp::GdipCreateBitmapFromScan0(
            n as i32, n as i32, (n * 4) as i32, PIXEL_FORMAT_32BPP_PARGB,
            buf.as_mut_ptr() as *mut u8, &mut canvas,
        );
        let mut g: *mut gp::GpGraphics = null_mut();
        gp::GdipGetImageGraphicsContext(canvas as *mut gp::GpImage, &mut g);
        gp::GdipSetInterpolationMode(g, 7); // HighQualityBicubic
        gp::GdipSetPixelOffsetMode(g, 4); // HighQuality
        let inset = if full_bleed { 0 } else { (n as f32 * 0.14) as i32 };
        let side = n as i32 - 2 * inset;
        gp::GdipDrawImageRectI(g, img as *mut gp::GpImage, inset, inset, side, side);
        gp::GdipDeleteGraphics(g);
        gp::GdipDisposeImage(canvas as *mut gp::GpImage);
        gp::GdipDisposeImage(img as *mut gp::GpImage);
        Some(buf)
    }
}

/// Finds the logo of an installed Store (MSIX) app whose package name
/// contains the agent name, e.g. Claude_... or OpenAI.ChatGPT-Desktop_...,
/// using the per-user package registry (no PowerShell, no directory scans).
fn installed_app_logo(agent: &str) -> Option<PathBuf> {
    const PACKAGES: &str = "Software\\Classes\\Local Settings\\Software\\Microsoft\\Windows\\CurrentVersion\\AppModel\\Repository\\Packages";
    const LOGOS: [&str; 6] = [
        "Square44x44Logo.targetsize-256.png",
        "Square44x44Logo.targetsize-256_altform-unplated.png",
        "Square150x150Logo.scale-400.png",
        "Square150x150Logo.scale-200.png",
        "StoreLogo.scale-400.png",
        "StoreLogo.png",
    ];
    if agent.len() < 3 {
        return None;
    }
    unsafe {
        let mut key: HKEY = null_mut();
        if RegOpenKeyExW(HKEY_CURRENT_USER, wide(PACKAGES).as_ptr(), 0, KEY_READ, &mut key) != 0 {
            return None;
        }
        let mut best: Option<(String, PathBuf)> = None;
        let mut name = [0u16; 512];
        for i in 0.. {
            let mut len = name.len() as u32;
            let status = RegEnumKeyExW(key, i, name.as_mut_ptr(), &mut len, null(), null_mut(), null_mut(), null_mut());
            if status != 0 {
                break;
            }
            let full = String::from_utf16_lossy(&name[..len as usize]);
            let package = full.split('_').next().unwrap_or("").to_lowercase();
            if !package.contains(agent) || best.as_ref().is_some_and(|(b, _)| *b > full) {
                continue;
            }
            let mut root = [0u16; 1024];
            let mut size = (root.len() * 2) as u32;
            let status = RegGetValueW(
                key, name.as_ptr(), wide("PackageRootFolder").as_ptr(),
                RRF_RT_REG_SZ, null_mut(), root.as_mut_ptr() as *mut _, &mut size,
            );
            if status == 0 {
                let end = root.iter().position(|&c| c == 0).unwrap_or(0);
                best = Some((full, PathBuf::from(String::from_utf16_lossy(&root[..end]))));
            }
        }
        RegCloseKey(key);
        let root = best?.1;
        ["Assets", "Images", ""]
            .iter()
            .flat_map(|dir| LOGOS.iter().map(|logo| root.join(dir).join(logo)).collect::<Vec<_>>())
            .find(|p| p.exists())
    }
}

fn step(b: &mut Ball, dt: f32, r: f32) {
    if !b.rolling {
        b.vy += b.gravity * dt;
        b.y += b.vy * dt;
        if b.y >= b.floor {
            b.y = b.floor;
            let up = b.vy * RESTITUTION;
            if up < b.min_bounce {
                b.vy = 0.0;
                b.rolling = true;
            } else {
                b.vy = -up;
            }
        }
    } else {
        // Brake just hard enough to stop on our own spot (v^2 = 2 a d), so
        // balls never pile up on each other.
        let ahead = b.rest_x - b.x;
        let friction = if b.vx > 0.0 && ahead > 1.0 {
            (b.vx * b.vx / (2.0 * ahead)).clamp(b.friction * 0.25, b.friction * 4.0)
        } else {
            b.friction * 4.0
        };
        let dv = friction * dt;
        if b.vx.abs() <= dv {
            b.vx = 0.0;
            b.moving = false;
        } else {
            b.vx -= b.vx.signum() * dv;
        }
    }
    b.x += b.vx * dt;
    if b.x > b.right || b.x < b.left {
        b.x = b.x.clamp(b.left, b.right);
        b.vx = -b.vx * 0.5;
    }
    // Rolling without slipping: arc length travelled equals distance moved.
    b.angle += b.vx * dt / r;
}

/// Precomputes the anti-aliased circle mask, diffuse light and specular
/// highlight once, so each frame is just a rotate-and-multiply.
fn ball_shading(n: usize) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
    let (mut mask, mut light, mut shine) = (vec![0.0; n * n], vec![0.0; n * n], vec![0.0; n * n]);
    let c = (n as f32 - 1.0) * 0.5;
    let r = n as f32 * 0.5;
    let l = { let (x, y, z) = (-0.45f32, -0.55f32, 0.70f32); let k = (x * x + y * y + z * z).sqrt(); (x / k, y / k, z / k) };
    let half = { let (x, y, z) = (l.0, l.1, l.2 + 1.0); let k = (x * x + y * y + z * z).sqrt(); (x / k, y / k, z / k) };
    for py in 0..n {
        for px in 0..n {
            let i = py * n + px;
            let (dx, dy) = (px as f32 - c, py as f32 - c);
            let dist = (dx * dx + dy * dy).sqrt();
            mask[i] = (r - dist + 0.5).clamp(0.0, 1.0);
            let (nx, ny) = (dx / r, dy / r);
            let nz = (1.0 - nx * nx - ny * ny).max(0.0).sqrt();
            let diffuse = (nx * l.0 + ny * l.1 + nz * l.2).max(0.0);
            light[i] = 0.68 + 0.40 * diffuse;
            shine[i] = (nx * half.0 + ny * half.1 + nz * half.2).max(0.0).powi(48) * 0.45;
        }
    }
    (mask, light, shine)
}

/// A beach-ball style sprite in the agent's colour, so rotation is visible
/// even without an icon. No third-party logos are bundled.
fn default_sprite(agent: &str, n: usize) -> Vec<u32> {
    let rgb = match agent {
        "claude" => 0xD97757,
        "chatgpt" | "openai" | "codex" => 0x10A37F,
        "grok" => 0x2B2B2B,
        "gemini" => 0x4285F4,
        "copilot" => 0x7B61FF,
        _ => 0x6C5CE7,
    };
    let tint = {
        let mix = |s: u32| ((s & 0xFF) * 45 / 100 + 255 * 55 / 100) & 0xFF;
        (mix(rgb >> 16) << 16) | (mix(rgb >> 8) << 8) | mix(rgb)
    };
    let c = (n as f32 - 1.0) * 0.5;
    let band = n as f32 * 0.09;
    let mut buf = vec![0u32; n * n];
    for py in 0..n {
        let dy = py as f32 - c;
        let colour = if dy.abs() < band { 0xFFFFFF } else if dy < 0.0 { rgb } else { tint };
        buf[py * n..(py + 1) * n].fill(0xFF00_0000 | colour);
    }
    buf
}

fn sample(src: &[u32], n: usize, u: f32, v: f32) -> [f32; 4] {
    let max = (n - 1) as f32;
    let (u, v) = (u.clamp(0.0, max), v.clamp(0.0, max));
    let (x0, y0) = (u as usize, v as usize);
    let (x1, y1) = ((x0 + 1).min(n - 1), (y0 + 1).min(n - 1));
    let (fx, fy) = (u - x0 as f32, v - y0 as f32);
    let px = [src[y0 * n + x0], src[y0 * n + x1], src[y1 * n + x0], src[y1 * n + x1]];
    let w = [(1.0 - fx) * (1.0 - fy), fx * (1.0 - fy), (1.0 - fx) * fy, fx * fy];
    let mut out = [0.0f32; 4];
    for (p, w) in px.iter().zip(w) {
        for (ch, o) in out.iter_mut().enumerate() {
            *o += ((p >> (ch * 8)) & 0xFF) as f32 * w;
        }
    }
    out // b, g, r, a (premultiplied)
}

fn shade(p: [f32; 4], light: f32, shine: f32, mask: f32) -> u32 {
    let a = p[3] * mask;
    let ch = |v: f32| ((v * light + shine * 255.0 * p[3] / 255.0) * mask).clamp(0.0, a) as u32;
    ((a as u32) << 24) | (ch(p[2]) << 16) | (ch(p[1]) << 8) | ch(p[0])
}

/// Removes every ball matching `pred`; the host exits after the last one.
unsafe fn dismiss(pred: impl Fn(&Ball) -> bool) {
    let removed = with_host(|h| {
        let (gone, keep): (Vec<Ball>, Vec<Ball>) = h.balls.drain(..).partition(|b| pred(b));
        h.balls = keep;
        (gone, h.balls.is_empty(), h.hwnd)
    });
    if let Some((gone, empty, host)) = removed {
        let any = !gone.is_empty();
        for b in gone {
            b.destroy();
        }
        if any && empty {
            DestroyWindow(host);
        }
    }
}

unsafe fn focus(target: HWND) {
    if target.is_null() || IsWindow(target) == 0 {
        return;
    }
    if IsIconic(target) != 0 {
        ShowWindow(target, SW_RESTORE);
    }
    if SetForegroundWindow(target) == 0 {
        // Windows can refuse a focus change; a synthetic Alt tap lifts that.
        keybd_event(VK_MENU as u8, 0, 0, 0);
        keybd_event(VK_MENU as u8, 0, KEYEVENTF_KEYUP, 0);
        SetForegroundWindow(target);
    }
}

unsafe extern "system" fn host_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_COPYDATA if (*(lparam as *const COPYDATASTRUCT)).dwData == COPYDATA_MAGIC => {
            let cds = &*(lparam as *const COPYDATASTRUCT);
            let data = std::slice::from_raw_parts(cds.lpData as *const u16, cds.cbData as usize / 2);
            let payload = String::from_utf16_lossy(data);
            with_host(|h| h.add_ball(&payload)).map_or(0, |_| 1)
        }
        WM_COPYDATA if (*(lparam as *const COPYDATASTRUCT)).dwData == COPYDATA_REMOVE => {
            let cds = &*(lparam as *const COPYDATASTRUCT);
            let data = std::slice::from_raw_parts(cds.lpData as *const u16, cds.cbData as usize / 2);
            let owner = String::from_utf16_lossy(data).parse::<isize>().unwrap_or(0) as HWND;
            if !owner.is_null() {
                dismiss(|b| b.callback == owner);
            }
            1
        }
        WM_TIMER => {
            with_host(|h| h.tick());
            0
        }
        WM_CLOSE => {
            dismiss(|_| true);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe extern "system" fn ball_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_LBUTTONUP => {
            let ball = with_host(|h| {
                let b = h.balls.iter_mut().find(|b| b.hwnd == hwnd)?;
                Some((b.target, std::mem::replace(&mut b.callback, null_mut())))
            });
            if let Some(Some((target, callback))) = ball {
                // The click made us the foreground app; pass that right on so
                // the browser may bring its own window forward.
                AllowSetForegroundWindow(ASFW_ANY);
                focus(target);
                if !callback.is_null() {
                    send_payload(callback, COPYDATA_MAGIC, "click");
                }
            }
            dismiss(|b| b.hwnd == hwnd);
            0
        }
        WM_RBUTTONUP => {
            dismiss(|b| b.hwnd == hwnd);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// If the user switches to an agent's window by themselves, its ball goes.
unsafe extern "system" fn on_foreground(
    _hook: HWINEVENTHOOK, _event: u32, hwnd: HWND,
    _object: i32, _child: i32, _thread: u32, _time: u32,
) {
    let root = GetAncestor(hwnd, GA_ROOT);
    if !root.is_null() {
        dismiss(|b| b.target == root);
    }
}
