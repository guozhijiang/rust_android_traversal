# atraverse —— Android 应用智能遍历工具（Fastbot 形态）

用 Rust 写的 Android 应用自动遍历 / 稳定性测试工具。**主体以单个二进制的形式运行在手机上**
（`adb shell /data/local/tmp/atraverse agent ...`），不依赖 PC 长连接，行为和 Fastbot 一致；
同时提供一个 PC 端编排命令，负责交叉编译、部署、拉回结果和出报告。

## 能力

| 能力 | 说明 |
|---|---|
| 高覆盖遍历 | 状态去重（Activity + 控件内容签名）+ 未探索动作优先 + 未操作控件加权 + 卡死回溯/重启 |
| 动作类型 | click / long_click / swipe（上下左右）/ input（文本输入）/ back / keyevent / launch |
| 步骤记录 | 每步一条 `steps.jsonl`，含动作、目标控件、坐标、前后 Activity、状态、耗时 |
| 截图与标注 | 每步截图，并在**截图本体上**画出标记：点击圆环准星、长按双环、滑动箭头、输入框高亮、返回键角标，带步骤编号与类型文字 |
| 异常捕获 | 实时流式 logcat，识别 Java 崩溃 / Native 崩溃 / ANR；自动抓取 dropbox、`/data/anr/traces.txt`、tombstone |
| 覆盖率 | Activity 数、去重状态数、控件级覆盖率（已操作/发现），并列出未被操作过的控件 |
| 报告 | 单文件 HTML（暗色）：指标卡、异常面板（可展开堆栈）、Activity 覆盖率表、操作时间线（缩略图点开大图）、按动作类型筛选 |

## 目录结构

```
src/
  main.rs             CLI：agent / run / build / deploy / pull / report / devices / probe
  device/
    mod.rs            Device trait + 高层操作（dump/input/screencap/activity…）+ DeviceInfo
    local.rs          手机端后端：直接 exec /system/bin 下的工具
    adb.rs            PC 端后端：通过 adb 转发（调试用）
  runner.rs           遍历主循环
  strategy.rs         高覆盖策略、状态图、覆盖率统计
  dump.rs             uiautomator dump XML 解析
  model.rs            Rect / UiNode / Hierarchy / Action
  annotate.rs         截图绘制（内置 5x7 点阵字模，无外部字体依赖）
  monitor.rs          logcat 流式监听与 crash/ANR 识别
  session.rs          步骤与会话数据结构（JSON）
  report.rs           HTML 报告渲染
```

## 构建

需要 Android NDK（r27d 验证过）与 rustup 的 Android target：

```bash
rustup target add aarch64-linux-android armv7-linux-androideabi
```

### 第一步：生成交叉编译配置（只需一次）

`.cargo/config.toml` 必须写死本机 NDK 绝对路径，因此**不入库**（否则换台机器就失效，
还会把你的目录结构泄露到公开仓库）。用脚本按 `ANDROID_NDK_HOME` → `$ANDROID_HOME/ndk/*`
→ 常见安装位置自动探测并生成：

```bash
# Windows
powershell -ExecutionPolicy Bypass -File scripts/setup-cargo-config.ps1
# 显式指定 NDK
powershell -ExecutionPolicy Bypass -File scripts/setup-cargo-config.ps1 -NdkRoot <你的NDK根目录>

# Linux / macOS
./scripts/setup-cargo-config.sh
# 或 NDK_ROOT=/path/to/ndk ./scripts/setup-cargo-config.sh
```

已存在配置时脚本会跳过，加 `-Force` / `--force` 可重新生成。结构参见
`.cargo/config.toml.example`。找不到 NDK 时脚本会明确报错并告诉你怎么设。

### 第二步：编译

```bash
# 编译设备端二进制（自动按设备 ABI 选择 target）
cargo run -- build

# 或手动
cargo build --target aarch64-linux-android --release
```

产物在 `target/aarch64-linux-android/release/atraverse`（已 strip，约 5.4MB）。

## 使用

### 方式一：手机端直接跑（推荐）

```bash
adb push target/aarch64-linux-android/release/atraverse /data/local/tmp/atraverse
adb shell chmod 755 /data/local/tmp/atraverse

adb shell "/data/local/tmp/atraverse agent --package com.example.app --duration 900"
```

会话产物写在 `/sdcard/atraverse/<包名>_<时间戳>/`（`/sdcard` 不可写时自动退回
`/data/local/tmp/atraverse/`），目录内容：

```
session.json        会话完整数据（步骤 / 异常 / 覆盖率）
steps.jsonl         逐步追加的步骤记录
logcat.txt          本次会话的原始 logcat
screenshots/        带操作标注的截图
thumbs/             报告用的缩略图
incidents/          crash / ANR 详细日志（含 dropbox、traces）
report/index.html   HTML 报告
```

在会话目录里 `touch STOP` 可让遍历优雅收尾并出报告。

### 方式二：PC 端一键（编译 → 部署 → 执行 → 拉回 → 出报告）

```bash
cargo run -- run --package com.example.app --duration 900
# 常用参数
cargo run -- run -p com.example.app -a .MainActivity --duration 1800 --max-steps 800 \
    --interval 700 --settle 600 --seed 42 --verbose
```

结果落到本地 `sessions/<包名>_<时间戳>/`，报告在其中的 `report/index.html`。

### 调试用：PC 端通过 adb 驱动

```bash
cargo run -- run -p com.example.app --host --duration 120
```

不部署二进制，遍历引擎跑在电脑上，命令经 adb 转发。适合对比排查 / 没有交叉编译环境时。

### 无真机：内置模拟设备演示

```bash
cargo run -- demo --steps 12        # 遍历内置模拟 App（含一次崩溃 + 一次 ANR），产出完整会话与报告
cargo run -- demo --open            # 跑完自动用浏览器打开报告
```

`demo` 走与真机完全相同的遍历引擎、截图标注、异常监控与报告链路，适合验证环境和查看报告样式。

### 其它命令

```bash
cargo run -- devices                      # 列出设备
cargo run -- probe                        # 检查 dump / 截图 / activity 是否正常
cargo run -- deploy --rebuild             # 只编译并 push 二进制
cargo run -- pull /sdcard/atraverse/xxx   # 拉回手机上的会话
cargo run -- report sessions/xxx          # 重新生成 HTML 报告
```

## 关键参数

| 参数 | 默认 | 说明 |
|---|---|---|
| `--duration` | 600 | 运行秒数，0 不限 |
| `--max-steps` | 0 | 最大步数，0 不限 |
| `--interval` | 700 | 两步之间的间隔 ms，太小容易被系统判定为 Monkey 压力 |
| `--settle` | 600 | 动作后等待多久再截图 ms（等动画结束） |
| `--seed` | 20260909 | 随机种子，同种子可复现遍历顺序 |
| `--max-same-state` | 8 | 连续多少步停留在同一状态就触发返回 |
| `--max-relaunch` | 30 | 应用最多重启次数 |
| `--text-pool` | 内置 | 一行一个候选文本，用于填充 EditText |
| `--no-screenshot` / `--no-annotate` / `--keep-raw` | - | 截图相关开关 |

## 覆盖率是怎么算的

- **状态** = Activity + 屏幕尺寸 + 所有可见交互控件的内容签名（含文本、选中态）+ 全屏文本集合。
  滚动出新的列表项会改变文本集合 → 新状态，因此"滑到底"也能计入覆盖进展。
- **控件身份** 优先用 `resource-id`，没有则用 `类名+文本`，再没有用 `类名+位置`。
- **已操作** = 该控件被 click / long_click / swipe / input 命中过（全局累计）。
- 只统计属于被测包名的控件，系统状态栏、权限弹窗等不计入分母。

## 已知限制

- `input text` 走系统的 `input` 命令，**不支持中文**（无 IME 广播）。需要中文输入时可在
  `--text-pool` 里放 ASCII，或自行扩展为广播方式注入。
- `/data/anr/traces.txt` 在非 root 设备上通常不可读，此时 ANR 详情依赖 logcat 的 `ANR in` 段。
- 依赖 `uiautomator dump`（Android 4.3+ 自带）。纯自绘 / 游戏类界面控件树可能为空，
  此时策略会退化为屏幕中心滑动。
- 设备端通过 `adb shell` 以 shell 用户运行，写 `/sdcard` 需要设备允许 shell 写外置存储；
  不允许时会自动退回 `/data/local/tmp/atraverse`。

## 测试

```bash
cargo test --release
```
覆盖：dump XML 解析、状态指纹与动作生成/穷尽、覆盖率增长、截图标注像素、报告渲染。
