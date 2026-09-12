//! atraverse —— Android 应用智能遍历工具（Fastbot 形态：主体跑在手机上）
//!
//! 两种运行方式：
//!   1) `atraverse agent ...`  二进制 push 到手机后直接在设备上跑（推荐，无 USB 往返开销）
//!   2) `atraverse run ...`    PC 端编排：交叉编译 → push → 远端执行 → 拉回结果 → 生成报告
//!      （加 `--host` 则退化为 PC 端通过 adb 驱动，便于调试）
//!
//! 能力：
//!   · 高覆盖遍历（状态去重 + 未探索动作优先 + 回溯重启）
//!   · 每步记录操作并截图，在截图上标注 click / long_click / swipe / input / back
//!   · 实时监控 logcat，捕获 crash / ANR，抓取 dropbox 与 traces.txt
//!   · 产出 HTML 报告 + session.json + steps.jsonl

mod annotate;
mod device;
mod dump;
mod model;
mod monitor;
mod report;
mod runner;
mod session;
mod strategy;
mod util;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use device::Device;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const REMOTE_BIN: &str = "/data/local/tmp/atraverse";

#[derive(Parser, Debug)]
#[command(
    name = "atraverse",
    version,
    about = "Android 应用智能遍历工具（Fastbot 形态）"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// 【手机端】在设备上直接执行遍历（二进制运行在手机上）
    Agent(AgentArgs),

    /// 【PC 端】部署到手机并执行，结束后把结果拉回本地并生成报告
    Run(RunArgs),

    /// 交叉编译出设备端二进制
    Build {
        /// 目标 ABI：auto / arm64 / armv7 / x86_64
        #[arg(long, default_value = "auto")]
        abi: String,
        /// 设备序列号（auto 时用于探测 ABI）
        #[arg(short, long)]
        serial: Option<String>,
    },

    /// 只把二进制 push 到手机
    Deploy {
        #[arg(short, long)]
        serial: Option<String>,
        /// 指定本地二进制路径（缺省用 target/<abi>/release 下的产物）
        #[arg(long)]
        binary: Option<PathBuf>,
        /// push 前先编译
        #[arg(long)]
        rebuild: bool,
    },

    /// 把手机上的会话目录拉回本地
    Pull {
        /// 设备上的会话目录
        remote: String,
        /// 本地目标目录
        local: Option<PathBuf>,
        #[arg(short, long)]
        serial: Option<String>,
    },

    /// 依据已有会话目录重新生成 HTML 报告
    Report { session: PathBuf },

    /// 列出已连接设备
    Devices,

    /// 用内置模拟 App 跑一遍（无需真机，用于验证链路与查看报告样式）
    Demo {
        /// 输出目录
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// 步数
        #[arg(long, default_value_t = 12)]
        steps: usize,
        /// 打开生成的报告
        #[arg(long)]
        open: bool,
        /// 状态指纹取法（用于对照两种口径的效果）
        #[arg(long, default_value = "structural", value_parser = ["structural", "exact"])]
        state_mode: String,
    },

    /// 探测设备能力（dump / 截图 / activity）
    Probe {
        #[arg(short, long)]
        serial: Option<String>,
    },
}

#[derive(Args, Debug, Clone)]
struct AgentArgs {
    /// 被测应用包名
    #[arg(short, long)]
    package: String,

    /// 启动 Activity（缺省自动解析 LAUNCHER）
    #[arg(short, long)]
    activity: Option<String>,

    /// 输出目录（设备上的路径）
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// 运行时长上限（秒），0 表示不限
    #[arg(long, default_value_t = 600)]
    duration: u64,

    /// 最大步数，0 表示不限
    #[arg(long, default_value_t = 0)]
    max_steps: usize,

    /// 两步动作之间的间隔（毫秒）
    #[arg(long, default_value_t = 700)]
    interval: u64,

    /// 动作执行后等待多久再截图（毫秒）
    #[arg(long, default_value_t = 600)]
    settle: u64,

    /// 随机种子
    #[arg(long, default_value_t = 20260909)]
    seed: u64,

    /// 关闭截图
    #[arg(long)]
    no_screenshot: bool,

    /// 关闭截图上的操作标注
    #[arg(long)]
    no_annotate: bool,

    /// 同时保留未标注的原始截图
    #[arg(long)]
    keep_raw: bool,

    /// 文本输入候选池文件（一行一个）
    #[arg(long)]
    text_pool: Option<PathBuf>,

    /// 连续停留在同一状态多少步后触发返回
    #[arg(long, default_value_t = 8)]
    max_same_state: usize,

    /// 状态指纹取法：structural = 只看可交互控件身份（默认，界面上的时间/电量/
    /// 进度百分比这类动态文本不会干扰判定）；exact = 把全部文本也算进指纹（旧行为，
    /// 动态文本多时状态会爆炸，导致同一页面被反复探索）
    #[arg(long, default_value = "structural", value_parser = ["structural", "exact"])]
    state_mode: String,

    /// 应用最多被重启多少次
    #[arg(long, default_value_t = 30)]
    max_relaunch: usize,

    /// 输出每一步详情
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Args, Debug)]
struct RunArgs {
    #[command(flatten)]
    agent: AgentArgs,

    /// 设备序列号
    #[arg(short, long)]
    serial: Option<String>,

    /// 手机端结果根目录（默认 /sdcard/atraverse）
    #[arg(long, default_value = "/sdcard/atraverse")]
    remote_dir: String,

    /// 拉回本地的目录（默认 sessions/<包名>_<时间戳>）
    #[arg(long)]
    out: Option<PathBuf>,

    /// 不自动编译，直接使用已有产物
    #[arg(long)]
    no_build: bool,

    /// 强制重新 push 二进制
    #[arg(long)]
    force_push: bool,

    /// PC 端模式：不部署到手机，直接在电脑上用 adb 驱动（调试用）
    #[arg(long)]
    host: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Agent(a) => cmd_agent(a),
        Cmd::Run(a) => cmd_run(a),
        Cmd::Build { abi, serial } => cmd_build(&abi, serial.as_deref()),
        Cmd::Deploy {
            serial,
            binary,
            rebuild,
        } => cmd_deploy(serial.as_deref(), binary.as_deref(), rebuild),
        Cmd::Pull {
            remote,
            local,
            serial,
        } => cmd_pull(&remote, local.as_deref(), serial.as_deref()),
        Cmd::Report { session } => cmd_report(&session),
        Cmd::Devices => cmd_devices(),
        Cmd::Demo {
            out,
            steps,
            open,
            state_mode,
        } => cmd_demo(out.as_deref(), steps, open, &state_mode),
        Cmd::Probe { serial } => cmd_probe(serial),
    }
}

// ---------------------------------------------------------------- 手机端

fn cmd_agent(a: AgentArgs) -> Result<()> {
    if a.package.is_empty() {
        bail!("--package 不能为空");
    }
    let mut dev = device::LocalDevice::new();
    let output = match &a.output {
        Some(p) => {
            util::ensure_dir(p)?;
            p.clone()
        }
        None => PathBuf::from(runner::pick_device_output(&runner::default_session_name(
            &a.package,
        ))),
    };

    let stop = Arc::new(AtomicBool::new(false));
    install_signal_handler(stop.clone());

    let opts = build_opts(&a, output.clone(), stop)?;

    println!("[输出目录] {}", output.display());
    let out = runner::run(&mut dev, opts)?;
    print_summary(&out.session, &output, &out.report);
    Ok(())
}

// ---------------------------------------------------------------- PC 端编排

fn cmd_run(a: RunArgs) -> Result<()> {
    if a.agent.package.is_empty() {
        bail!("--package 不能为空");
    }
    let adb = device::AdbDevice::new(a.serial.clone());
    adb.wait_for_device(std::time::Duration::from_secs(30))?;

    // ---- PC 端（adb 驱动）模式
    if a.host {
        let mut dev = adb;
        let stop = Arc::new(AtomicBool::new(false));
        install_signal_handler(stop.clone());
        let output = a
            .out
            .clone()
            .unwrap_or_else(|| runner::default_output_dir(&a.agent.package));
        util::ensure_dir(&output)?;
        let opts = build_opts(&a.agent, output.clone(), stop)?;
        let out = runner::run(&mut dev, opts)?;
        print_summary(&out.session, &output, &out.report);
        return Ok(());
    }

    // ---- 设备端模式：编译 → push → 远端执行 → 拉取
    let target = resolve_target(&adb)?;
    let bin = target_binary(&target);
    if !a.no_build && (!bin.exists() || a.force_push) {
        println!("[编译] {}", target);
        cargo_build(&target)?;
    }
    if !bin.exists() {
        bail!(
            "未找到设备端二进制: {}\n请先执行: atraverse build  或  atraverse run（去掉 --no-build）",
            bin.display()
        );
    }
    println!("[部署] {} -> {}", bin.display(), REMOTE_BIN);
    adb.push(&bin, REMOTE_BIN)?;
    adb.sh(
        "chmod 755 /data/local/tmp/atraverse",
        std::time::Duration::from_secs(10),
    )?;

    let name = runner::default_session_name(&a.agent.package);
    let remote_out = format!("{}/{}", a.remote_dir.trim_end_matches('/'), name);
    let local_out = a
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from("sessions").join(&name));

    let mut cmd = format!("{} agent --package {}", REMOTE_BIN, a.agent.package);
    if let Some(act) = &a.agent.activity {
        cmd.push_str(&format!(" --activity {}", act));
    }
    cmd.push_str(&format!(" --output {}", remote_out));
    cmd.push_str(&format!(" --duration {}", a.agent.duration));
    cmd.push_str(&format!(" --max-steps {}", a.agent.max_steps));
    cmd.push_str(&format!(" --interval {}", a.agent.interval));
    cmd.push_str(&format!(" --settle {}", a.agent.settle));
    cmd.push_str(&format!(" --seed {}", a.agent.seed));
    cmd.push_str(&format!(" --max-same-state {}", a.agent.max_same_state));
    cmd.push_str(&format!(" --max-relaunch {}", a.agent.max_relaunch));
    if a.agent.no_screenshot {
        cmd.push_str(" --no-screenshot");
    }
    if a.agent.no_annotate {
        cmd.push_str(" --no-annotate");
    }
    if a.agent.keep_raw {
        cmd.push_str(" --keep-raw");
    }
    if a.agent.verbose {
        cmd.push_str(" --verbose");
    }
    // 文本池：在手机上用内置默认池，需要自定义请先 push 文件并用 agent 模式启动

    println!("[执行] adb shell {}", cmd);
    let status = adb.shell_inherit(&cmd)?;
    if !status.success() {
        eprintln!("[警告] 设备端进程退出码: {:?}", status.code());
    }

    println!("[拉取] {} -> {}", remote_out, local_out.display());
    util::ensure_dir(&local_out)?;
    adb.pull(
        &remote_out,
        local_out.parent().unwrap_or(std::path::Path::new(".")),
    )
    .context("拉取结果失败（若 /sdcard 不可写，可改用 --remote-dir /data/local/tmp/atraverse）")?;

    let pulled = if local_out.join("session.json").exists() {
        local_out.clone()
    } else {
        // adb pull 目标已存在时会多套一层目录名
        local_out.join(&name)
    };
    let dir = if pulled.join("session.json").exists() {
        pulled
    } else {
        local_out.clone()
    };

    let s = session::read_session(&dir)?;
    let report = report::render(&s, &dir.join("report"))?;
    print_summary(&s, &dir, &report);
    Ok(())
}

fn build_opts(a: &AgentArgs, output: PathBuf, stop: Arc<AtomicBool>) -> Result<runner::RunOptions> {
    Ok(runner::RunOptions {
        package: a.package.clone(),
        activity: a.activity.clone(),
        output,
        duration_secs: a.duration,
        max_steps: a.max_steps,
        interval_ms: a.interval,
        settle_ms: a.settle,
        seed: a.seed,
        screenshot: !a.no_screenshot,
        annotate: !a.no_annotate,
        keep_raw: a.keep_raw,
        text_pool: load_text_pool(a.text_pool.as_deref())?,
        max_same_state: a.max_same_state,
        state_mode: crate::strategy::StateMode::parse(&a.state_mode)
            .ok_or_else(|| anyhow::anyhow!("未知的 --state-mode 取值: {}", a.state_mode))?,
        max_relaunch: a.max_relaunch,
        verbose: a.verbose,
        stop,
    })
}

// ---------------------------------------------------------------- 其它子命令

fn cmd_build(abi: &str, serial: Option<&str>) -> Result<()> {
    let target = match abi {
        "auto" => {
            let adb = device::AdbDevice::new(serial.map(|s| s.to_string()));
            adb.wait_for_device(std::time::Duration::from_secs(20))?;
            let a = device::getprop(&adb, "ro.product.cpu.abi");
            println!("[设备 ABI] {}", a);
            device::target_for_abi(&a).to_string()
        }
        "arm64" | "arm64-v8a" | "aarch64" => "aarch64-linux-android".to_string(),
        "armv7" | "armeabi-v7a" | "arm" => "armv7-linux-androideabi".to_string(),
        "x86_64" => "x86_64-linux-android".to_string(),
        other => bail!("未知 ABI: {}（可选 auto/arm64/armv7/x86_64）", other),
    };
    cargo_build(&target)?;
    println!("[产物] {}", target_binary(&target).display());
    Ok(())
}

fn cmd_deploy(serial: Option<&str>, binary: Option<&std::path::Path>, rebuild: bool) -> Result<()> {
    let adb = device::AdbDevice::new(serial.map(|s| s.to_string()));
    adb.wait_for_device(std::time::Duration::from_secs(20))?;
    let target = resolve_target(&adb)?;
    if rebuild {
        cargo_build(&target)?;
    }
    let bin = match binary {
        Some(b) => b.to_path_buf(),
        None => target_binary(&target),
    };
    if !bin.exists() {
        bail!(
            "未找到二进制: {}（先 cargo build --target {} --release）",
            bin.display(),
            target
        );
    }
    adb.push(&bin, REMOTE_BIN)?;
    adb.sh(
        "chmod 755 /data/local/tmp/atraverse",
        std::time::Duration::from_secs(10),
    )?;
    println!("[完成] {} 已部署到 {}", bin.display(), REMOTE_BIN);
    Ok(())
}

fn cmd_pull(remote: &str, local: Option<&std::path::Path>, serial: Option<&str>) -> Result<()> {
    let adb = device::AdbDevice::new(serial.map(|s| s.to_string()));
    let local = match local {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from("sessions"),
    };
    util::ensure_dir(&local)?;
    adb.pull(remote, &local)?;
    println!("[完成] 已拉取到 {}", local.display());
    Ok(())
}

fn cmd_report(dir: &std::path::Path) -> Result<()> {
    let s = session::read_session(dir)?;
    let p = report::render(&s, &dir.join("report"))?;
    println!("报告已生成: {}", p.display());
    Ok(())
}

fn cmd_devices() -> Result<()> {
    let devs = device::AdbDevice::devices()?;
    if devs.is_empty() {
        println!("未发现设备。请连接手机并开启 USB 调试（adb devices）。");
        return Ok(());
    }
    for d in &devs {
        let adb = device::AdbDevice::new(Some(d.clone()));
        match device::DeviceInfo::fetch(&adb, d) {
            Ok(info) => println!("{}\t{}", d, info.display()),
            Err(_) => println!("{}", d),
        }
    }
    Ok(())
}

fn cmd_demo(
    out: Option<&std::path::Path>,
    steps: usize,
    open: bool,
    state_mode: &str,
) -> Result<()> {
    use device::mock::MockDevice;

    let dir = match out {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from("sessions").join(runner::default_session_name("com.demo")),
    };
    util::ensure_dir(&dir)?;

    // 混入一段崩溃日志，顺便验证异常捕获与报告里的异常面板
    let log = vec![
        "09-01 12:00:01.100  1234  1234 I demo    : start".to_string(),
        "09-01 12:00:03.200  1234  1234 E AndroidRuntime: FATAL EXCEPTION: main".to_string(),
        "09-01 12:00:03.201  1234  1234 E AndroidRuntime: Process: com.demo, PID: 1234".to_string(),
        "09-01 12:00:03.202  1234  1234 E AndroidRuntime: java.lang.NullPointerException: boom".to_string(),
        "09-01 12:00:03.203  1234  1234 E AndroidRuntime: \tat com.demo.MainActivity.onClick(MainActivity.java:42)".to_string(),
        "09-01 12:00:05.300  1234  1300 E ActivityManager: ANR in com.demo".to_string(),
        "09-01 12:00:05.301  1234  1300 E ActivityManager: PID: 1234".to_string(),
        "09-01 12:00:05.302  1234  1300 E ActivityManager: Reason: Input dispatching timed out".to_string(),
    ];
    let scratch = std::env::temp_dir().join(format!("atraverse_demo_{}", std::process::id()));
    let mut dev = MockDevice::with_log(log, &scratch);

    let stop = Arc::new(AtomicBool::new(false));
    install_signal_handler(stop.clone());
    let opts = runner::RunOptions {
        package: "com.demo".to_string(),
        activity: None,
        output: dir.clone(),
        duration_secs: 120,
        max_steps: steps,
        interval_ms: 0,
        settle_ms: 0,
        seed: 20260909,
        screenshot: true,
        annotate: true,
        keep_raw: false,
        text_pool: vec![],
        max_same_state: 6,
        state_mode: crate::strategy::StateMode::parse(state_mode)
            .ok_or_else(|| anyhow::anyhow!("未知的 --state-mode 取值: {}", state_mode))?,
        max_relaunch: 3,
        verbose: false,
        stop,
    };

    println!("[演示] 使用内置模拟设备遍历 com.demo（{} 步）", steps);
    let out = runner::run(&mut dev, opts)?;
    print_summary(&out.session, &dir, &out.report);
    if open {
        let _ = opener(&out.report);
    }
    Ok(())
}

fn opener(path: &std::path::Path) -> Result<()> {
    let p = path.to_string_lossy().to_string();
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", &p])
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(&p).spawn();
    }
    Ok(())
}

fn cmd_probe(serial: Option<String>) -> Result<()> {
    let mut dev = device::AdbDevice::new(serial);
    dev.wait_for_device(std::time::Duration::from_secs(20))?;
    let info = device::DeviceInfo::fetch(&dev, "")?;
    println!("设备: {}", info.display());
    println!("ABI -> target: {}", device::target_for_abi(&info.abi));
    println!(
        "当前 Activity: {}",
        device::current_activity(&dev).unwrap_or_else(|e| format!("<{}>", e))
    );
    match dev.dump_xml() {
        Ok(xml) => match dump::parse_hierarchy(&xml) {
            Ok(h) => {
                let vis = h.visible_nodes();
                println!(
                    "控件树: {}x{}, 节点 {} 个，可见 {} 个，可交互 {} 个",
                    h.width,
                    h.height,
                    h.nodes().len(),
                    vis.len(),
                    vis.iter().filter(|n| n.interactive()).count()
                );
                for n in vis.iter().take(12) {
                    println!(
                        "  {} {} {} {}",
                        n.short_class(),
                        n.resource_id,
                        util::truncate(&n.text, 16),
                        n.bounds
                    );
                }
            }
            Err(e) => println!("解析失败: {}", e),
        },
        Err(e) => println!("dump 失败: {}", e),
    }
    match dev.screencap() {
        Ok(png) => println!("截图: {} 字节", png.len()),
        Err(e) => println!("截图失败: {}", e),
    }
    Ok(())
}

// ---------------------------------------------------------------- 辅助

fn install_signal_handler(stop: Arc<AtomicBool>) {
    let stop2 = stop.clone();
    let _ = ctrlc::set_handler(move || {
        if stop2.load(Ordering::SeqCst) {
            println!("\n[强制退出]");
            std::process::exit(130);
        }
        println!("\n[收到中断] 正在收尾并生成报告，请稍候…");
        stop2.store(true, Ordering::SeqCst);
    });
}

fn load_text_pool(p: Option<&std::path::Path>) -> Result<Vec<String>> {
    match p {
        None => Ok(Vec::new()),
        Some(path) => Ok(std::fs::read_to_string(path)?
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()),
    }
}

fn resolve_target(adb: &device::AdbDevice) -> Result<String> {
    let abi = device::getprop(adb, "ro.product.cpu.abi");
    println!("[设备 ABI] {} -> {}", abi, device::target_for_abi(&abi));
    Ok(device::target_for_abi(&abi).to_string())
}

fn target_binary(target: &str) -> PathBuf {
    let name = if cfg!(windows) {
        "atraverse.exe"
    } else {
        "atraverse"
    };
    PathBuf::from("target")
        .join(target)
        .join("release")
        .join(name)
}

fn cargo_build(target: &str) -> Result<()> {
    let status = std::process::Command::new("cargo")
        .args(["build", "--target", target, "--release"])
        .status()
        .context("无法启动 cargo")?;
    if !status.success() {
        bail!("cargo build --target {} 失败", target);
    }
    Ok(())
}

fn print_summary(s: &session::Session, dir: &std::path::Path, report: &std::path::Path) {
    println!("\n================ 遍历完成 ================");
    println!("应用        : {}", s.package);
    println!(
        "运行位置    : {}",
        if s.device.mode == "device" {
            "手机端"
        } else {
            "PC 端（adb）"
        }
    );
    println!(
        "步骤        : {} 步（{}）",
        s.steps.len(),
        util::fmt_duration(s.duration_ms)
    );
    println!(
        "Activity    : {} 个，去重状态 {} 个",
        s.coverage.activity_count, s.state_count
    );
    println!(
        "控件覆盖    : {}/{} ({:.1}%)  [可交互控件；全部节点 {}/{} = {:.1}%]",
        s.coverage.interactive_touched,
        s.coverage.interactive_nodes,
        s.coverage.interactive_coverage * 100.0,
        s.coverage.touched_nodes,
        s.coverage.total_nodes,
        s.coverage.node_coverage * 100.0
    );
    println!(
        "异常        : crash {} 个，ANR {} 个",
        s.crash_count(),
        s.anr_count()
    );
    println!("会话目录    : {}", dir.display());
    println!("报告        : {}", report.display());
    println!("==========================================");
}
