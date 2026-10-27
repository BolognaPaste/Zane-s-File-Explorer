#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::fs;
use tauri::Emitter;

type R<T> = Result<T, String>;

fn e<E: std::fmt::Display>(x: E) -> String {
    x.to_string()
}

#[cfg(windows)]
fn quiet(c: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt;
    c.creation_flags(0x08000000)
}
#[cfg(not(windows))]
fn quiet(c: &mut Command) -> &mut Command {
    c
}

fn ms(t: io::Result<SystemTime>) -> f64 {
    t.ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

fn name(p: &str) -> R<OsString> {
    Path::new(p).file_name().map(|n| n.to_owned()).ok_or_else(|| "Invalid path".to_string())
}

fn rm_path(p: &Path) -> io::Result<()> {
    if p.is_dir() { fs::remove_dir_all(p) } else { fs::remove_file(p) }
}

fn cp(src: &Path, dst: &Path) -> io::Result<()> {
    if dst.exists() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{} already exists", dst.display())));
    }
    if dst.starts_with(src) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Cannot copy a folder into itself"));
    }
    if src.is_dir() {
        fs::create_dir_all(dst)?;
        for ent in fs::read_dir(src)? {
            let ent = ent?;
            cp(&ent.path(), &dst.join(ent.file_name()))?;
        }
    } else {
        fs::copy(src, dst)?;
    }
    Ok(())
}

// `c` must be a constant script. User data goes in through environment variables,
// never spliced into the script text (PowerShell treats curly quotes as quotes, so escaping is unsafe).
fn ps(c: &str, env: &[(&str, &str)]) -> R<()> {
    let mut cmd = Command::new("powershell.exe");
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", c]);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let o = quiet(&mut cmd).output().map_err(e)?;
    if o.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&o.stderr).trim().to_string()) }
}

// Same item? Windows paths ignore case.
fn same(a: &Path, b: &Path) -> bool {
    a == b || (cfg!(windows) && a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase())
}

// Err if `dst` is taken. fs::rename silently replaces existing files on Windows.
// A case-only rename of the same item (a.txt -> A.txt) is allowed on Windows.
fn vacant(src: &Path, dst: &Path) -> io::Result<()> {
    if !same(src, dst) && fs::symlink_metadata(dst).is_ok() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{} already exists", dst.display())));
    }
    Ok(())
}

// Move one item to an exact path, copying across drives
fn mv1(src: &Path, t: &Path) -> io::Result<()> {
    vacant(src, t)?;
    match fs::rename(src, t) {
        Ok(()) => Ok(()),
        // 17 = ERROR_NOT_SAME_DEVICE (Windows), 18 = EXDEV (Unix): only then copy + delete
        Err(er) if matches!(er.raw_os_error(), Some(17) | Some(18)) => {
            cp(src, t)?;
            rm_path(src)
        }
        Err(er) => Err(er),
    }
}

// A free name next to `t`: "name (2).ext", or with a tag "name - Copy.ext", "name - Copy (2).ext"
fn free(t: &Path, tag: &str, dir: bool) -> PathBuf {
    let at = t.parent().unwrap_or(Path::new(""));
    let n = t.file_name().map(|x| x.to_string_lossy().into_owned()).unwrap_or_default();
    let (stem, ext) = match n.rfind('.') {
        Some(i) if i > 0 && !dir => (n[..i].to_string(), n[i..].to_string()),
        _ => (n.clone(), String::new()),
    };
    for i in 1.. {
        if i == 1 && tag.is_empty() {
            continue;
        }
        let c = if i == 1 { format!("{stem}{tag}{ext}") } else { format!("{stem}{tag} ({i}){ext}") };
        let p = at.join(c);
        if fs::symlink_metadata(&p).is_err() {
            return p;
        }
    }
    unreachable!()
}

// Where `src` lands in `dest`. A name that is already taken is handled by `mode`: "replace" sends the
// old item to the Recycle Bin (and notes it in `gone` so undo can bring it back), "keep" picks a free
// name, "skip" leaves it out. Anything else leaves the clash for cp/vacant to report.
// Copying an item into its own folder makes "name - Copy", like Explorer.
fn target(src: &Path, dest: &Path, mode: &str, copying: bool, gone: &mut Vec<String>) -> R<Option<PathBuf>> {
    let t = dest.join(src.file_name().ok_or_else(|| "Invalid path".to_string())?);
    if same(src, &t) {
        return Ok(if copying { Some(free(&t, " - Copy", src.is_dir())) } else { None });
    }
    if fs::symlink_metadata(&t).is_err() {
        return Ok(Some(t));
    }
    match mode {
        "skip" => Ok(None),
        "keep" => Ok(Some(free(&t, "", src.is_dir()))),
        "replace" => {
            if src.starts_with(&t) {
                return Err(format!("{} cannot replace the folder it is in", src.display()));
            }
            trash::delete(&t).map_err(e)?;
            gone.push(t.to_string_lossy().into_owned());
            Ok(Some(t))
        }
        _ => Ok(Some(t)),
    }
}

fn known() -> Vec<(String, String)> {
    let s = |n: &str, p: Option<PathBuf>| p.map(|p| (n.to_string(), p.to_string_lossy().into_owned()));
    [
        s("Desktop", dirs::desktop_dir()),
        s("Documents", dirs::document_dir()),
        s("Downloads", dirs::download_dir()),
        s("Pictures", dirs::picture_dir()),
        s("Music", dirs::audio_dir()),
        s("Videos", dirs::video_dir()),
        s("Home", dirs::home_dir()),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn roots() -> Vec<String> {
    if cfg!(windows) {
        (b'C'..=b'Z').map(|c| format!("{}:\\", c as char)).filter(|p| Path::new(p).exists()).collect()
    } else {
        vec!["/".to_string()]
    }
}

#[derive(Serialize)]
struct Entry {
    name: String,
    hidden: bool,
    dir: bool,
    size: u64,
    mtime: f64,
    // Folder inside the listed folder, relative to it. Empty for a normal listing; search results fill it in.
    #[serde(skip_serializing_if = "String::is_empty")]
    sub: String,
}

fn is_hidden(m: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        m.file_attributes() & 0x2 != 0
    }
    #[cfg(not(windows))]
    {
        let _ = m;
        false
    }
}

// Sizes, dates and attributes come with the directory listing itself (FindNextFile on Windows), so no file is
// opened. Opening each one to ask, as before, was 100-400x slower: about 2 s instead of 5 ms for System32.
// Links and junctions are the exception: they are looked up so they show what they point to.
#[tauri::command]
async fn list(dir: String) -> R<Vec<Entry>> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut v = vec![];
        for ent in fs::read_dir(&dir).map_err(e)?.flatten() {
            let own = ent.metadata();
            let md = if own.as_ref().map_or(false, |m| m.file_type().is_symlink()) { fs::metadata(ent.path()).or(own) } else { own };
            let (d, s, m, h) = match md {
                Ok(m) => (m.is_dir(), m.len(), ms(m.modified()), is_hidden(&m)),
                Err(_) => (false, 0, 0.0, false),
            };
            let nm = ent.file_name().to_string_lossy().into_owned();
            v.push(Entry { hidden: h || nm.starts_with('.'), name: nm, dir: d, size: if d { 0 } else { s }, mtime: m, sub: String::new() });
        }
        Ok(v)
    })
    .await
    .map_err(e)?
}

// ---------- Search ----------
// The UI parses the query (kind:, size:, ext:, modified: and plain words) into this
// structure; all matching happens here so huge folders never cross the bridge.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchQ {
    terms: Vec<String>,
    exts: Option<Vec<String>>,
    dirs: bool,
    files: bool,
    min_size: Option<f64>,
    max_size: Option<f64>,
    after: Option<f64>,
    before: Option<f64>,
    hidden: bool,
}

// One counter per tab. Starting a new search in a tab bumps its counter, which makes the
// older search in that same tab stop early; searches in other tabs and panes are left alone.
fn search_slot(slot: u64) -> Arc<AtomicU64> {
    static SLOTS: OnceLock<Mutex<HashMap<u64, Arc<AtomicU64>>>> = OnceLock::new();
    let mut m = SLOTS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap_or_else(|x| x.into_inner());
    m.entry(slot).or_insert_with(|| Arc::new(AtomicU64::new(0))).clone()
}
const SEARCH_LIMIT: usize = 5000;

fn search_walk(root: &Path, q: &SearchQ, tok: &AtomicU64, my: u64) -> R<Value> {
    let terms: Vec<String> = q.terms.iter().map(|t| t.to_lowercase()).collect();
    let exts: Option<Vec<String>> = q.exts.as_ref().map(|v| v.iter().map(|x| x.to_lowercase()).collect());
    let mut rows: Vec<Entry> = vec![];
    let mut more = false;
    let mut stack = vec![root.to_path_buf()];
    'outer: while let Some(d) = stack.pop() {
        let rd = match fs::read_dir(&d) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for ent in rd.flatten() {
            if tok.load(Ordering::Relaxed) != my {
                return Err("cancelled".to_string());
            }
            // Links are skipped so a folder loop can never trap the walk
            match ent.file_type() {
                Ok(t) if !t.is_symlink() => {}
                _ => continue,
            }
            let md = match ent.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let nm = ent.file_name().to_string_lossy().into_owned();
            let hidden = is_hidden(&md) || nm.starts_with('.');
            if hidden && !q.hidden {
                continue;
            }
            let is_dir = md.is_dir();
            if is_dir {
                stack.push(ent.path());
                if !q.dirs {
                    continue;
                }
            } else {
                if !q.files {
                    continue;
                }
                if let Some(list) = &exts {
                    let x = Path::new(&nm).extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
                    if !list.iter().any(|y| *y == x) {
                        continue;
                    }
                }
            }
            let low = nm.to_lowercase();
            if !terms.iter().all(|t| low.contains(t.as_str())) {
                continue;
            }
            let size = if is_dir { 0 } else { md.len() };
            let sz = size as f64;
            if q.min_size.map_or(false, |m| sz < m) || q.max_size.map_or(false, |m| sz > m) {
                continue;
            }
            let mt = ms(md.modified());
            if q.after.map_or(false, |a| mt < a) || q.before.map_or(false, |b| mt >= b) {
                continue;
            }
            if rows.len() >= SEARCH_LIMIT {
                more = true;
                break 'outer;
            }
            // `name` stays the plain file name; `sub` is the folder it sits in, relative to the searched folder
            let sub = d.strip_prefix(root).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            rows.push(Entry { name: nm, hidden, dir: is_dir, size, mtime: mt, sub });
        }
    }
    Ok(json!({ "rows": rows, "more": more }))
}

#[tauri::command]
async fn search(dir: String, q: SearchQ, slot: u64) -> R<Value> {
    let tok = search_slot(slot);
    let my = tok.fetch_add(1, Ordering::SeqCst) + 1;
    tauri::async_runtime::spawn_blocking(move || search_walk(Path::new(&dir), &q, &tok, my)).await.map_err(e)?
}

#[tauri::command]
async fn places() -> Value {
    json!({ "places": known(), "drives": roots() })
}

#[tauri::command]
async fn pcinfo() -> Value {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let ds: Vec<Value> = roots()
        .into_iter()
        .map(|p| {
            let (t, f, n) = disks
                .list()
                .iter()
                .find(|d| d.mount_point().to_string_lossy().eq_ignore_ascii_case(&p))
                .map(|d| (d.total_space(), d.available_space(), d.name().to_string_lossy().into_owned()))
                .unwrap_or((0, 0, String::new()));
            json!({ "p": p, "total": t, "free": f, "name": n })
        })
        .collect();
    json!({ "places": known(), "drives": ds })
}

// ---------- Copy and move with progress ----------
// A running copy or move sends "job-progress" events (at most ten a second) and stops early
// once cancel_job sets its flag. Without a job id it runs silently.
fn job_flags() -> &'static Mutex<HashMap<u64, Arc<AtomicBool>>> {
    static J: OnceLock<Mutex<HashMap<u64, Arc<AtomicBool>>>> = OnceLock::new();
    J.get_or_init(Default::default)
}

struct Job {
    app: Option<(tauri::AppHandle, u64)>,
    stop: Arc<AtomicBool>,
    phase: &'static str,
    bytes: u64,
    total: u64,
    files: u64,
    count: u64,
    last: Instant,
}

impl Job {
    fn new(app: Option<(tauri::AppHandle, u64)>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        if let Some((_, id)) = &app {
            job_flags().lock().unwrap_or_else(|x| x.into_inner()).insert(*id, stop.clone());
        }
        Job { app, stop, phase: "scan", bytes: 0, total: 0, files: 0, count: 0, last: Instant::now() }
    }
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
    fn tick(&mut self, at: &Path, force: bool) {
        let Some((app, id)) = &self.app else { return };
        if !force && self.last.elapsed() < Duration::from_millis(100) {
            return;
        }
        self.last = Instant::now();
        let name = at.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let _ = app.emit(
            "job-progress",
            json!({ "job": id, "phase": self.phase, "bytes": self.bytes, "total": self.total, "files": self.files, "count": self.count, "name": name }),
        );
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        if let Some((_, id)) = &self.app {
            job_flags().lock().unwrap_or_else(|x| x.into_inner()).remove(id);
        }
    }
}

#[tauri::command]
async fn cancel_job(job: u64) {
    if let Some(f) = job_flags().lock().unwrap_or_else(|x| x.into_inner()).get(&job) {
        f.store(true, Ordering::Relaxed);
    }
}

// Adds up what is about to be copied, so progress can be shown as a share of the whole
fn scan(p: &Path, j: &mut Job) {
    if j.stopped() {
        return;
    }
    match fs::metadata(p) {
        Ok(m) if m.is_dir() => {
            if let Ok(rd) = fs::read_dir(p) {
                for ent in rd.flatten() {
                    scan(&ent.path(), j);
                }
            }
        }
        Ok(m) => {
            j.total += m.len();
            j.count += 1;
            j.tick(p, false);
        }
        Err(_) => {}
    }
}

fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "Cancelled")
}

// Like cp(), with progress. Files under 8 MB go through fs::copy, which keeps their attributes;
// bigger ones are copied in 1 MB pieces so Cancel works part way through them.
fn cp_job(src: &Path, dst: &Path, j: &mut Job) -> io::Result<()> {
    if j.stopped() {
        return Err(cancelled());
    }
    if dst.exists() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{} already exists", dst.display())));
    }
    if dst.starts_with(src) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Cannot copy a folder into itself"));
    }
    if src.is_dir() {
        fs::create_dir_all(dst)?;
        for ent in fs::read_dir(src)? {
            let ent = ent?;
            cp_job(&ent.path(), &dst.join(ent.file_name()), j)?;
        }
        return Ok(());
    }
    let md = fs::metadata(src)?;
    if md.len() < 8 << 20 {
        fs::copy(src, dst)?;
        j.bytes += md.len();
    } else {
        let mut r = fs::File::open(src)?;
        let mut w = fs::OpenOptions::new().write(true).create_new(true).open(dst)?;
        let mut buf = vec![0u8; 1 << 20];
        let res = (|| -> io::Result<()> {
            loop {
                if j.stopped() {
                    return Err(cancelled());
                }
                let n = r.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                w.write_all(&buf[..n])?;
                j.bytes += n as u64;
                j.tick(src, false);
            }
            w.set_modified(md.modified()?)
        })();
        drop(w);
        if let Err(er) = res {
            let _ = fs::remove_file(dst);
            return Err(er);
        }
        if md.permissions().readonly() {
            let _ = fs::set_permissions(dst, md.permissions());
        }
    }
    j.files += 1;
    j.tick(src, false);
    Ok(())
}

// Copies or moves items into `dest`. A move within one drive is a plain rename; everything else is
// copied with progress (and for a move, the original is deleted after its copy is complete).
// Reports {done: [[from, to]], replaced: [paths sent to the Recycle Bin], cancelled, error}.
// Items finished before a cancel or an error stay done, so the UI can still select and undo them.
// The item that was in progress is removed, and anything it was replacing is put back.
fn xfer(items: Vec<String>, dest: String, mode: Option<String>, copying: bool, j: &mut Job) -> Value {
    let (mode, dest) = (mode.unwrap_or_default(), PathBuf::from(dest));
    let (mut done, mut gone) = (vec![], vec![]);
    let (mut err, mut stop): (Option<String>, bool) = (None, false);
    let vol = |p: &Path| p.components().next().map(|c| c.as_os_str().to_string_lossy().to_lowercase());
    for s in items.iter().filter(|s| copying || vol(Path::new(s)) != vol(&dest)) {
        scan(Path::new(s), j);
    }
    j.phase = "copy";
    j.tick(&dest, true);
    for s in items {
        if j.stopped() {
            stop = true;
            break;
        }
        let src = PathBuf::from(&s);
        let n = gone.len();
        let t = match target(&src, &dest, &mode, copying, &mut gone) {
            Ok(Some(t)) => t,
            Ok(None) => continue,
            Err(x) => {
                err = Some(x);
                break;
            }
        };
        let rep = gone.get(n).cloned();
        let put_back = |gone: &mut Vec<String>| {
            if let Some(p) = &rep {
                gone.retain(|x| x != p);
                let _ = unrecycle(&[p.clone()]);
            }
        };
        if !copying {
            match vacant(&src, &t).and_then(|_| fs::rename(&src, &t)) {
                Ok(()) => {
                    done.push((s, t.to_string_lossy().into_owned()));
                    continue;
                }
                // 17 = ERROR_NOT_SAME_DEVICE (Windows), 18 = EXDEV (Unix): copy it below instead
                Err(er) if matches!(er.raw_os_error(), Some(17) | Some(18)) => {}
                Err(er) => {
                    put_back(&mut gone);
                    err = Some(e(er));
                    break;
                }
            }
        }
        // Only a copy this run created may be cleaned up; never an item that was already there
        let fresh = fs::symlink_metadata(&t).is_err();
        match cp_job(&src, &t, j) {
            Ok(()) => {
                if !copying {
                    if let Err(er) = rm_path(&src) {
                        err = Some(format!("Copied {} but could not remove the original: {}", src.display(), er));
                    }
                }
                done.push((s, t.to_string_lossy().into_owned()));
                if err.is_some() {
                    break;
                }
            }
            Err(er) => {
                if fresh {
                    let _ = rm_path(&t);
                }
                put_back(&mut gone);
                if j.stopped() { stop = true } else { err = Some(e(er)) }
                break;
            }
        }
    }
    json!({ "done": done, "replaced": gone, "cancelled": stop, "error": err })
}

async fn run_xfer(app: tauri::AppHandle, items: Vec<String>, dest: String, mode: Option<String>, job: Option<u64>, copying: bool) -> R<Value> {
    tauri::async_runtime::spawn_blocking(move || xfer(items, dest, mode, copying, &mut Job::new(job.map(|id| (app, id)))))
        .await
        .map_err(e)
}

#[tauri::command]
async fn copy(app: tauri::AppHandle, items: Vec<String>, dest: String, mode: Option<String>, job: Option<u64>) -> R<Value> {
    run_xfer(app, items, dest, mode, job, true).await
}

#[tauri::command]
async fn mv(app: tauri::AppHandle, items: Vec<String>, dest: String, mode: Option<String>, job: Option<u64>) -> R<Value> {
    run_xfer(app, items, dest, mode, job, false).await
}

// Copy without progress, for Send to
fn copy_quiet(items: Vec<String>, dest: String) -> R<()> {
    let v = xfer(items, dest, None, true, &mut Job::new(None));
    v["error"].as_str().map_or(Ok(()), |x| Err(x.to_string()))
}

// Names that would clash if `items` were put in `dest`
#[tauri::command]
async fn conflicts(items: Vec<String>, dest: String) -> Vec<String> {
    items
        .iter()
        .filter_map(|s| {
            let src = Path::new(s);
            let n = src.file_name()?;
            let t = Path::new(&dest).join(n);
            (!same(src, &t) && fs::symlink_metadata(&t).is_ok()).then(|| n.to_string_lossy().into_owned())
        })
        .collect()
}

// Copy or move each item to an exact path. Used by undo and redo.
#[tauri::command]
async fn place(pairs: Vec<(String, String)>, copying: bool) -> R<()> {
    for (a, b) in pairs {
        let (a, b) = (Path::new(&a), Path::new(&b));
        if copying { cp(a, b) } else { mv1(a, b) }.map_err(e)?;
    }
    Ok(())
}

#[tauri::command]
async fn recycle(items: Vec<String>) -> R<()> {
    trash::delete_all(&items).map_err(e)
}

#[tauri::command]
async fn restore(items: Vec<String>) -> R<()> {
    unrecycle(&items)
}

// Put items back from the Recycle Bin. When the same path was deleted more than once, the newest wins.
fn unrecycle(items: &[String]) -> R<()> {
    #[cfg(windows)]
    {
        let all = trash::os_limited::list().map_err(e)?;
        let pick: Vec<trash::TrashItem> = items
            .iter()
            .filter_map(|p| all.iter().filter(|t| same(&t.original_path(), Path::new(p))).max_by_key(|t| t.time_deleted).cloned())
            .collect();
        if pick.is_empty() {
            return Err("Those items are no longer in the Recycle Bin".to_string());
        }
        trash::os_limited::restore_all(pick).map_err(e)
    }
    #[cfg(not(windows))]
    {
        let _ = items;
        Err("Not supported on this platform".to_string())
    }
}

// ---------- Live refresh ----------
// The UI passes the folders it is showing. Changes inside them are collected and sent as one
// "fs-change" event (a list of folders) every 250 ms, so a big copy doesn't flood the UI.
struct Watch {
    w: notify::RecommendedWatcher,
    on: Vec<PathBuf>,
}

#[tauri::command]
async fn watch(app: tauri::AppHandle, dirs: Vec<String>) -> R<()> {
    use notify::Watcher;
    static W: OnceLock<Mutex<Option<Watch>>> = OnceLock::new();
    let mut g = W.get_or_init(|| Mutex::new(None)).lock().unwrap_or_else(|x| x.into_inner());
    if g.is_none() {
        let pend: Arc<Mutex<HashSet<String>>> = Default::default();
        let out = pend.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(250));
            let v: Vec<String> = out.lock().unwrap_or_else(|x| x.into_inner()).drain().collect();
            if !v.is_empty() {
                let _ = app.emit("fs-change", v);
            }
        });
        let w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else { return };
            if ev.kind.is_access() {
                return;
            }
            let mut s = pend.lock().unwrap_or_else(|x| x.into_inner());
            for p in ev.paths {
                if let Some(d) = p.parent() {
                    s.insert(d.to_string_lossy().into_owned());
                }
                s.insert(p.to_string_lossy().into_owned());
            }
        })
        .map_err(e)?;
        *g = Some(Watch { w, on: vec![] });
    }
    let st = g.as_mut().unwrap();
    let want: Vec<PathBuf> = dirs.iter().map(PathBuf::from).collect();
    for p in st.on.iter().filter(|p| !want.contains(p)) {
        let _ = st.w.unwatch(p);
    }
    st.on.retain(|p| want.contains(p));
    for p in want {
        if !st.on.contains(&p) && st.w.watch(&p, notify::RecursiveMode::NonRecursive).is_ok() {
            st.on.push(p);
        }
    }
    Ok(())
}

// Expands %USERPROFILE% and friends for paths typed in the address bar
#[tauri::command]
async fn envx(s: String) -> String {
    #[cfg(windows)]
    {
        reg::expand(&s)
    }
    #[cfg(not(windows))]
    {
        s
    }
}

#[tauri::command]
async fn rm(items: Vec<String>) -> R<()> {
    for s in items {
        rm_path(Path::new(&s)).map_err(e)?;
    }
    Ok(())
}

#[tauri::command]
async fn mkdir(p: String) -> R<()> {
    fs::create_dir(&p).map_err(e)
}

#[tauri::command]
async fn rename(a: String, b: String) -> R<()> {
    vacant(Path::new(&a), Path::new(&b)).map_err(e)?;
    fs::rename(&a, &b).map_err(e)
}

#[tauri::command]
async fn open_path(p: String) -> R<()> {
    open::that(&p).map_err(e)
}

#[tauri::command]
async fn read_text(p: String) -> R<String> {
    let mut f = fs::File::open(&p).map_err(e)?;
    let mut b = vec![0u8; 30000];
    let n = f.read(&mut b).map_err(e)?;
    b.truncate(n);
    Ok(String::from_utf8_lossy(&b).into_owned())
}

#[tauri::command]
async fn clip(t: String) -> R<()> {
    arboard::Clipboard::new().and_then(|mut c| c.set_text(t)).map_err(e)
}

#[tauri::command]
async fn show(p: String) -> R<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        Command::new("explorer").raw_arg(format!("/select,\"{}\"", p)).spawn().map_err(e)?;
    }
    #[cfg(not(windows))]
    {
        open::that(Path::new(&p).parent().unwrap_or(Path::new("/"))).map_err(e)?;
    }
    Ok(())
}

#[tauri::command]
async fn openwith(p: String) -> R<()> {
    // Same "openas" shell verb Explorer uses; errors are reported instead of swallowed
    ps("Start-Process -FilePath $env:VO_P -Verb OpenAs", &[("VO_P", &p)])
}

#[derive(Serialize)]
struct App {
    name: String,
    exe: String,
}

#[cfg(windows)]
mod reg {
    use super::App;
    use std::path::Path;
    use winreg::enums::*;
    use winreg::types::FromRegValue;
    use winreg::RegKey;

    pub fn expand(s: &str) -> String {
        let mut out = String::new();
        let mut rest = s;
        while let Some(i) = rest.find('%') {
            out.push_str(&rest[..i]);
            let after = &rest[i + 1..];
            match after.find('%') {
                Some(j) => {
                    match std::env::var(&after[..j]) {
                        Ok(v) => out.push_str(&v),
                        Err(_) => {
                            out.push('%');
                            out.push_str(&after[..j]);
                            out.push('%');
                        }
                    }
                    rest = &after[j + 1..];
                }
                None => {
                    out.push('%');
                    rest = after;
                }
            }
        }
        out.push_str(rest);
        out
    }

    // First .exe in a registry command line such as: "C:\Program Files\VLC\vlc.exe" --x "%1"
    fn exe_of(cmd: &str) -> Option<String> {
        let c = expand(cmd.trim());
        let p = if let Some(r) = c.strip_prefix('"') {
            r.split('"').next()?.to_string()
        } else {
            let i = c.to_ascii_lowercase().find(".exe")?;
            c[..i + 4].to_string()
        };
        Path::new(&p).is_file().then_some(p)
    }

    fn prog_exe(progid: &str) -> Option<String> {
        let k = RegKey::predef(HKEY_CLASSES_ROOT).open_subkey(format!("{}\\shell\\open\\command", progid)).ok()?;
        exe_of(&k.get_value::<String, _>("").ok()?)
    }

    fn named_exe(name: &str) -> Option<String> {
        if let Ok(k) = RegKey::predef(HKEY_CLASSES_ROOT).open_subkey(format!("Applications\\{}\\shell\\open\\command", name)) {
            if let Some(p) = k.get_value::<String, _>("").ok().and_then(|v| exe_of(&v)) {
                return Some(p);
            }
        }
        for hive in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
            let key = format!("Software\\Microsoft\\Windows\\CurrentVersion\\App Paths\\{}", name);
            if let Ok(k) = RegKey::predef(hive).open_subkey(key) {
                if let Ok(v) = k.get_value::<String, _>("") {
                    let p = expand(v.trim().trim_matches('"'));
                    if Path::new(&p).is_file() {
                        return Some(p);
                    }
                }
            }
        }
        None
    }

    fn title(exe: &str) -> String {
        let stem = Path::new(exe).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let mut c = stem.chars();
        c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
    }

    fn label(exe: &str) -> String {
        let file = Path::new(exe).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        RegKey::predef(HKEY_CLASSES_ROOT)
            .open_subkey(format!("Applications\\{}", file))
            .ok()
            .and_then(|k| k.get_value::<String, _>("FriendlyAppName").ok())
            .filter(|n| !n.is_empty() && !n.starts_with('@'))
            .unwrap_or_else(|| title(exe))
    }

    fn push(out: &mut Vec<App>, exe: Option<String>) {
        if let Some(exe) = exe {
            let lower = exe.to_ascii_lowercase();
            if lower.ends_with("\\openwith.exe") || out.iter().any(|a| a.exe.eq_ignore_ascii_case(&exe)) {
                return;
            }
            out.push(App { name: label(&exe), exe });
        }
    }

    // Programs Windows associates with this extension, like Explorer's "Open with" list
    pub fn apps(ext: &str) -> Vec<App> {
        let ext = format!(".{}", ext.trim_start_matches('.'));
        let hkcr = RegKey::predef(HKEY_CLASSES_ROOT);
        let fe = format!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\FileExts\\{}", ext);
        let user = RegKey::predef(HKEY_CURRENT_USER).open_subkey(&fe).ok();
        let mut out: Vec<App> = vec![];

        if let Some(id) = user
            .as_ref()
            .and_then(|k| k.open_subkey("UserChoice").ok())
            .and_then(|k| k.get_value::<String, _>("ProgId").ok())
        {
            push(&mut out, match id.strip_prefix("Applications\\") { Some(n) => named_exe(n), None => prog_exe(&id) });
        }
        if let Some(id) = hkcr.open_subkey(&ext).ok().and_then(|k| k.get_value::<String, _>("").ok()) {
            push(&mut out, prog_exe(&id));
        }
        if let Some(k) = user.as_ref().and_then(|k| k.open_subkey("OpenWithList").ok()) {
            for (n, v) in k.enum_values().flatten() {
                if n != "MRUList" {
                    if let Ok(name) = String::from_reg_value(&v) {
                        push(&mut out, named_exe(&name));
                    }
                }
            }
        }
        if let Some(k) = user.as_ref().and_then(|k| k.open_subkey("OpenWithProgids").ok()) {
            for (n, _) in k.enum_values().flatten() {
                push(&mut out, prog_exe(&n));
            }
        }
        if let Some(k) = hkcr.open_subkey(format!("{}\\OpenWithList", ext)).ok() {
            for n in k.enum_keys().flatten() {
                push(&mut out, named_exe(&n));
            }
        }
        out
    }
}

#[tauri::command]
async fn apps(ext: String) -> Vec<App> {
    #[cfg(windows)]
    {
        reg::apps(&ext)
    }
    #[cfg(not(windows))]
    {
        let _ = ext;
        vec![]
    }
}

// Only programs that apps() offers for this file's extension may be launched
#[tauri::command]
async fn openapp(exe: String, p: String) -> R<()> {
    #[cfg(windows)]
    {
        let ext = Path::new(&p).extension().map(|x| x.to_string_lossy().into_owned()).unwrap_or_default();
        if !reg::apps(&ext).iter().any(|a| a.exe.eq_ignore_ascii_case(&exe)) {
            return Err("That program is not registered for this file type".to_string());
        }
        Command::new(exe).arg(p).spawn().map(|_| ()).map_err(e)
    }
    #[cfg(not(windows))]
    {
        let _ = (exe, p);
        Err("Not supported on this platform".to_string())
    }
}

#[tauri::command]
async fn term(d: String) -> R<()> {
    quiet(Command::new("cmd.exe").args(["/c", "start", "cmd.exe"]).current_dir(&d)).spawn().map_err(e)?;
    Ok(())
}

// Returns the path of the new copy
#[tauri::command]
async fn dup(p: String) -> R<String> {
    let src = Path::new(&p);
    let n = free(src, " - Copy", src.is_dir());
    cp(src, &n).map_err(e)?;
    Ok(n.to_string_lossy().into_owned())
}

#[tauri::command]
async fn newfile(p: String) -> R<()> {
    fs::OpenOptions::new().write(true).create_new(true).open(&p).map(|_| ()).map_err(e)
}

#[tauri::command]
async fn link(p: String, d: String) -> R<()> {
    let lnk = Path::new(&d).join(format!("{} - Shortcut.lnk", name(&p)?.to_string_lossy()));
    ps(
        "$s=(New-Object -ComObject WScript.Shell).CreateShortcut($env:VO_LNK);$s.TargetPath=$env:VO_TGT;$s.Save()",
        &[("VO_LNK", &lnk.to_string_lossy()), ("VO_TGT", &p)],
    )
}

#[tauri::command]
async fn zip(items: Vec<String>, out: String) -> R<()> {
    // '|' cannot appear in a Windows path, so it is a safe separator
    let list = items.join("|");
    ps(
        "Compress-Archive -LiteralPath ($env:VO_ITEMS -split '\\|') -DestinationPath $env:VO_OUT",
        &[("VO_ITEMS", &list), ("VO_OUT", &out)],
    )
}

#[tauri::command]
async fn unzip(z: String, d: String) -> R<()> {
    ps(
        "Expand-Archive -LiteralPath $env:VO_ZIP -DestinationPath $env:VO_DEST",
        &[("VO_ZIP", &z), ("VO_DEST", &d)],
    )
}

fn walk(d: &Path, s: &mut u64, f: &mut u64, n: &mut u64) {
    if let Ok(rd) = fs::read_dir(d) {
        for ent in rd.flatten() {
            match ent.metadata() {
                Ok(m) if m.is_dir() => {
                    *n += 1;
                    walk(&ent.path(), s, f, n);
                }
                Ok(m) => {
                    *f += 1;
                    *s += m.len();
                }
                Err(_) => {}
            }
        }
    }
}

#[tauri::command]
async fn props(p: String) -> R<Value> {
    let m = fs::metadata(&p).map_err(e)?;
    let (mut s, mut f, mut d) = (m.len(), 0u64, 0u64);
    if m.is_dir() {
        s = 0;
        walk(Path::new(&p), &mut s, &mut f, &mut d);
    }
    Ok(json!({ "size": s, "files": f, "dirs": d, "dir": m.is_dir(), "ctime": ms(m.created()), "mtime": ms(m.modified()), "atime": ms(m.accessed()) }))
}

#[tauri::command]
async fn rar(args: Vec<String>) -> R<()> {
    let exe = ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"]
        .iter()
        .filter_map(|k| std::env::var_os(k))
        .map(|d| PathBuf::from(d).join("WinRAR").join("WinRAR.exe"))
        .find(|p| p.exists())
        .ok_or_else(|| "WinRAR was not found. Install it from rarlab.com.".to_string())?;
    let mut c = Command::new(exe);
    // WinRAR parses its own command line, so quote paths by hand (raw_arg) the way its docs show,
    // e.g. x -- "C:\a.rar" "C:\out\"  (a trailing backslash must survive intact)
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        for a in &args {
            if a.contains(|ch: char| " \\?:".contains(ch)) {
                c.raw_arg(format!("\"{}\"", a));
            } else {
                c.raw_arg(a);
            }
        }
    }
    #[cfg(not(windows))]
    {
        c.args(&args);
    }
    // Blocks until the WinRAR window is closed or finishes, so the file list refreshes afterwards
    let st = c.status().map_err(e)?;
    if st.success() { Ok(()) } else { Err(format!("WinRAR exited with code {}", st.code().unwrap_or(-1))) }
}

// ---------- Send to ----------
// Targets come from the user's own SendTo folder, the same list Explorer's "Send to" menu is built from.
fn sendto_dir() -> R<PathBuf> {
    dirs::config_dir()
        .map(|d| d.join("Microsoft").join("Windows").join("SendTo"))
        .ok_or_else(|| "Could not find the Send to folder".to_string())
}

// Like ps(), but returns what the script printed. Output is forced to UTF-8 so non-English paths survive.
fn ps_out(c: &str, env: &[(&str, &str)]) -> R<String> {
    let script = format!("[Console]::OutputEncoding=[System.Text.Encoding]::UTF8;{}", c);
    let mut cmd = Command::new("powershell.exe");
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", script.as_str()]);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let o = quiet(&mut cmd).output().map_err(e)?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).trim_start_matches('\u{feff}').to_string())
    } else {
        Err(String::from_utf8_lossy(&o.stderr).trim().to_string())
    }
}

fn exe_ext(p: &Path) -> bool {
    p.extension()
        .map(|x| {
            let x = x.to_string_lossy().to_lowercase();
            x == "exe" || x == "bat" || x == "cmd" || x == "com"
        })
        .unwrap_or(false)
}

#[derive(Serialize)]
struct SendTarget {
    id: String,
    name: String,
}

#[tauri::command]
async fn sendto_list() -> Vec<SendTarget> {
    let mut v: Vec<SendTarget> = vec![];
    let dir = match sendto_dir() {
        Ok(d) => d,
        Err(_) => return v,
    };
    let rd = match fs::read_dir(&dir) {
        Ok(r) => r,
        Err(_) => return v,
    };
    let mut links = false;
    for ent in rd.flatten() {
        let p = ent.path();
        if !p.is_file() {
            continue;
        }
        let ext = p.extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
        let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let id = ent.file_name().to_string_lossy().into_owned();
        match ext.as_str() {
            "zfsendtotarget" | "desklink" | "mydocs" => v.push(SendTarget { id, name: stem }),
            "lnk" => links = true,
            _ => {}
        }
    }
    if links {
        // One PowerShell call resolves every shortcut. Only folders and programs are usable targets.
        let script = "$w=New-Object -ComObject WScript.Shell;Get-ChildItem -LiteralPath $env:VO_DIR -Filter *.lnk -File | ForEach-Object { $t=$w.CreateShortcut($_.FullName).TargetPath; if ($t) { $_.Name + '|' + $t } }";
        if let Ok(out) = ps_out(script, &[("VO_DIR", &dir.to_string_lossy())]) {
            for line in out.lines() {
                if let Some((id, target)) = line.trim().split_once('|') {
                    let t = Path::new(target);
                    if t.is_dir() || (t.is_file() && exe_ext(t)) {
                        let stem = Path::new(id).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                        v.push(SendTarget { id: id.to_string(), name: stem });
                    }
                }
            }
        }
    }
    v.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    v
}

#[tauri::command]
async fn sendto(id: String, items: Vec<String>) -> R<()> {
    if items.is_empty() {
        return Ok(());
    }
    // `id` must be a bare file name from the SendTo folder, never a path
    if id.contains('\\') || id.contains('/') || id.contains("..") {
        return Err("Invalid Send to target".to_string());
    }
    let entry = sendto_dir()?.join(&id);
    if !entry.is_file() {
        return Err("That Send to target no longer exists".to_string());
    }
    let ext = entry.extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "zfsendtotarget" => {
            let first = Path::new(&items[0]);
            let parent = first.parent().ok_or_else(|| "Invalid path".to_string())?;
            let st: Option<String> = if items.len() == 1 {
                first.file_stem().map(|s| s.to_string_lossy().into_owned())
            } else {
                parent.file_name().map(|s| s.to_string_lossy().into_owned())
            };
            let stem = st.unwrap_or_else(|| "Archive".to_string());
            let mut out = parent.join(format!("{}.zip", stem));
            let mut i = 2;
            while out.exists() {
                out = parent.join(format!("{} ({}).zip", stem, i));
                i += 1;
            }
            let out = out.to_string_lossy().into_owned();
            zip(items, out).await
        }
        "desklink" => {
            let d = dirs::desktop_dir().ok_or_else(|| "Could not find the Desktop folder".to_string())?;
            let d = d.to_string_lossy().into_owned();
            for it in items {
                link(it, d.clone()).await?;
            }
            Ok(())
        }
        "mydocs" => {
            let d = dirs::document_dir().ok_or_else(|| "Could not find the Documents folder".to_string())?;
            copy_quiet(items, d.to_string_lossy().into_owned())
        }
        "lnk" => {
            let out = ps_out(
                "$s=(New-Object -ComObject WScript.Shell).CreateShortcut($env:VO_LNK);$s.TargetPath;$s.Arguments",
                &[("VO_LNK", &entry.to_string_lossy())],
            )?;
            let mut lines = out.lines();
            let target = lines.next().unwrap_or("").trim().to_string();
            let args = lines.next().unwrap_or("").trim().to_string();
            let t = PathBuf::from(&target);
            if t.is_dir() {
                return copy_quiet(items, target);
            }
            if t.is_file() && exe_ext(&t) {
                // Same as dropping the files on the shortcut: run the program with the files as arguments
                let mut c = Command::new(&t);
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    if !args.is_empty() {
                        c.raw_arg(&args);
                    }
                    for it in &items {
                        c.raw_arg(format!("\"{}\"", it));
                    }
                }
                #[cfg(not(windows))]
                {
                    let _ = &args;
                    c.args(&items);
                }
                c.spawn().map(|_| ()).map_err(e)?;
                return Ok(());
            }
            Err("This Send to target cannot be used here".to_string())
        }
        _ => Err("This Send to target is not supported".to_string()),
    }
}

fn main() {
    tauri::Builder::default()
        // Native drag, so files can be dragged out of the app into other programs
        .plugin(tauri_plugin_drag::init())
        .invoke_handler(tauri::generate_handler![
            list, places, pcinfo, copy, mv, recycle, rm, mkdir, rename, open_path, read_text, clip, show, openwith,
            term, apps, openapp, dup, newfile, link, zip, unzip, props, rar, sendto_list, sendto, search, conflicts, place,
            restore, watch, envx, cancel_job
        ])
        .run(tauri::generate_context!())
        .expect("error while running Zane's File Explorer");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("vo-test-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }
    fn s(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }
    fn run(items: &[&Path], dest: &Path, mode: &str, copying: bool, j: &mut Job) -> Value {
        xfer(items.iter().map(|p| s(p)).collect(), s(dest), Some(mode.to_string()), copying, j)
    }

    #[test]
    fn lists_sizes_dates_and_folders_from_the_listing() {
        let d = tmp("list");
        fs::create_dir(d.join("sub")).unwrap();
        fs::write(d.join("a.txt"), "hello").unwrap();
        fs::write(d.join(".dot"), "").unwrap();
        let mut v = tauri::async_runtime::block_on(list(s(&d))).unwrap();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        let got: Vec<_> = v.iter().map(|x| (x.name.as_str(), x.dir, x.size, x.hidden, x.mtime > 0.0)).collect();
        assert_eq!(got, [(".dot", false, 0, true, true), ("a.txt", false, 5, false, true), ("sub", true, 0, false, true)]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn copies_folders_and_big_files() {
        let d = tmp("copy");
        let (src, out) = (d.join("src"), d.join("out"));
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::create_dir(&out).unwrap();
        let big: Vec<u8> = (0..9 << 20).map(|i| (i % 251) as u8).collect();
        fs::write(src.join("sub").join("big.bin"), &big).unwrap();
        fs::write(src.join("a.txt"), "hello").unwrap();
        let mut j = Job::new(None);
        let v = run(&[&src], &out, "", true, &mut j);
        assert_eq!(v["error"], Value::Null);
        assert_eq!(v["done"].as_array().unwrap().len(), 1);
        assert_eq!(fs::read(out.join("src").join("sub").join("big.bin")).unwrap(), big);
        assert_eq!(fs::read_to_string(out.join("src").join("a.txt")).unwrap(), "hello");
        assert_eq!((j.files, j.count, j.bytes, j.total), (2, 2, big.len() as u64 + 5, big.len() as u64 + 5));
        let m = |p: PathBuf| fs::metadata(p).unwrap().modified().unwrap();
        assert_eq!(m(src.join("sub").join("big.bin")), m(out.join("src").join("sub").join("big.bin")));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_clash_never_touches_the_item_already_there() {
        let d = tmp("clash");
        let (a, out) = (d.join("a.txt"), d.join("out"));
        fs::create_dir(&out).unwrap();
        fs::write(&a, "new").unwrap();
        fs::write(out.join("a.txt"), "old").unwrap();
        let v = run(&[&a], &out, "", true, &mut Job::new(None));
        assert!(v["error"].as_str().unwrap().contains("already exists"));
        assert_eq!(fs::read_to_string(out.join("a.txt")).unwrap(), "old");
        let v = run(&[&a], &out, "keep", true, &mut Job::new(None));
        assert_eq!(v["error"], Value::Null);
        assert_eq!(fs::read_to_string(out.join("a (2).txt")).unwrap(), "new");
        let v = run(&[&a], &out, "skip", false, &mut Job::new(None));
        assert_eq!(v["done"].as_array().unwrap().len(), 0);
        assert!(a.exists());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn cancel_leaves_no_half_copies_and_keeps_the_originals() {
        let d = tmp("cancel");
        let (a, out) = (d.join("a.txt"), d.join("out"));
        fs::create_dir(&out).unwrap();
        fs::write(&a, "x").unwrap();
        let mut j = Job::new(None);
        j.stop.store(true, Ordering::Relaxed);
        let v = run(&[&a], &out, "", false, &mut j);
        assert_eq!(v["cancelled"], true);
        assert_eq!(v["done"].as_array().unwrap().len(), 0);
        assert!(a.exists() && !out.join("a.txt").exists());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn copy_into_its_own_folder_makes_a_copy_and_move_renames() {
        let d = tmp("same");
        let (a, sub) = (d.join("a.txt"), d.join("sub"));
        fs::create_dir(&sub).unwrap();
        fs::write(&a, "x").unwrap();
        run(&[&a], &d, "", true, &mut Job::new(None));
        assert!(d.join("a - Copy.txt").exists());
        let v = run(&[&a], &sub, "", false, &mut Job::new(None));
        assert_eq!(v["done"][0][1], s(&sub.join("a.txt")));
        assert!(!a.exists() && sub.join("a.txt").exists());
        let _ = fs::remove_dir_all(&d);
    }
}
