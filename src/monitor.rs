//! crash / ANR 监控：流式读取 logcat，识别异常块，落盘并产出结构化事件。
//!
//! 识别依据（与 Android 系统行为一致）：
//!  - Java 崩溃：`FATAL EXCEPTION` / `AndroidRuntime` / `am_crash`（events buffer）
//!  - Native 崩溃：`FATAL SIGNAL` / `tombstone` / `*** ***`
//!  - ANR：`ANR in ` / `am_anr` / `Input dispatching timed out`

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::device::Device;
use crate::util::now_str;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IncidentKind {
    Crash,
    Anr,
    NativeCrash,
}

impl IncidentKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            IncidentKind::Crash => "crash",
            IncidentKind::Anr => "anr",
            IncidentKind::NativeCrash => "native_crash",
        }
    }
    pub fn cn(&self) -> &'static str {
        match self {
            IncidentKind::Crash => "Java 崩溃",
            IncidentKind::Anr => "ANR",
            IncidentKind::NativeCrash => "Native 崩溃",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Incident {
    pub kind: IncidentKind,
    pub time: String,
    pub package: String,
    pub summary: String,
    pub detail: String,
    /// 归档文件名（相对 session 目录）
    pub file: Option<String>,
    /// 发生时的步骤序号
    pub step: Option<usize>,
    /// 异常现场截图（相对 session 目录）。崩溃后界面很快会变，
    /// 这张图记录的是「出问题时屏幕上是什么样」。
    #[serde(default)]
    pub shot: Option<String>,
    /// 现场截图的缩略图（报告里用，点开看大图）
    #[serde(default)]
    pub shot_thumb: Option<String>,
}

impl Incident {
    pub fn kind_cn(&self) -> String {
        self.kind.cn().to_string()
    }
}

/// 判定一行是否属于异常。
///
/// 返回 `(类型, 强触发)`。强触发表示这是**新一次**异常的开场白
/// （`FATAL EXCEPTION` / `ANR in` / `*** ***`），弱触发通常是同一异常的补充行
/// （`Input dispatching timed out` / `Shutting down VM` / `tombstone`）。
/// 强触发可打断正在收集的异常块，避免紧随其后的 crash + ANR 被合并成一条。
fn classify(line: &str) -> Option<(IncidentKind, bool)> {
    if line.contains("ANR in ") || line.contains("am_anr") {
        return Some((IncidentKind::Anr, true));
    }
    if line.contains("Input dispatching timed out") {
        return Some((IncidentKind::Anr, false));
    }
    if line.contains("FATAL SIGNAL") || line.contains("*** ***") {
        return Some((IncidentKind::NativeCrash, true));
    }
    if line.contains("tombstone") {
        return Some((IncidentKind::NativeCrash, false));
    }
    if line.contains("FATAL EXCEPTION") || line.contains("am_crash") {
        return Some((IncidentKind::Crash, true));
    }
    if line.contains("AndroidRuntime:")
        && (line.contains("Shutting down VM") || line.contains("FATAL"))
    {
        return Some((IncidentKind::Crash, false));
    }
    None
}

/// 新的 logcat 记录行：`09-09 23:50:01.123  1234  1234 E tag:`
#[cfg(test)]
fn is_log_start(line: &str) -> bool {
    let b = line.as_bytes();
    // "MM-DD HH:MM:SS.mmm" -> 0,1 数字；2 '-'；3,4 数字；5 空格；6,7 数字；8 ':'
    b.len() > 18
        && b[0].is_ascii_digit()
        && b[1].is_ascii_digit()
        && b[2] == b'-'
        && b[3].is_ascii_digit()
        && b[4].is_ascii_digit()
        && b[5] == b' '
        && b[6].is_ascii_digit()
        && b[7].is_ascii_digit()
        && b[8] == b':'
}

pub struct LogcatMonitor {
    rx: Receiver<Incident>,
    child: Arc<Mutex<Child>>,
    buf: Arc<Mutex<Option<std::fs::File>>>,
    stop: Arc<Mutex<bool>>,
}

impl LogcatMonitor {
    /// 启动 logcat 子进程与监听线程
    pub fn start(dev: &dyn Device, package: &str, log_path: &std::path::Path) -> Result<Self> {
        let mut child = spawn_logcat(dev)?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("无法获取 logcat stdout"))?;

        let (tx, rx): (Sender<Incident>, Receiver<Incident>) = channel();
        let file = std::fs::File::create(log_path)?;
        let buf = Arc::new(Mutex::new(Some(file)));
        let stop = Arc::new(Mutex::new(false));

        let pkg = package.to_string();
        let buf2 = buf.clone();
        let stop2 = stop.clone();
        std::thread::spawn(move || {
            let _ = run_loop(stdout, tx, &pkg, buf2, stop2);
        });

        Ok(Self {
            rx,
            child: Arc::new(Mutex::new(child)),
            buf,
            stop,
        })
    }

    /// 取出当前累积的异常（非阻塞）
    pub fn poll(&self) -> Vec<Incident> {
        let mut out = Vec::new();
        while let Ok(inc) = self.rx.try_recv() {
            out.push(inc);
        }
        out
    }

    pub fn stop(&self) {
        if let Ok(mut s) = self.stop.lock() {
            *s = true;
        }
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Ok(mut b) = self.buf.lock() {
            if let Some(mut f) = b.take() {
                let _ = f.flush();
            }
        }
    }
}

/// 启动 logcat 子进程；优先带 main/system/events/crash buffer，
/// 若设备不支持（会立即退出）则退化为默认 buffer。
fn spawn_logcat(dev: &dyn Device) -> Result<Child> {
    let mut c = dev.logcat_cmd();
    // stderr 必须丢弃：logcat 是长跑进程，若用管道又不去读，缓冲区写满后会把它堵死
    c.args(["-b", "main", "-b", "system", "-b", "events", "-b", "crash"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = c.spawn()?;

    // 给 800ms 判断是否因为不支持某个 buffer 而退出
    std::thread::sleep(Duration::from_millis(800));
    if let Ok(Some(_st)) = child.try_wait() {
        child = dev
            .logcat_cmd()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
    }
    Ok(child)
}

fn run_loop<R: std::io::Read>(
    reader: R,
    tx: Sender<Incident>,
    package: &str,
    buf: Arc<Mutex<Option<std::fs::File>>>,
    stop: Arc<Mutex<bool>>,
) -> Result<()> {
    let mut br = BufReader::new(reader);
    let mut line = String::new();
    // 滚动窗口：(绝对行号, 内容)
    let mut window: VecDeque<(u64, String)> = VecDeque::with_capacity(1024);
    let mut total: u64 = 0;
    // 正在收集的异常块
    let mut pending: Option<Block> = None;
    // 最近上报过的异常：(类型|包名) → 时间，用于跨日志通道去重
    let mut recent: HashMap<String, Instant> = HashMap::new();

    loop {
        line.clear();
        let n = br.read_line(&mut line)?;
        if n == 0 {
            break; // EOF
        }
        let line = line.trim_end_matches(['\n', '\r']).to_string();

        // 写原始日志
        if let Ok(mut g) = buf.lock() {
            if let Some(f) = g.as_mut() {
                let _ = writeln!(f, "{}", line);
            }
        }

        total += 1;
        window.push_back((total, line.clone()));
        if window.len() > 800 {
            window.pop_front();
        }

        if let Some((kind, strong)) = classify(&line) {
            // 触发行本身常常不含包名（包名在下一行的 "Process: com.xxx"），
            // 因此这里不按包名过滤，统一在收集完成后按整块内容过滤
            let start_new = match &pending {
                None => true,
                // 类型不同，或又出现了一次强触发 → 上一次异常已完整，先落盘再开新块。
                // 否则（同一异常的补充行）只是延长收集窗口，让堆栈收全。
                Some(b) => b.kind != kind || strong,
            };
            if start_new {
                if let Some(b) = pending.take() {
                    flush_block(b, total, &window, package, &tx, &mut recent);
                }
                pending = Some(Block {
                    kind,
                    start_idx: total,
                    deadline: Instant::now() + Duration::from_millis(2500),
                    summary: line.clone(),
                });
            } else if let Some(b) = pending.as_mut() {
                b.deadline = Instant::now() + Duration::from_millis(800);
            }
        }

        // 到期 / 过长则收集
        let due = pending
            .as_ref()
            .map(|b| Instant::now() >= b.deadline || total - b.start_idx > 300)
            .unwrap_or(false);
        if due {
            if let Some(b) = pending.take() {
                flush_block(b, total + 1, &window, package, &tx, &mut recent);
            }
        }

        if *stop.lock().unwrap() {
            break;
        }
    }

    // EOF：把还没收集完的异常块冲刷出去（例如 logcat 进程结束）
    if let Some(b) = pending.take() {
        flush_block(b, total + 1, &window, package, &tx, &mut recent);
    }
    Ok(())
}

/// 同一次异常常被多条日志通道重复上报 —— 一次 Java 崩溃会同时产生
/// AndroidRuntime 的 `FATAL EXCEPTION` 和 ActivityManager 的 `am_crash`，
/// 一次 ANR 也会同时有 `ANR in` 和 `am_anr`。这个窗口内、同类型同包名的异常
/// 视为同一事件，只记一条，否则报告里的 crash/anr 计数会凭空翻倍。
const DEDUP_WINDOW: Duration = Duration::from_secs(5);

/// 从异常详情里抽出「特征」，用来判断两条上报是不是同一个事件。
///
/// 同一次崩溃会在两条日志通道里出现，内容形态完全不同（一边是 Java 堆栈、
/// 一边是 events buffer 的 `am_crash` 单行），但它们**都含同一个异常类名**，
/// 所以拿类名当指纹比单纯用时间窗口更准：
/// 既能合并同一事件的重复上报，又不会把 5 秒内连续发生的两个不同崩溃吞掉。
fn incident_fingerprint(detail: &str) -> Option<String> {
    for line in detail.lines() {
        // Native 崩溃：FATAL SIGNAL 11 → signal-11
        if let Some(i) = line.find("FATAL SIGNAL") {
            let sig: String = line[i + "FATAL SIGNAL".len()..]
                .trim_start()
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if !sig.is_empty() {
                return Some(format!("signal-{sig}"));
            }
        }
        // Java 崩溃：java.lang.NullPointerException / OutOfMemoryError …
        for tok in line.split(|c: char| !(c.is_alphanumeric() || c == '.' || c == '_' || c == '$'))
        {
            if tok.len() > 8 && (tok.ends_with("Exception") || tok.ends_with("Error")) {
                return Some(tok.to_string());
            }
        }
    }
    None
}

/// 形如包名的字符串：有点号、只含字母数字下划线和点、以字母开头。
fn is_pkg_like(s: &str) -> bool {
    let s = s.trim();
    !s.is_empty()
        && s.contains('.')
        && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
}

/// 从异常块里指出**到底是谁崩了**。
///
/// 这是能不能把事件归因到被测应用的关键：收集窗口有几百行，被测 App 在前台时
/// 日志里到处是它的包名，靠「详情里出现过包名」判断必然命中——**别的进程崩了也会
/// 被算到被测应用头上**。所以要显式解析崩溃进程，而不是扫字符串。
///
/// 各来源的写法：
///  - Java 崩溃：`E AndroidRuntime: Process: com.demo, PID: 1234`
///  - events buffer：`am_crash: [pid,userId,<pkg>,...]` / `am_anr: [...,<pkg>,...]`（包名都在第 3 个字段）
///  - ANR：`ANR in com.demo`
///  - Native：`>>> com.demo <<<`
///
/// 解析不出来时返回 `None`（例如裸 `app_process` 起的手工 shell 进程，见 `is_tool_crash`）。
fn attribute_package(detail: &str) -> Option<String> {
    for line in detail.lines() {
        if let Some(i) = line.find("Process:") {
            let rest = &line[i + "Process:".len()..];
            let tok = rest
                .trim_start()
                .split(|c: char| c == ',' || c.is_whitespace())
                .next()
                .unwrap_or("");
            if is_pkg_like(tok) {
                return Some(tok.to_string());
            }
        }
    }

    for line in detail.lines() {
        if line.contains("am_crash") || line.contains("am_anr") {
            if let (Some(lb), Some(rb)) = (line.find('['), line.find(']')) {
                if rb > lb {
                    let fields: Vec<&str> = line[lb + 1..rb].split(',').collect();
                    if let Some(cand) = fields.get(2) {
                        let cand = cand.trim();
                        if is_pkg_like(cand) {
                            return Some(cand.to_string());
                        }
                    }
                }
            }
        }
    }

    for line in detail.lines() {
        if let Some(i) = line.find("ANR in ") {
            let tok = line[i + "ANR in ".len()..]
                .split_whitespace()
                .next()
                .unwrap_or("");
            if is_pkg_like(tok) {
                return Some(tok.to_string());
            }
        }
    }

    if let Some(i) = detail.find(">>> ") {
        let rest = &detail[i + 4..];
        if let Some(j) = rest.find(" <<<") {
            let tok = rest[..j].trim();
            if is_pkg_like(tok) {
                return Some(tok.to_string());
            }
        }
    }

    None
}

/// 判断这个异常块是不是「我们自己的工具」崩的。
///
/// `uiautomator dump` / `am` 这类命令是裸 `app_process`，不是 zygote 拉起的应用，
/// 所以崩溃时**没有 `Process: <pkg>` 行**，走的却是同一条 AndroidRuntime 崩溃路径，
/// 打出来的同样是 `FATAL EXCEPTION`。典型现场：
///
/// ```text
/// E AndroidRuntime: FATAL EXCEPTION: main
/// E AndroidRuntime: PID: 32613
/// E AndroidRuntime: java.lang.IllegalStateException: UiAutomationService ... already registered!
/// E AndroidRuntime:     at com.android.commands.uiautomator.DumpCommand.run(DumpCommand.java:78)
/// ```
///
/// 实测（抖音遍历，2026-09-16）：一份 logcat 里一条 `Process:`、一条 `am_crash` 都没有，
/// 却有 4 次这种 dump 进程崩溃 —— 全部被记成了被测应用的崩溃。
fn is_tool_crash(detail: &str) -> bool {
    // 用「行内包含」而不是「行首匹配」：logcat 的堆栈帧前面还带着
    // `09-16 23:44:17.719 32613 32613 E AndroidRuntime: \tat com...`，
    // 行首是时间戳，不是 `at`。
    const TOOL_FRAMES: &[&str] = &["at com.android.commands.", "at com.android.uiautomator."];
    detail
        .lines()
        .any(|l| TOOL_FRAMES.iter().any(|f| l.contains(f)))
}

/// 正在收集中的异常块
struct Block {
    kind: IncidentKind,
    start_idx: u64,
    deadline: Instant,
    summary: String,
}

/// `end_idx` 为开区间上界：新块抢走当前行时传 `total`，让上一块到上一行为止，
/// 避免两条不同的异常被拼进同一段详情。
fn flush_block(
    b: Block,
    end_idx: u64,
    window: &VecDeque<(u64, String)>,
    package: &str,
    tx: &Sender<Incident>,
    recent: &mut HashMap<String, Instant>,
) {
    let detail: Vec<String> = window
        .iter()
        .filter(|(idx, _)| *idx >= b.start_idx && *idx < end_idx)
        .map(|(_, l)| l.clone())
        .collect();
    let detail = detail.join("\n");
    if detail.trim().is_empty() {
        return;
    }
    // ---- 归因：先剔掉「不是被测应用崩的」----
    // 注意不要退回「详情里出现过包名就算数」：收集窗口几百行，被测 App 在前台时
    // 它的包名到处都有，这个判据等价于「一律算数」。
    if !package.is_empty() {
        // 我们自己的采集工具崩的（uiautomator dump / am 这类裸 app_process）
        if is_tool_crash(&detail) {
            return;
        }
        match attribute_package(&detail) {
            // 能解析出崩溃进程 → 必须就是目标包
            Some(p) => {
                if p != package {
                    return;
                }
            }
            // 解析不出来（没有 `Process:` 行）→ 退回文本启发式
            None => {
                if !detail.contains(package) && !b.summary.contains(package) {
                    return;
                }
            }
        }
    }

    // 同类型 + 同包名 + 同异常特征，且在去重窗口内 → 判定为同一事件走了另一条日志通道
    let fp = incident_fingerprint(&detail).unwrap_or_else(|| "unknown".to_string());
    let key = format!("{:?}|{}|{}", b.kind, package, fp);
    let now = Instant::now();
    if recent
        .get(&key)
        .is_some_and(|t| now.duration_since(*t) < DEDUP_WINDOW)
    {
        return;
    }
    recent.insert(key, now);
    let _ = tx.send(Incident {
        kind: b.kind,
        time: now_str(),
        package: package.to_string(),
        summary: crate::util::truncate(b.summary.trim(), 160),
        detail,
        file: None,
        step: None,
        shot: None,
        shot_thumb: None,
    });
}

impl Drop for LogcatMonitor {
    fn drop(&mut self) {
        self.stop();
    }
}

// ------------------------------------------------------------------ 附加取证

/// 抓取 dropbox 中的崩溃/ANR 记录（best effort）
pub fn collect_dropbox(dev: &dyn Device, kind: &str) -> String {
    let out = dev.sh_quiet(&format!("dumpsys dropbox --print {}", kind));
    if out.trim().is_empty() {
        return String::new();
    }
    // 只保留最近的一部分，避免报告过大
    let lines: Vec<&str> = out.lines().collect();
    let take = lines.len().min(400);
    lines[..take].join("\n")
}

/// 读取设备上的文本文件：优先直接读（设备端模式下最快、也不受 toybox `cat` 权限限制），
/// 失败再退回 `cat`。`/data/anr`、`/data/tombstones` 在非 root 设备上通常不可读，全部 best effort。
pub fn read_text(dev: &dyn Device, path: &str) -> String {
    if let Ok(bytes) = dev.read_file(path) {
        let s = String::from_utf8_lossy(&bytes).to_string();
        if !s.trim().is_empty() {
            return s;
        }
    }
    let out = dev.sh_quiet(&format!("cat {}", path));
    if out.contains("Permission denied") || out.contains("No such file") {
        String::new()
    } else {
        out
    }
}

/// 尝试抓取 ANR traces（多数非 root 设备不可读，best effort）
pub fn collect_anr_traces(dev: &dyn Device) -> String {
    let out = read_text(dev, "/data/anr/traces.txt");
    if !out.trim().is_empty() {
        return out;
    }
    let list = dev.sh_quiet("ls -t /data/anr");
    if !list.trim().is_empty() && !list.contains("Permission denied") {
        if let Some(first) = list.lines().next().map(|s| s.trim().to_string()) {
            if !first.is_empty() {
                let out = read_text(dev, &format!("/data/anr/{}", first));
                if !out.trim().is_empty() {
                    return out;
                }
            }
        }
    }
    String::new()
}

/// 抓取 tombstone（native 崩溃）
pub fn collect_tombstones(dev: &dyn Device) -> String {
    dev.sh_quiet("ls -t /data/tombstones")
        .lines()
        .take(1)
        .map(|f| read_text(dev, &format!("/data/tombstones/{}", f.trim())))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_lines() {
        assert_eq!(
            classify("09-09 23:50:01.123  1234  1234 E AndroidRuntime: FATAL EXCEPTION: main"),
            Some((IncidentKind::Crash, true))
        );
        assert_eq!(
            classify("09-09 23:50:01.123  1234  1234 I am_crash: [1234,0,com.demo,0,java.lang.NullPointerException,Unknown]"),
            Some((IncidentKind::Crash, true))
        );
        assert_eq!(
            classify("09-09 23:50:01.123  1234  1234 E ActivityManager: ANR in com.demo"),
            Some((IncidentKind::Anr, true))
        );
        assert_eq!(
            classify("09-09 23:50:01.123  1234  1234 I am_anr  : [0,1234,com.demo,550026048,Input dispatching timed out]"),
            Some((IncidentKind::Anr, true))
        );
        // 弱触发：同一异常的补充行，不应打断正在收集的块
        assert_eq!(
            classify("09-09 23:50:01.123  1234  1300 E ActivityManager: Reason: Input dispatching timed out"),
            Some((IncidentKind::Anr, false))
        );
        assert_eq!(
            classify("09-09 23:50:01.123  1234  1234 F libc    : FATAL SIGNAL 11"),
            Some((IncidentKind::NativeCrash, true))
        );
        assert_eq!(
            classify("09-09 23:50:01.123  1234  1234 D okhttp: --> GET http"),
            None
        );
    }

    #[test]
    fn detects_log_start() {
        assert!(is_log_start("09-09 23:50:01.123  1234  1234 E tag: x"));
        assert!(!is_log_start("    at com.demo.Main.onCreate(Main.java:12)"));
    }

    fn collect(log: &str, package: &str) -> Vec<Incident> {
        let cursor = std::io::Cursor::new(log.as_bytes().to_vec());
        let (tx, rx) = channel();
        let buf = Arc::new(Mutex::new(None));
        let stop = Arc::new(Mutex::new(false));
        run_loop(cursor, tx, package, buf, stop).unwrap();
        rx.try_iter().collect()
    }

    #[test]
    fn collects_block_and_filters_package() {
        let log = "09-09 23:50:01.100   100   100 D other: noise\n\
                   09-09 23:50:01.123  1234  1234 E AndroidRuntime: FATAL EXCEPTION: main\n\
                   09-09 23:50:01.124  1234  1234 E AndroidRuntime: Process: com.demo, PID: 1234\n\
                   09-09 23:50:01.125  1234  1234 E AndroidRuntime: java.lang.NullPointerException\n\
                   09-09 23:50:03.000   100   100 D other: after\n";

        let got = collect(log, "com.demo");
        assert_eq!(
            got.len(),
            1,
            "应收集到 1 条崩溃: {:?}",
            got.iter().map(|x| x.kind).collect::<Vec<_>>()
        );
        assert_eq!(got[0].kind, IncidentKind::Crash);
        assert!(
            got[0].detail.contains("NullPointerException"),
            "堆栈未收集全"
        );
        assert!(got[0].detail.contains("Process: com.demo"));

        // 包名不匹配 → 丢弃
        assert!(collect(log, "com.other").is_empty());
    }

    /// 崩溃后紧跟着 ANR：两条都要抓到，不能被合并成一条
    #[test]
    fn separates_consecutive_crash_and_anr() {
        let log = "09-09 23:50:01.100  1234  1234 E AndroidRuntime: FATAL EXCEPTION: main\n\
                   09-09 23:50:01.101  1234  1234 E AndroidRuntime: Process: com.demo, PID: 1234\n\
                   09-09 23:50:01.102  1234  1234 E AndroidRuntime: \tat com.demo.Main.onClick(Main.java:42)\n\
                   09-09 23:50:03.300  1234  1300 E ActivityManager: ANR in com.demo\n\
                   09-09 23:50:03.301  1234  1300 E ActivityManager: PID: 1234\n\
                   09-09 23:50:03.302  1234  1300 E ActivityManager: Reason: Input dispatching timed out\n";

        let got = collect(log, "com.demo");
        let kinds: Vec<IncidentKind> = got.iter().map(|x| x.kind).collect();
        assert_eq!(
            kinds,
            vec![IncidentKind::Crash, IncidentKind::Anr],
            "异常被错误合并: {:?}",
            kinds
        );
        assert!(got[0].detail.contains("Main.onClick"), "崩溃堆栈不完整");
        assert!(!got[0].detail.contains("ANR in"), "ANR 内容混进了崩溃块");
        assert!(
            got[1].detail.contains("Input dispatching timed out"),
            "ANR 详情不完整"
        );
    }

    /// 同一次崩溃会同时出现在两条日志通道：AndroidRuntime 的 FATAL EXCEPTION
    /// 和 ActivityManager 的 am_crash。去重后只应记一条，否则 crash 计数凭空翻倍。
    #[test]
    fn deduplicates_crash_across_log_channels() {
        let log = "09-09 23:50:01.100  1234  1234 E AndroidRuntime: FATAL EXCEPTION: main\n\
                   09-09 23:50:01.101  1234  1234 E AndroidRuntime: Process: com.demo, PID: 1234\n\
                   09-09 23:50:01.102  1234  1234 E AndroidRuntime: \tat com.demo.Main.onClick(Main.java:42)\n\
                   09-09 23:50:01.102  1234  1234 E AndroidRuntime: Caused by: java.lang.NullPointerException\n\
                   09-09 23:50:01.180  1234  1234 I am_crash: [1234,0,com.demo,0,java.lang.NullPointerException,Unknown]\n";

        let got = collect(log, "com.demo");
        assert_eq!(
            got.len(),
            1,
            "同一次崩溃被记了两次: {:?}",
            got.iter().map(|i| i.summary.clone()).collect::<Vec<_>>()
        );
        assert!(
            got[0].detail.contains("Main.onClick"),
            "应保留信息更全的那条（堆栈）"
        );
    }

    /// 反向约束：5 秒内发生的两个「不同」崩溃不能被误合并
    #[test]
    fn keeps_distinct_crashes_within_dedup_window() {
        let log = "09-09 23:50:01.100  1234  1234 E AndroidRuntime: FATAL EXCEPTION: main\n\
                   09-09 23:50:01.101  1234  1234 E AndroidRuntime: Process: com.demo, PID: 1234\n\
                   09-09 23:50:01.102  1234  1234 E AndroidRuntime: java.lang.NullPointerException\n\
                   09-09 23:50:02.900  1234  1234 E AndroidRuntime: FATAL EXCEPTION: main\n\
                   09-09 23:50:02.901  1234  1234 E AndroidRuntime: Process: com.demo, PID: 1234\n\
                   09-09 23:50:02.902  1234  1234 E AndroidRuntime: java.lang.IllegalStateException\n";

        let got = collect(log, "com.demo");
        assert_eq!(
            got.len(),
            2,
            "不同异常不应被合并: {:?}",
            got.iter().map(|i| i.summary.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn extracts_incident_fingerprint() {
        assert_eq!(
            incident_fingerprint("E AndroidRuntime: java.lang.NullPointerException"),
            Some("java.lang.NullPointerException".to_string())
        );
        assert_eq!(
            incident_fingerprint("F libc    : FATAL SIGNAL 11 (SIGSEGV)"),
            Some("signal-11".to_string())
        );
        assert_eq!(
            incident_fingerprint(
                "I am_anr  : [0,1234,com.demo,550026048,Input dispatching timed out]"
            ),
            None
        );
    }

    #[test]
    fn parses_crashing_package() {
        assert_eq!(
            attribute_package("E AndroidRuntime: Process: com.demo, PID: 1234"),
            Some("com.demo".to_string())
        );
        // events buffer 第 3 个字段是包名
        assert_eq!(
            attribute_package(
                "I am_crash: [1234,0,com.demo,0,java.lang.NullPointerException,Unknown]"
            ),
            Some("com.demo".to_string())
        );
        assert_eq!(
            attribute_package("E ActivityManager: ANR in com.demo (com.demo/.Main)"),
            Some("com.demo".to_string())
        );
        assert_eq!(
            attribute_package("F DEBUG: >>> com.demo <<<"),
            Some("com.demo".to_string())
        );
        // 裸 app_process（uiautomator dump）没有 Process: 行 → 归因不出来
        assert_eq!(attribute_package("E AndroidRuntime: PID: 32613"), None);
    }

    /// 实测回归（抖音遍历 2026-09-16）：`uiautomator dump` 进程因
    /// 「UiAutomationService already registered!」崩溃，被测 App 正在前台。
    /// 崩溃块后面紧跟的 300 行窗口里落进了抖音自己 `E/ViewRootImpl` 的
    /// 无障碍告警栈（含 `com.ss.android.ugc.aweme.*` 帧），旧的
    /// 「详情含包名即认定」于是把它记成了**抖音崩溃**。
    ///
    /// 真实数据：整份 logcat 里 0 条 `Process:`、0 条 `am_crash`，
    /// 却有 4 次 dump 进程崩溃，全部被误报。
    #[test]
    fn ignores_uiautomator_dump_crash() {
        let log = "09-16 23:44:17.719 32613 32613 E AndroidRuntime: FATAL EXCEPTION: main\n\
                   09-16 23:44:17.719 32613 32613 E AndroidRuntime: PID: 32613\n\
                   09-16 23:44:17.719 32613 32613 E AndroidRuntime: java.lang.IllegalStateException: UiAutomationService android.accessibilityservice.IAccessibilityServiceClient$Stub$Proxy@d827670already registered!\n\
                   09-16 23:44:17.719 32613 32613 E AndroidRuntime: \tat com.android.uiautomator.core.UiAutomationShellWrapper.connect(UiAutomationShellWrapper.java:36)\n\
                   09-16 23:44:17.719 32613 32613 E AndroidRuntime: \tat com.android.commands.uiautomator.DumpCommand.run(DumpCommand.java:78)\n\
                   09-16 23:44:18.943 32132 32132 E ViewRootImpl: \tat com.ss.android.ugc.aweme.platform.collect.base.CollectClient.bind(SourceFile:84345063)\n\
                   09-16 23:44:18.943 32132 32132 E ViewRootImpl: \tat com.ss.android.ugc.aweme.collection.FeedCollectPresenterV2.LJIILJJIL(SourceFile:34013286)\n";

        let got = collect(log, "com.ss.android.ugc.aweme");
        assert!(
            got.is_empty(),
            "dump 工具自己的崩溃被记成了被测应用崩溃: {:?}",
            got.iter().map(|i| i.summary.clone()).collect::<Vec<_>>()
        );
    }

    /// 反向约束：窗口里误入的第三方 `ViewRootImpl` 告警栈不影响真正的应用崩溃被识别。
    #[test]
    fn still_reports_real_app_crash_among_noise() {
        let log = "09-16 23:44:18.943 32132 32132 E ViewRootImpl: \tat com.other.app.Thing.run(SourceFile:1)\n\
                   09-16 23:44:19.100  1234  1234 E AndroidRuntime: FATAL EXCEPTION: main\n\
                   09-16 23:44:19.101  1234  1234 E AndroidRuntime: Process: com.ss.android.ugc.aweme, PID: 1234\n\
                   09-16 23:44:19.102  1234  1234 E AndroidRuntime: java.lang.NullPointerException\n\
                   09-16 23:44:19.103  1234  1234 E AndroidRuntime: \tat com.ss.android.ugc.aweme.Main.onClick(Main.java:42)\n";

        let got = collect(log, "com.ss.android.ugc.aweme");
        assert_eq!(got.len(), 1, "真崩溃漏报了: {got:?}");
        assert!(got[0].detail.contains("Main.onClick"));
    }

    /// 别的应用崩了（有明确 `Process:` 行）不能算到目标应用头上。
    #[test]
    fn does_not_attribute_other_app_crash() {
        let log = "09-16 23:44:19.100  1234  1234 E AndroidRuntime: FATAL EXCEPTION: main\n\
                   09-16 23:44:19.101  1234  1234 E AndroidRuntime: Process: com.other.app, PID: 1234\n\
                   09-16 23:44:19.102  1234  1234 E AndroidRuntime: java.lang.NullPointerException\n\
                   09-16 23:44:19.103  1234  1234 E ActivityManager: Displayed com.ss.android.ugc.aweme/.Main\n";

        let got = collect(log, "com.ss.android.ugc.aweme");
        assert!(got.is_empty(), "别的应用崩溃被算到了目标应用: {got:?}");
    }
}
