//! 遍历主循环：观察 → 决策 → 执行 → 截图标注 → 记录 → 异常检测。
//!
//! 与运行位置无关：跑在手机上时用 LocalDevice，跑在 PC 上时用 AdbDevice。

use crate::annotate::{annotate_png, markers_for_action, write_thumbnail, Marker};
use crate::device::{self, Device, DeviceInfo};
use crate::dump::parse_hierarchy;
use crate::model::{Action, Hierarchy};
use crate::monitor::{
    collect_anr_traces, collect_dropbox, collect_tombstones, Incident, IncidentKind, LogcatMonitor,
};
use crate::session::{append_step, write_session, RunConfig, Session, StepRecord};
use crate::strategy::{Explorer, ExplorerConfig, StateMode};
use crate::util::{elapsed_ms, ensure_dir, now_file_str, now_str, slug};
use anyhow::{bail, Context, Result};
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct RunOptions {
    pub package: String,
    pub activity: Option<String>,
    /// 输出目录（手机端为设备路径，PC 端为本地路径）
    pub output: PathBuf,
    pub duration_secs: u64,
    pub max_steps: usize,
    pub interval_ms: u64,
    pub settle_ms: u64,
    pub seed: u64,
    pub screenshot: bool,
    pub annotate: bool,
    pub keep_raw: bool,
    pub text_pool: Vec<String>,
    pub max_same_state: usize,
    /// 状态指纹取法（结构优先 / 内容精确）
    pub state_mode: StateMode,
    pub max_relaunch: usize,
    pub verbose: bool,
    pub stop: Arc<AtomicBool>,
}

pub struct RunOutput {
    pub session: Session,
    pub report: PathBuf,
}

pub fn run(dev: &mut dyn Device, opts: RunOptions) -> Result<RunOutput> {
    let pkg = opts.package.clone();
    let info = DeviceInfo::fetch(dev, "")?;
    println!("[设备] {}", info.display());
    println!(
        "[模式] {}",
        if dev.kind() == "device" {
            "设备端（运行在手机上）"
        } else {
            "PC 端（adb 转发）"
        }
    );

    // ---------------- 目录准备
    let dir = opts.output.clone();
    let shot_dir = dir.join("screenshots");
    let thumb_dir = dir.join("thumbs");
    let raw_dir = dir.join("raw");
    let inc_dir = dir.join("incidents");
    let report_dir = dir.join("report");
    for d in [&dir, &shot_dir, &thumb_dir, &inc_dir, &report_dir] {
        ensure_dir(d)?;
    }
    if opts.keep_raw {
        ensure_dir(&raw_dir)?;
    }
    let steps_path = dir.join("steps.jsonl");
    let logcat_path = dir.join("logcat.txt");

    // ---------------- 日志监控
    let _ = device::logcat_clear(dev);
    let monitor = LogcatMonitor::start(dev, &pkg, &logcat_path).context("启动 logcat 监控失败")?;

    // ---------------- 启动应用
    let _ = device::force_stop(dev, &pkg);
    std::thread::sleep(Duration::from_millis(800));
    let component = device::launch(dev, &pkg, opts.activity.as_deref())?;
    println!("[启动] {}", component);
    std::thread::sleep(Duration::from_millis(2500));

    let cfg = ExplorerConfig {
        package: pkg.clone(),
        text_pool: if opts.text_pool.is_empty() {
            crate::strategy::default_text_pool()
        } else {
            opts.text_pool.clone()
        },
        max_same_state: opts.max_same_state.max(2),
        state_mode: opts.state_mode,
        ..Default::default()
    };
    let mut ex = Explorer::new(cfg, opts.seed);

    let start = Instant::now();
    let started_at = now_str();
    let mut steps: Vec<StepRecord> = Vec::new();
    let mut incidents: Vec<Incident> = Vec::new();
    let mut step_index: usize = 0;
    let mut relaunches: usize = 0;
    let mut back_streak: usize = 0;
    let mut prev_step: Option<usize> = None;
    let mut pending_transition: Option<(u64, String, usize)> = None;
    let mut away_count: usize = 0;
    let mut dump_fail_count: usize = 0;
    // 连续「取不到 Activity 且 dump 失败」的次数，用于识别设备掉线
    let mut dump_fail_streak: usize = 0;
    let mut incident_seq: usize = 0;

    println!(
        "[开始遍历] 目标 {} · 最长 {}s · 最多 {} 步",
        pkg, opts.duration_secs, opts.max_steps
    );

    while !opts.stop.load(Ordering::SeqCst) {
        // 支持从外部放一个 STOP 文件来停止（手机端没有 Ctrl-C 时很方便）
        if dir.join("STOP").exists() {
            println!("[结束] 检测到 STOP 文件");
            break;
        }
        if opts.duration_secs > 0 && start.elapsed().as_secs() >= opts.duration_secs {
            println!("[结束] 到达时长上限");
            break;
        }
        if opts.max_steps > 0 && step_index >= opts.max_steps {
            println!("[结束] 到达步数上限");
            break;
        }

        // ---- 当前 Activity
        let activity = device::current_activity(dev).unwrap_or_default();
        if let Some(i) = prev_step {
            if let Some(s) = steps.get_mut(i) {
                s.activity_after = activity.clone();
            }
        }

        // ---- 不在被测应用内：处理弹窗 / 返回 / 重启
        if activity.is_empty() || !activity.starts_with(&pkg) {
            away_count += 1;
            let handled = match dev.dump_xml() {
                Ok(xml) => {
                    dump_fail_streak = 0;
                    match parse_hierarchy(&xml) {
                        Ok(h) => try_dismiss_dialog(dev, &h).unwrap_or(false),
                        Err(_) => false,
                    }
                }
                Err(_) => {
                    dump_fail_streak += 1;
                    false
                }
            };
            // 既拿不到 Activity 又一直 dump 不出来，多半不是界面问题而是设备/连接断了。
            // 此时继续 press_back + 重启毫无意义，直接给出明确诊断。
            if dump_fail_streak >= 3 {
                bail!(
                    "连续 {} 次既取不到前台 Activity，又无法 dump 控件树：设备可能已断开连接 \
                     （USB 松动 / 授权失效 / adb 掉线）。请检查 adb devices 后重跑。",
                    dump_fail_streak
                );
            }
            if !handled {
                let _ = device::press_back(dev);
            }
            std::thread::sleep(Duration::from_millis(700));
            if away_count >= 4 {
                away_count = 0;
                if relaunches >= opts.max_relaunch {
                    println!(
                        "[结束] 重启次数达到上限（{}），应用可能无法保持在前台",
                        opts.max_relaunch
                    );
                    break;
                }
                relaunches += 1;
                println!("[重启] 应用已离开前台，第 {} 次重启", relaunches);
                let _ = device::force_stop(dev, &pkg);
                std::thread::sleep(Duration::from_millis(500));
                let _ = device::launch(dev, &pkg, opts.activity.as_deref());
                std::thread::sleep(Duration::from_millis(2500));
            }
            continue;
        }
        away_count = 0;

        // ---- 取控件树
        let xml = match dev.dump_xml() {
            Ok(x) => x,
            Err(e) => {
                dump_fail_count += 1;
                eprintln!("[警告] dump 失败({}/5): {}", dump_fail_count, e);
                if dump_fail_count >= 5 {
                    bail!(
                        "连续 5 次无法 dump 控件树（最后一次：{}）。设备可能已断开或 uiautomator 不可用，\
                         请检查 adb devices 与设备是否处于解锁状态。",
                        e
                    );
                }
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        };
        dump_fail_count = 0;
        dump_fail_streak = 0;
        let hier: Hierarchy = match parse_hierarchy(&xml) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("[警告] 解析控件树失败: {}", e);
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        };

        // ---- 状态登记
        let (state_id, is_new) = ex.observe(&hier, &activity, step_index);
        if let Some(i) = prev_step {
            if let Some(s) = steps.get_mut(i) {
                s.state_after = state_id;
                s.new_state = is_new;
            }
        }
        // 上一步动作引发的状态迁移（from -> to）
        if let Some((from, key, st)) = pending_transition.take() {
            ex.note_transition(from, key, state_id, st);
        }

        // ---- 决策
        let action = match ex.choose(state_id, &hier) {
            Some(a) => {
                back_streak = 0;
                a
            }
            None => {
                back_streak += 1;
                Action::Back
            }
        };
        let action = if ex.is_stuck() && !matches!(action, Action::Back) {
            if opts.verbose {
                println!(
                    "[策略] 连续 {} 次停留在同一状态，强制返回",
                    ex.same_streak()
                );
            }
            ex.reset_streak();
            Action::Back
        } else {
            action
        };
        if back_streak >= 8 {
            back_streak = 0;
            relaunches += 1;
            if relaunches > opts.max_relaunch {
                println!("[结束] 重启次数达到上限");
                break;
            }
            println!("[重启] 动作已穷尽，第 {} 次重启应用", relaunches);
            let _ = device::launch(dev, &pkg, opts.activity.as_deref());
            std::thread::sleep(Duration::from_millis(2500));
            continue;
        }

        // ---- 执行
        step_index += 1;
        let t0 = Instant::now();
        let mut rec = StepRecord::from_action(
            step_index,
            &action,
            &activity,
            state_id,
            start.elapsed().as_millis() as u64,
        );
        let exec = execute_action(dev, &action);
        rec.duration_ms = elapsed_ms(t0);
        if let Err(e) = &exec {
            rec.error = Some(format!("{}", e));
        }
        // 立刻计入覆盖率：被操作过的控件不再享有"未探索"加权
        if let Some(k) = action.target_key() {
            ex.mark_touched(&k);
        }
        pending_transition = Some((state_id, action.action_key(), step_index));

        // ---- 截图 + 标注
        if opts.screenshot {
            std::thread::sleep(Duration::from_millis(opts.settle_ms));
            match dev.screencap() {
                Ok(png) => {
                    if opts.keep_raw {
                        let _ = std::fs::write(
                            raw_dir.join(format!("step_{:04}_{}.png", step_index, action.kind())),
                            &png,
                        );
                    }
                    let markers =
                        scale_markers(markers_for_action(&action, step_index), &hier, &png);
                    // 标注关闭时直接写原始 PNG，不再整份拷贝一次
                    let data: Option<Cow<'_, [u8]>> = if opts.annotate {
                        match annotate_png(&png, &markers) {
                            Ok(out) => Some(Cow::Owned(out)),
                            Err(e) => {
                                if opts.verbose {
                                    eprintln!("[警告] 截图标注失败: {}", e);
                                }
                                None
                            }
                        }
                    } else {
                        Some(Cow::Borrowed(png.as_slice()))
                    };
                    if let Some(data) = data {
                        let name = format!("step_{:04}_{}.png", step_index, action.kind());
                        let p = shot_dir.join(&name);
                        if std::fs::write(&p, &data).is_ok() {
                            rec.screenshot = Some(format!("screenshots/{}", name));
                            let tp = thumb_dir.join(&name);
                            if write_thumbnail(&data, &tp, 260, 460).is_ok() {
                                rec.thumb = Some(format!("thumbs/{}", name));
                            }
                        }
                    }
                }
                Err(e) => {
                    if opts.verbose {
                        eprintln!("[警告] 截图失败: {}", e);
                    }
                }
            }
        }

        let _ = append_step(&steps_path, &rec);
        steps.push(rec);
        prev_step = Some(steps.len() - 1);

        // ---- 异常
        for mut inc in monitor.poll() {
            incident_seq += 1;
            inc.step = Some(step_index);
            let (fname, shot) =
                write_incident_report(dev, &dir, &inc, incident_seq, opts.screenshot);
            inc.file = Some(fname);
            if let Some((big, thumb)) = shot {
                inc.shot = Some(big);
                if !thumb.is_empty() {
                    inc.shot_thumb = Some(thumb);
                }
            }
            println!(
                "[异常] {} · 步骤 #{} · {} → incidents/",
                inc.kind.cn(),
                step_index,
                crate::util::truncate(&inc.summary, 90)
            );
            incidents.push(inc);

            // 崩溃后应用通常已退出，稍后重启
            std::thread::sleep(Duration::from_millis(1200));
            if !device::is_running(dev, &pkg) && relaunches < opts.max_relaunch {
                relaunches += 1;
                println!("[重启] 应用进程已退出，第 {} 次重启", relaunches);
                let _ = device::launch(dev, &pkg, opts.activity.as_deref());
                std::thread::sleep(Duration::from_millis(2000));
            }
        }

        // ---- 进度
        if opts.verbose || step_index.is_multiple_of(10) {
            let cov = ex.coverage();
            println!(
                "[#{}] {} | {} | 状态 {} · Activity {} · 控件 {}/{} ({:.1}%) | 异常 {}",
                step_index,
                action.kind_cn(),
                crate::util::truncate(
                    steps.last().map(|r| r.action_label.as_str()).unwrap_or(""),
                    46
                ),
                ex.state_count(),
                cov.activity_count,
                cov.interactive_touched,
                cov.interactive_nodes,
                cov.interactive_coverage * 100.0,
                incidents.len()
            );
        }

        std::thread::sleep(Duration::from_millis(opts.interval_ms));
    }

    // ---------------- 收尾：再取一次异常，避免最后 2.5s 窗口内的丢失
    monitor.stop();
    for mut inc in monitor.poll() {
        incident_seq += 1;
        inc.step = Some(step_index);
        let (fname, shot) = write_incident_report(dev, &dir, &inc, incident_seq, opts.screenshot);
        inc.file = Some(fname);
        if let Some((big, thumb)) = shot {
            inc.shot = Some(big);
            if !thumb.is_empty() {
                inc.shot_thumb = Some(thumb);
            }
        }
        incidents.push(inc);
    }

    let coverage = ex.coverage();
    let session = Session {
        package: pkg.clone(),
        activity: component.clone(),
        device: info,
        config: RunConfig {
            package: pkg.clone(),
            activity: opts.activity.clone(),
            duration_secs: opts.duration_secs,
            max_steps: opts.max_steps,
            interval_ms: opts.interval_ms,
            seed: opts.seed,
            screenshot: opts.screenshot,
            annotate: opts.annotate,
            state_mode: opts.state_mode.as_str().to_string(),
        },
        started_at,
        finished_at: now_str(),
        duration_ms: start.elapsed().as_millis() as u64,
        steps,
        incidents,
        coverage,
        transitions: ex.take_transitions(),
        state_count: ex.state_count(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };
    write_session(&dir, &session)?;
    let report = crate::report::render(&session, &report_dir)?;

    Ok(RunOutput { session, report })
}

/// 异常落盘：incidents/*.txt（logcat 块 + 有限次数的 dropbox / traces / tombstone 取证）
fn write_incident_report(
    dev: &dyn Device,
    dir: &Path,
    inc: &Incident,
    seq: usize,
    want_shot: bool,
) -> (String, Option<(String, String)>) {
    // 现场截图先抓：崩溃之后界面很快就变了（弹窗消失、应用退出、被系统回收），
    // 这是「趁还看得见」的取证，所以不标注 —— 它不是某一步的操作，
    // 而是异常发生那一刻屏幕上是什么样。
    let shot = if want_shot && seq <= MAX_INCIDENT_SHOTS {
        capture_incident_shot(dev, dir, inc.kind.as_str(), seq)
    } else {
        None
    };

    let fname = format!(
        "incidents/{}_{:02}_{}.txt",
        inc.kind.as_str(),
        seq,
        now_file_str()
    );
    let mut body = String::new();
    body.push_str(&format!("类型: {}\n", inc.kind.cn()));
    body.push_str(&format!("时间: {}\n", inc.time));
    body.push_str(&format!("包名: {}\n", inc.package));
    if let Some(st) = inc.step {
        body.push_str(&format!("步骤: #{}\n", st));
    }
    body.push_str(&format!("摘要: {}\n\n", inc.summary));
    body.push_str("---- logcat ----\n");
    body.push_str(&inc.detail);
    body.push('\n');

    // 附加取证（限制次数，避免拖慢遍历）
    if seq <= 3 {
        match inc.kind {
            IncidentKind::Anr => {
                let t = collect_anr_traces(dev);
                if !t.trim().is_empty() {
                    body.push_str("\n---- /data/anr/traces.txt ----\n");
                    body.push_str(&t);
                }
                let d = collect_dropbox(dev, "data_app_anr");
                if !d.trim().is_empty() {
                    body.push_str("\n---- dropbox data_app_anr ----\n");
                    body.push_str(&d);
                }
            }
            IncidentKind::Crash => {
                let d = collect_dropbox(dev, "data_app_crash");
                if !d.trim().is_empty() {
                    body.push_str("\n---- dropbox data_app_crash ----\n");
                    body.push_str(&d);
                }
            }
            IncidentKind::NativeCrash => {
                let d = collect_dropbox(dev, "system_app_native_crash");
                if !d.trim().is_empty() {
                    body.push_str("\n---- dropbox native ----\n");
                    body.push_str(&d);
                }
                let t = collect_tombstones(dev);
                if !t.trim().is_empty() {
                    body.push_str("\n---- tombstone ----\n");
                    body.push_str(&t);
                }
            }
        }
    }
    if let Some((big, _)) = &shot {
        body.push_str(&format!(
            "
---- 现场截图 ----
{}
",
            big
        ));
    }
    let _ = std::fs::write(dir.join(&fname), &body);
    (fname, shot)
}

/// 异常现场截图：抓一张原始画面 + 一张缩略图，返回 (大图, 缩略图) 相对路径
fn capture_incident_shot(
    dev: &dyn Device,
    dir: &Path,
    kind: &str,
    seq: usize,
) -> Option<(String, String)> {
    let png = dev.screencap().ok()?;
    if png.len() < 8 {
        return None;
    }
    let big = format!("incidents/{}_{:02}.png", kind, seq);
    std::fs::write(dir.join(&big), &png).ok()?;
    let thumb = format!("thumbs/{}_{:02}.png", kind, seq);
    let ok = write_thumbnail(&png, &dir.join(&thumb), 260, 460).is_ok();
    Some((big, if ok { thumb } else { String::new() }))
}

/// 最多为多少个异常抓现场截图（异常风暴时避免反复 screencap 拖慢遍历）
const MAX_INCIDENT_SHOTS: usize = 20;

/// 执行一个动作
pub fn execute_action(dev: &dyn Device, action: &Action) -> Result<()> {
    match action {
        Action::Click { point, .. } => device::input_tap(dev, point.0, point.1),
        Action::LongClick { point, .. } => device::input_long_press(dev, point.0, point.1, 900),
        Action::Swipe {
            from,
            to,
            duration_ms,
            ..
        } => device::input_swipe(dev, from.0, from.1, to.0, to.1, *duration_ms),
        Action::Input { point, text, .. } => {
            device::input_tap(dev, point.0, point.1)?;
            std::thread::sleep(Duration::from_millis(250));
            device::input_text(dev, text)
        }
        Action::Back => device::press_back(dev),
        Action::Key { code, .. } => device::input_keyevent(dev, *code),
        Action::Launch { component } => {
            let (p, a) = component
                .split_once('/')
                .unwrap_or((component.as_str(), ""));
            device::launch(dev, p, Some(a)).map(|_| ())
        }
    }
}

/// 截图分辨率与控件树坐标不一致时，按比例缩放标注点
fn scale_markers(mut markers: Vec<Marker>, hier: &Hierarchy, png: &[u8]) -> Vec<Marker> {
    let (sw, sh) = match crate::annotate::png_size(png) {
        Some(v) => (v.0 as f32, v.1 as f32),
        None => return markers,
    };
    if hier.width <= 0 || hier.height <= 0 || sw <= 0.0 || sh <= 0.0 {
        return markers;
    }
    let fx = sw / hier.width as f32;
    let fy = sh / hier.height as f32;
    if (fx - 1.0).abs() < 0.02 && (fy - 1.0).abs() < 0.02 {
        return markers;
    }
    for m in markers.iter_mut() {
        m.points = m
            .points
            .iter()
            .map(|(x, y)| ((*x as f32 * fx) as i32, (*y as f32 * fy) as i32))
            .collect();
        if let Some(b) = &m.bounds {
            m.bounds = Some(crate::model::Rect::new(
                (b.x1 as f32 * fx) as i32,
                (b.y1 as f32 * fy) as i32,
                (b.x2 as f32 * fx) as i32,
                (b.y2 as f32 * fy) as i32,
            ));
        }
    }
    markers
}

/// 尝试关闭运行时权限弹窗 / 崩溃对话框，返回是否处理
fn try_dismiss_dialog(dev: &dyn Device, h: &Hierarchy) -> Result<bool> {
    let allow_keys = [
        "permission_allow_button",
        "permission_allow_foreground_only_button",
        "permission_allow_one_time_button",
        "com.android.packageinstaller:id/ok_button",
        "android:id/button1",
    ];
    let allow_texts = [
        "允许",
        "始终允许",
        "仅在使用该应用时允许",
        "确定",
        "ALLOW",
        "Allow",
        "OK",
        "关闭应用",
    ];

    for n in h.visible_nodes() {
        let is_allow_id = allow_keys.iter().any(|k| n.resource_id.contains(k));
        let is_allow_text = allow_texts.iter().any(|t| n.text.trim() == *t);
        if (is_allow_id || is_allow_text) && (n.clickable || n.is_leaf_with_content()) {
            if let Some(p) = n.click_point(&h.screen()) {
                let _ = device::input_tap(dev, p.0, p.1);
                std::thread::sleep(Duration::from_millis(600));
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// 会话目录默认名：<包名的安全形式>_<时间戳>
pub fn default_session_name(package: &str) -> String {
    format!("{}_{}", slug(package), now_file_str())
}

/// 手机端输出根目录选择：优先 /sdcard（方便用户直接查看），不可写则退回 /data/local/tmp
pub fn pick_device_output(name: &str) -> String {
    for base in ["/sdcard/atraverse", "/data/local/tmp/atraverse"] {
        let dir = format!("{}/{}", base, name);
        if std::fs::create_dir_all(&dir).is_ok() {
            // 写个探针文件确认真的可写
            if std::fs::write(format!("{}/.probe", dir), b"1").is_ok() {
                let _ = std::fs::remove_file(format!("{}/.probe", dir));
                return dir;
            }
        }
    }
    format!("/data/local/tmp/atraverse/{}", name)
}

/// 本地（PC）输出目录
pub fn default_output_dir(package: &str) -> PathBuf {
    Path::new("sessions").join(default_session_name(package))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::mock::MockDevice;
    use std::sync::atomic::AtomicBool;

    fn opts(dir: &Path, max_steps: usize) -> RunOptions {
        RunOptions {
            package: "com.demo".to_string(),
            activity: None,
            output: dir.to_path_buf(),
            duration_secs: 60,
            max_steps,
            interval_ms: 0,
            settle_ms: 0,
            seed: 7,
            screenshot: true,
            annotate: true,
            keep_raw: false,
            text_pool: vec![],
            max_same_state: 6,
            state_mode: StateMode::Structural,
            max_relaunch: 3,
            verbose: false,
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn end_to_end_against_mock_device() {
        let base = std::env::temp_dir().join(format!("atraverse_it_{}", std::process::id()));
        let dir = base.join("s1");
        let _ = std::fs::remove_dir_all(&base);
        let mut dev = MockDevice::new(&base);

        let out = run(&mut dev, opts(&dir, 15)).unwrap();
        let s = &out.session;

        assert!(s.steps.len() >= 5, "steps={}", s.steps.len());
        assert!(dir.join("session.json").exists(), "session.json 缺失");
        assert!(dir.join("steps.jsonl").exists(), "steps.jsonl 缺失");
        assert!(dir.join("report/index.html").exists(), "报告缺失");
        assert!(dir.join("logcat.txt").exists(), "logcat.txt 缺失");

        // 覆盖多种动作类型：至少要有点击，滑动也应该出现（列表可滚动）
        let kinds: std::collections::HashSet<&str> =
            s.steps.iter().map(|x| x.action_kind.as_str()).collect();
        assert!(kinds.contains("click"), "未产生点击: {:?}", kinds);
        assert!(kinds.contains("swipe"), "未产生滑动: {:?}", kinds);

        // 截图文件存在，且与原始截图不同（说明标注画上去了）
        let first_shot = s
            .steps
            .iter()
            .find_map(|x| x.screenshot.clone())
            .expect("没有截图记录");
        let p = dir.join(&first_shot);
        assert!(p.exists(), "截图缺失: {}", p.display());
        let annotated = std::fs::read(&p).unwrap();
        let raw = dev.png.clone();
        assert!(!annotated.is_empty() && annotated != raw, "截图未被标注");

        // 报告内容包含包名与步骤
        let html = std::fs::read_to_string(dir.join("report/index.html")).unwrap();
        assert!(html.contains("com.demo"));
        assert!(html.contains("操作步骤时间线"));
        // 缩略图也生成了
        assert!(s.steps.iter().any(|x| x.thumb.is_some()), "未生成缩略图");

        // 至少进入过一个 Activity 并统计到控件
        assert!(s.coverage.activity_count >= 1, "未统计到 Activity");
        assert!(s.coverage.total_nodes > 0, "未统计到控件");
    }

    #[test]
    fn captures_crash_from_logcat() {
        let base = std::env::temp_dir().join(format!("atraverse_crash_{}", std::process::id()));
        let dir = base.join("s1");
        let _ = std::fs::remove_dir_all(&base);
        let log = vec![
            "09-09 23:50:01.100   100   100 D other: noise".to_string(),
            "09-09 23:50:01.123  1234  1234 E AndroidRuntime: FATAL EXCEPTION: main".to_string(),
            "09-09 23:50:01.124  1234  1234 E AndroidRuntime: Process: com.demo, PID: 1234".to_string(),
            "09-09 23:50:01.125  1234  1234 E AndroidRuntime: java.lang.NullPointerException".to_string(),
            "09-09 23:50:01.126  1234  1234 E AndroidRuntime: at com.demo.Main.onCreate(Main.java:12)".to_string(),
        ];
        let mut dev = MockDevice::with_log(log, &base);

        let out = run(&mut dev, opts(&dir, 4)).unwrap();
        assert!(!out.session.incidents.is_empty(), "未捕获到异常");
        let inc = &out.session.incidents[0];
        assert!(inc.summary.contains("FATAL EXCEPTION"), "{:?}", inc.summary);
        assert!(inc.detail.contains("com.demo"), "异常块中应包含包名");
        assert!(inc.file.is_some(), "异常日志未落盘");
        assert!(
            dir.join(inc.file.as_ref().unwrap()).exists(),
            "异常文件不存在"
        );

        // 现场截图：崩溃后界面很快会变，这是趁还看得见时的取证
        let shot = inc.shot.as_ref().expect("未抓取异常现场截图");
        assert!(dir.join(shot).exists(), "现场截图文件不存在: {shot}");
        assert!(
            inc.shot_thumb.is_some(),
            "现场截图缩略图未生成（报告里会加载全尺寸大图）"
        );

        // 报告里应出现崩溃面板
        let html = std::fs::read_to_string(dir.join("report/index.html")).unwrap();
        assert!(html.contains("Java 崩溃"), "报告未展示崩溃");
        assert!(html.contains("inc-shot"), "报告未展示异常现场截图");
    }
}
