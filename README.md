# atraverse —— Android 应用智能遍历工具（Fastbot 形态）

[![build](https://github.com/guozhijiang/rust_android_traversal/actions/workflows/build.yml/badge.svg)](https://github.com/guozhijiang/rust_android_traversal/actions/workflows/build.yml)

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
| 异常捕获 | 实时流式 logcat，识别 Java 崩溃 / Native 崩溃 / ANR；自动抓取 dropbox、`/data/anr/traces.txt`、tombstone；**并抓一张异常现场截图**（崩溃那一刻屏幕什么样），报告里可点开大图 |
| 覆盖率 | Activity 数、去重状态数、**可交互控件覆盖率**（纯布局容器不计入分母），并列出还没被点到的可交互控件 |
| 报告 | 单文件 HTML（暗色）：指标卡、异常面板（可展开堆栈）、Activity 覆盖率表、操作时间线（缩略图点开大图）、按动作类型筛选 |
| **用例执行** | `script` 子命令：按用例 IR（YAML）执行动作 + 断言，产出**用例级报告**（PASS / FAIL / BLOCKED），并复用 crash/ANR 监控 |

## 目录结构

```
src/
  main.rs             CLI：agent / run / script / build / deploy / pull / report / devices / probe / demo
  device/
    mod.rs            Device trait + 高层操作（dump/input/screencap/activity…）+ DeviceInfo
    local.rs          手机端后端：直接 exec /system/bin 下的工具
    adb.rs            PC 端后端：通过 adb 转发（调试用）
    mock.rs           模拟设备（无真机时验证链路）
  runner.rs           遍历主循环
  script.rs           用例 IR 执行器（断言原语 + 定位器校验闸门 + 用例级报告）
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

### 不想装 NDK：直接用 CI 编好的产物

仓库配了 GitHub Actions（`.github/workflows/build.yml`）：

- **push / PR** → 跑单元测试 + 交叉编译 3 个 ABI（`arm64-v8a` / `armeabi-v7a` / `x86_64`），
  产物作为 workflow artifact 保留，在 Actions 页面直接下载
- **push tag `v*`** → 额外自动建 GitHub Release，附 3 个二进制 + SHA256

```bash
git tag v0.1.0 && git push origin v0.1.0   # 触发 Release
```

拿到二进制后按「方式一：手机端直接跑（推荐）」那节 push 到手机即可。
CI 用的是 ubuntu runner + NDK r27d，与本地脚本同一套探测逻辑（仓库里的
`scripts/setup-cargo-config.sh`），因此不会出现本地能编、CI 编不出来的情况。

产物在 `target/aarch64-linux-android/release/atraverse`（已 strip，约 1.5MB）。

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
cargo run -- demo --state-mode exact --out sessions/cmp   # 用旧的状态指纹口径对照跑一遍
```

`demo` 走与真机完全相同的遍历引擎、截图标注、异常监控与报告链路，适合验证环境和查看报告样式。

两点说明，免得看到结果时误会：

- **截图是纯色底 + 标注**。模拟设备没有真实画面，它只提供一张纯色 PNG，
  步骤截图上看到的控件框、点击准星、滑动箭头全是工具自己画上去的（这恰好说明标注链路是通的）。
  异常现场截图不做标注，所以是纯色底。
- 它只有两个页面，因此状态数只有 2；状态数不随步数增长是正常的，不是策略卡住了。

### 按用例 IR 执行（`script`）

遍历模式回答「覆盖率够不够」，`script` 回答「这几条用例过不过」。用例写成 YAML（中间表示），
**不写坐标、不写选择器** —— 只引用元素注册表里的元素 id，运行时才展开该元素的分级定位器链。

```bash
# 1) 先只跑定位器校验闸门：不连设备，检查用例引用的元素是否都真实存在
cargo run -- script cases/douyin --kb kb --dry-run

# 2) 连真机执行
cargo run -- script cases/douyin --kb kb --package com.ss.android.ugc.aweme

# 单条用例 / 只跑某个子集 / 指定输出目录
cargo run -- script cases/douyin/TC_DY_NAV_001.yaml --kb kb --package com.example
cargo run -- script cases/douyin --kb kb --filter NAV --package com.example
cargo run -- script cases/douyin --kb kb --out reports/nav -v
```

产物：`reports/script_<时间戳>/report.html`（用例级报告）、`cases.json`（机器可读）、`logcat.txt`。

目录名若已存在会自动改名为 `script_<时间戳>_2`、`_3`……（时间戳只到秒，
同一秒内连跑两次不会互相覆盖 —— 覆盖过一次，两个套件的报告只剩后一份，很容易被误导）。

退出码：有失败或阻塞时为 1，可直接当 CI 门禁。

#### 用例 IR 结构

```yaml
id: TC_DY_NAV_001
title: 底部导航切换到「我」，应进入个人主页
priority: P1
tags: [navigation]
trace:                                   # 需求追溯
  requirement: REQ-DY-NAV-01
  pattern: navigation_tab
  dimension: 正常流
setup:                                   # 预热（可选）：跑在 steps 之前
  - { n: 0, action: wait, value: "2000" }
steps:
  - { n: 1, action: click, locator: { id: FrameLayout_4 }, desc: 点击底部「我」 }
assertions:
  - { n: 2, type: text_visible, expect: "编辑主页", timeout_ms: 6000 }
  - { n: 3, type: element_exists, locator: { id: 底部Tab_首页 }, timeout_ms: 6000 }
teardown:
  - { action: click, locator: { id: FrameLayout_0 } }
```

- `locator.id` 是 `<kb>/elements/*.yaml` 里的元素 id（通常由 `tools/snapshot.py` 从真机 dump 生成）
- 动作用 `click` / `long_click` / `input`（配 `value`）/ `swipe`（配 `direction`）/ `back` / `key`（配 `code`）/ `launch`（配 `component`）/ `wait`（配 `value`，毫秒）
- `desc` 只是给人看的备注，会渲染进报告

#### 断言原语

| 类型 | 判据 |
|---|---|
| `text_visible` / `text_contains` | 任一节点的 text 或 content-desc **包含** `expect` |
| `text_equals` | 任一节点的 text 或 content-desc **等于** `expect` |
| `element_exists` / `element_absent` | 元素在 / 不在当前页面（**只认语义定位器**） |
| `activity_is` | 前台 Activity 含 `expect` |
| `no_crash` | 本条用例执行期间没有归因到被测应用的 crash / ANR（需 `--package`，否则跳过） |

断言会一直重试到 `timeout_ms`（默认 4000）。**重试只改变等待时长，不降低判据严格程度。**

#### ★ 定位器校验闸门

执行前，用例里每个元素引用都会回查 L2 元素注册表。命中不了就 `BLOCKED` 且**不执行**：

- 引用的元素 id 不存在 → 拦住（LLM 幻觉出的 `resource-id` 主要死在这里）
- 断言用的元素**只有位置 / 序号定位器** → 拦住（下面解释）
- 用例没有任何断言 → 拦住（没有判据的用例不算测试）

**为什么存在性断言不许用位置 / 序号定位器**：位置只能回答「该点在哪」，回答不了「控件在不在」；
`class_index` 更糟 —— 同 class 的节点一大把（一个详情页几十个 `View`），它几乎总能匹配到某个无关节点，
于是断言**永远通过**。假阳性比 FAIL 危险得多：FAIL 会有人去查，假的 PASS 会让一条根本没测到的用例看起来是绿的。

同理，**断言全部被跳过的用例记为 `SKIP` 而不是 `PASS`** —— 没验证过就不能算绿。

#### 定位器分级

元素在 `kb/elements/*.yaml` 里带一条**分级定位器链**，按可信度从高到低尝试：

```
resource_id  >  动态文本正则(content_desc_regex / text_regex)  >  原文 content-desc  >  text
             >  bounds_center（屏幕相对位置）  >  class_index（按 dump 序号，最后兜底）
```

后两级只在**动作**里可用（它们能给出坐标），在**存在性断言**里被闸门拦掉。
动态 content-desc 必须正则化，否则内容一变就失效：「喜欢5.4万」→ `^未点赞，喜欢[\d.万亿]+，按钮$`。

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
| `--state-mode` | structural | 状态指纹取法：`structural` 只看可交互控件身份（抗动态文本）；`exact` 把全部文本算进指纹（旧行为，仅用于对照） |
| `--seed` | 20260909 | 随机种子，同种子可复现遍历顺序 |
| `--max-same-state` | 8 | 连续多少步停留在同一状态就触发返回 |
| `--max-relaunch` | 30 | 应用最多重启次数 |
| `--text-pool` | 内置 | 一行一个候选文本，用于填充 EditText |
| `--no-screenshot` / `--no-annotate` / `--keep-raw` | - | 截图相关开关 |
| `--keep-animations` | 关 | **默认遍历期间临时关闭系统动画**，结束（含失败）后自动恢复原值 |

> **为什么要关动画**：`uiautomator dump` 必须等到 UI 空闲才拿控件树。页面有持续动画时
> （视频在播、转场没结束、无限循环动画）永远等不到 idle，dump 直接报
> `ERROR: could not get idle state.` 失败，失败前还要白等十几秒。
> 实测抖音推荐流（视频在播）：动画开启时 dump **3/3 失败**（每次约 11.4s）；
> 三个 `*_animation_scale` 置 0 后，同一页面 **3/3 成功**（每次约 3s）。
> 这是通用手段（Appium / UiAutomator2 也这么做），与应用无关。
> 若进程被强杀（`kill -9`）来不及恢复，设备上的动画开关会残留为 0。

## 覆盖率是怎么算的

### 分母只算「可交互控件」

`FrameLayout` 这类纯布局容器是永远点不到的，把它们算进分母会让覆盖率永远到不了 100%，
看不出真实短板。所以主指标口径是：

> 可点击 / 可长按 / 可勾选 / 可滚动 / 输入框，以及带文本的叶子节点（点击会冒泡到父容器）。

报告里同时给出「全部节点」宽口径供对照。实测同一次遍历：可交互 **5/5 = 100%**，
全部节点 **5/6 = 83.3%** —— 差的正是那个不可能被点到的根容器。

### 状态指纹默认「结构优先」

**状态** = Activity + 屏幕尺寸 + 可交互控件的身份集合（不含文本内容）。

这一点很关键：如果把界面文本也算进指纹，时钟、电量、进度百分比、推荐流内容这类
每屏都在变的东西会让**每一步都被判成新状态**，后果是同一页面上的控件被反复当作
「未探索动作」点击（浪费步数），而且「连续 N 步停留在同一状态」的卡死检测永远无法触发。

实测（`demo --steps 60`）：结构优先得到 **2 个状态**、精确模式得到 **25 个状态**，
两者最终覆盖率相同 —— 状态数少 92% 而结果不变，说明合并掉的确是同一个页面的抖动。
需要旧行为做对照时加 `--state-mode exact`。

### 其它

- **控件身份**：优先 `resource-id`，没有则 `类名+文本`，再没有用 `类名+位置`；
  状态指纹另用不含文本的 `resource-id → 类名+位置`，避免动态文本干扰。
- **已操作** = 该控件被 click / long_click / swipe / input 命中过（全局累计）。
- 只统计属于被测包名的控件，系统状态栏、权限弹窗等不计入分母。

### crash / ANR 计数会去重

同一次崩溃会同时出现在两条日志通道（`AndroidRuntime` 的 `FATAL EXCEPTION` 和
events buffer 的 `am_crash`），同一次 ANR 也有 `ANR in` 与 `am_anr` 两条。
工具会按「类型 + 包名 + 异常类名」在 5 秒窗口内判重，只记一条；
5 秒内发生的两个**不同**异常不会被误合并。

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
25 项单测，覆盖：dump XML 解析、状态指纹（含动态文本抗性 / 状态爆发对照）、动作生成与穷尽、
覆盖率口径、截图标注像素、logcat 异常识别与跨通道去重、mock 设备端到端、大输出不死锁。
