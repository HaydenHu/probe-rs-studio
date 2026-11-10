# ProbeRS Studio

纯 Rust 编写的桌面固件烧录工作台。直接链接 `probe-rs` 库，不调用外部 CLI、不启动子进程、不依赖 WebView/Electron。

## 两个仓库

本项目的代码拆成两个独立仓库维护：

| 仓库 | 内容 |
| --- | --- |
| [HaydenHu/probe-rs](https://github.com/HaydenHu/probe-rs) | probe-rs 的 fork。`daplink-hid-fallback` 分支包含国产 DAPLink 兼容补丁；`master` 保持与官方上游同步 |
| [HaydenHu/probe-rs-studio](https://github.com/HaydenHu/probe-rs-studio) | 本仓库，GUI 烧录软件全部源码 |

本仓库 `Cargo.toml` 通过 git 依赖引用 fork 的 `daplink-hid-fallback` 分支，因此单独 clone 本仓库即可构建。

补丁内容（均在 `daplink-hid-fallback` 分支）：

1. `find_packet_size()` 在 16 次 `DAP_Info(PacketSize)` 超时后，对 CMSIS-DAP v1 HID 设备回退到 HID 报告描述符给出的 `report_size`，而不是报 `NoPacketSize`。部分 DAPLink 固件不实现该命令。
2. **跳过陈旧回复**：这类探针会在 HID 输入队列里留下上一个会话 / 超时命令的回复（实测多为 `[00 02 40 00]`，即 `DAP_Info(PacketSize)=64`），当前命令读到别人的回执就报 `CommandIdMismatch(0x0, ...)`，长流程烧录会在 Program 中途失败。接收端现在会丢弃不属于当前命令的回执（上限 16 个），直到拿到自己的回复；同时忽略空闲时发出的全 0 HID 报告，并把 Windows 的 `WinError: WaitTimedOut` 归一为可重试的超时。
3. **控制命令自动重同步**：`DAP_Transfer` / `DAP_TransferBlock` 不重试（重复 SWD 写会推进 AP 地址），其余控制类命令在重同步后重试一次；打开探针阶段对超时也重试，给刚结束上一轮会话的固件留出恢复时间。

GUI 侧另有兜底：`Connect` / `ReadBack` / `Flash` / `VerifyFirmware` / `EraseAll` / `Reset` 遇到传输层错误时按 300 / 600 / 900 ms 退避，最多重试 3 次（写相同镜像是幂等的）；**选项字节写入不参与自动重试**，配置错误、文件缺失、校验失败也不会被重试。

## 功能

- **会话式布局**：左侧窄栏「当前会话」集中连接配置、固件选择、烧录选项与全部操作（可滚动）；右侧会话区是已载入固件的 16 进制查看器（虚拟滚动、8/16/32 字节每行、ASCII 栏、镜像 / 文件原始字节切换）
- 探针枚举、按设备身份选择、连接测试；**调试探针下拉栏已加宽**（侧栏 272 px、弹出列表 320 px，长名称完整显示）；USB 插入/拔出自动检测（1.5 秒轮询 + 去抖），烧录期间自动暂停轮询
- **网络探针（elaphureLink 无线下载器）**：fork 的 probe-rs 新增 TCP CMSIS-DAP 后端，按 OpenOCD `cmsis-dap backend elaphurelink` 的握手与 `0x88` vendor 帧收发；GUI「连接配置」默认已填 `dap.local`，下拉框里直接可选、改地址实时更新（详见「网络探针：elaphureLink」）
- **烧录按钮常驻**：「开始烧录」固定在「当前会话」侧栏顶部、滚动区之外，滚到任何设置位置都能一键触达；全界面只有这一个烧录入口，避免重复控件
- **固件可移除**：左栏「固件文件」标题行右侧、右侧固件信息卡片右上角各有一个**纯「✕」**关闭按钮（不带文字，悬停显示说明）；点击会清除当前固件、解码信息、16 进制缓冲，并清掉配置里保存的路径（下次启动不会自动载回）；「最近固件」以**一排紧凑相连的按钮**放在右侧固件信息卡片的**右栏**（点选载入、右键移除、一键清空、当前固件高亮），只占一行高度
- **日志可复制**：日志台「复制全部」把当前可见日志按时间顺序写进系统剪贴板，单条日志右键可「复制整行 / 只复制消息内容」，运行日志页也有「复制全部日志」；选中文字按 Ctrl+C 同样能粘贴到其它程序（详见「日志复制与剪贴板」）
- **串口识别 + 串口终端**：纯 Rust Win32 FFI 枚举当前在线 COM 口，自动标注 DAPLink / CMSIS-DAP 自带的虚拟串口（VID/PID、接口号、占用状态），2 秒轮询自动刷新；左侧设置波特率等线路参数并独占打开，右侧会话区实时收发，支持文本 / HEX、CRLF、RX/TX 计数
- **定时发送**：设定周期（100 ms – 1 小时）后自动重发发送区内容，显示累计次数；关闭串口或会话中断会立即停发，启用状态不写进配置，重启不会自己发
- **彩色 emoji 快捷输入**：发送区可展开一排彩色 emoji 与常用 AT 指令，点击插入到光标处（按字符索引，中文 / emoji 混排不会切坏 UTF-8）；egui 0.37 配合 epaint `color_fonts` 与系统 Segoe UI Emoji 字体渲染真彩色字形
- **Ctrl + 滚轮缩放**：接收区与发送区各自独立调整字号（8–32 pt），标题旁实时显示当前字号，随配置持久化
- **选项字节设置**：读取 / 编辑芯片的读保护 RDP、BOR、看门狗、NRST、BOOT1、SDK 与写保护 WRP；自动维护全部反码字节、显示变更 diff、写入需勾选风险确认并二次点击，写入走厂商专用 OPT.FLM 算法并回读校验
- **读取目标芯片 ID**：只读采集 SWD `DPIDR`、CoreSight ROM Table（制造商 / 部件号）、`CPUID`（内核型号）、厂商 `DBGMCU_IDCODE`（DEV_ID / REV_ID）、96 位 UID 与 Flash 容量；内置 PY32F0xx / 常见 STM32 寄存器映射，其它型号走通用寄存器并可填自定义地址。顶部工具栏与左栏均可打开独立报告窗口，烧录选项里还可勾选「烧录前读取芯片 ID」，在同一连接内自动记录身份到日志
- 内置约 4700 个芯片型号，另附 Holychip HC32F4XX 系列；**内置库没有的芯片用同一个「加载芯片支持…」入口导入厂商 `.pack` / `.pack.zip`、已解压目录，或只给一个 `.FLM` 算法**（按文件类型自动分流，纯 Rust 解析并永久保存），支持外部 `.yaml` 芯片描述
- **最近确认的型号**：点选、连接或烧录时自动记录（最多 8 个），搜索框下方给出随输入过滤的快捷按钮，型号列表把历史置顶并标注「· 最近」，工作台也能一键切回；随配置持久化，旧配置自动以当前已选型号作为首条历史
- HEX / ELF / BIN 固件解析（地址区间、记录数、SHA256），支持拖拽载入
- 一键烧录、库内 verify、独立读回 SHA256 比对、烧录前读回备份、复位运行、整片擦除（二次确认）
- **不稳定的 DAPLink 容错**：自动跳过错位的 HID 回执与空闲全 0 报告，控制命令重同步，传输类失败按退避自动重试整个幂等任务（详见「两个仓库」）
- **连接失败可诊断**：`--connect` 是 GUI「连接测试」的无界面等价路径；探针打开失败时重新枚举最多 3 次，错误链区分 HID 固件死锁（需重新插拔）与进程独占（需关闭 pyOCD 等），死锁状态不再无谓重试。芯片 ID 报告还会核对实测内核与所选型号是否一致，型号选错直接给出告警
- 深浅双主题、SVG 矢量导航图标（运行时用 epaint 直接绘制，无位图纹理）、可折叠日志台、进度与状态栏
- 配置自动保存到可执行文件同目录的 `probe-rs-studio.json`

## 环境要求

- Rust **1.95+**（egui / eframe 0.37 的 MSRV 为 1.95）
- 支持 OpenGL 的桌面环境（egui / eframe 0.37 的 glow 后端，未启用 wgpu）
- Windows：运行时加载 `C:/Windows/Fonts/Deng.ttf`（或 `msyh.ttc` / `simhei.ttf`）作为中文字体回退；egui 内置字体不含 CJK 字形。彩色 emoji 另需 `C:/Windows/Fonts/seguiemj.ttf`（或 `NotoColorEmoji.ttf` / `seguisym.ttf`），缺失时降级为单色轮廓
- **彩色 emoji 依赖 epaint 的 `color_fonts` feature**：egui/eframe 0.37 并不转发该 feature，因此 `Cargo.toml` 里直接依赖了 `epaint` 并开启它，Cargo 会把 feature 合并到同一个 epaint 实例。去掉这行会退回单色 emoji
- 构建需能访问 crates.io 与 github.com；如网络受限，参见 `.cargo/config.example.toml`

## 构建与运行

```bash
git clone https://github.com/HaydenHu/probe-rs-studio.git
cd probe-rs-studio
cargo build --release --bin probe-rs-studio
./target/release/probe-rs-studio
```

首次构建会拉取并编译整个 probe-rs 依赖树，耗时较长；之后为增量编译。

### 本地开发（两个仓库平级放置）

若希望改动 fork 源码后立即生效、避免重复编译，把两个仓库 clone 到同级目录：

```
workspace/
├── probe-rs/            # HaydenHu/probe-rs，切到 daplink-hid-fallback
└── probe-rs-studio/     # 本仓库
```

然后把 `Cargo.toml` 中的依赖行改为：

```toml
probe-rs = { path = "../probe-rs/probe-rs", default-features = false, features = [
    "builtin-targets", "cmsisdap_v1", "builtin-formats",
] }
```

`.cargo/config.example.toml` 里还提供了共享 `target-dir` 与国内 crates.io 镜像的可选配置。

## Windows 可执行文件图标

`assets/icons/app-icon.rgba`（64×64 RGBA）是运行时窗口 / 任务栏图标；同一图案的
`assets/icons/app-icon.ico` 是 16 / 32 / 48 / 64 四种尺寸的 32bpp + alpha ICO，用作
**exe 文件本身的图标**（资源管理器、快捷方式、Alt+Tab 都读它）。

链路：`tools/gen_icons.py` 同时生成 `.rgba` 与 `.ico` → `assets/app-icon.rc` 由 Windows SDK
`rc.exe` 编译成 `assets/app-icon.res` → `build.rs` 用 `cargo:rustc-link-arg` 把预编译的
`.res` 链接进 exe。`.res` 已提交，所以正常构建**不需要** SDK 工具，也不需要 `winres`
之类的 build-dependency，`--offline` 同样可用；非 Windows 目标自动跳过。

改图标时重新生成：

```bash
python tools/gen_icons.py
rc /nologo /fo assets\app-icon.res assets\app-icon.rc
cargo build --release --bin probe-rs-studio
```

`rc.exe` 在 Windows SDK 的 `bin/<版本>/x64/` 下；Git Bash 里要设 `MSYS_NO_PATHCONV=1`，
否则 `/fo` 会被路径转换破坏。

## 扩展芯片支持：一个入口，pack / FLM 都行

`.pack` 本质就是「一个 `.pdsc` 器件描述 + 若干 `.FLM` Flash 算法」；只给一个 `.FLM`
时缺的仅仅是 pdsc 里的芯片名、内核和 RAM。所以界面只有**一个「加载芯片支持…」按钮**，
按选中的文件类型自动分流，不需要先判断该点哪个：

| 选择的文件 | 处理方式 |
| --- | --- |
| `.pack` / `.pack.zip` / 已解压的 pack 目录 | 解析 `.pdsc` 并导入包内**全部** `.FLM`，一次生成所有型号，直接导入 |
| `.FLM` | 打开参数表单：自动读出 Flash 布局与算法入口，补芯片名、内核、RAM 后导入 |

- 「芯片库」页 → **加载芯片支持…**；「设置」页是同一个按钮，旁边的「已解压目录…」
  只用于选择文件夹（文件对话框无法选目录）
- 文件对话框默认「**全部文件 (\*.\*)**」，不会被扩展名挡住；可下拉切到
  `.pack / .pack.zip / .FLM` 专用过滤器。无扩展名或非常规扩展名的文件按内容
  （ZIP / ELF 文件头）自动识别；`--load` 与拖放同样支持
- 也可把芯片支持文件或已解压目录直接拖进窗口（固件文件仍按固件载入）
- 不需要重新编译、不需要 Python、不需要 probe-rs 源码里的 `target-gen`：纯 Rust 解析
  `.pdsc` 与 ELF32 `.FLM`，生成 target YAML，先用 `Registry::add_target_family_from_yaml()`
  校验通过才写入 exe 同目录 `targets/<名>.yaml`（不会留下坏文件）
- 写入后立即刷新芯片库，日志逐条列出新增型号；**重启后自动加载**，
  条目在芯片库中标注「· 扩展包」，之后可像内置芯片一样搜索、查看详情、连接、烧录、校验

| 来源 | 位置 | 说明 |
| --- | --- | --- |
| 随程序编译 | `targets/HC32F4XX.yaml` | 通过 `include_str!` 嵌入 exe，单个可执行文件即可运行 |
| 导入的芯片描述 | exe 同目录 `targets/*.yaml`（或运行目录 `targets/*.yaml`） | 由界面/CLI 自动生成，启动时自动扫描，放进去即生效 |
| 临时指定 | 设置页「外部芯片描述 (.yaml)」 | 单文件，优先级最高，可覆盖同名 family |

命令行同样只有一个入口，`--load` 会按文件类型做和界面完全相同的分流（无需打开窗口）：

```bash
# 无需硬件，打印当前芯片库总数与扩展包加载情况
probe-rs-studio.exe --chips

# 给什么就导入什么：pack 走包内全部算法，FLM 走参数缺省值
probe-rs-studio.exe --load <厂商.pack.zip>
probe-rs-studio.exe --load MYCHIP.FLM

# 加 --out 只转换不安装，把 YAML 写到指定位置
probe-rs-studio.exe --load <厂商.pack> --out <out.yaml>
probe-rs-studio.exe --load MYCHIP.FLM --chip MYCHIP_A --out mychip.yaml

# 显式参数仍保留（等价于 --load 分流后的路径）
probe-rs-studio.exe --pack <厂商.pack.zip>
probe-rs-studio.exe --flm MYCHIP.FLM --chip MYCHIP_A
```

**支持范围与限制**

- 内核：`Cortex-M0/M0+` → `armv6m`，`M3` → `armv7m`，`M4/M7` → `armv7em`，
  `M23/M33/M55/M85/Star-MC1` → `armv8m`；其它内核明确报错，不会生成错误描述
- `.FLM` 需为 32 位小端 ELF，含 `PrgCode` / `PrgData` 段与 `FlashDevice` 符号（Keil 标准格式）
- 按 `(FLM, 声明的 Flash 区间)` 生成算法实例，**不做代码内容去重**：不同容量但代码相同的
  FLM 会各自生成独立算法，避免官方 `target-gen` 把 256K 算法误去重的问题
- 只提取 Flash 算法与内存布局；不导入 SVD、启动文件、外设头文件等非烧录所需内容
- `.pack` 导入路径假定 FLM 为位置无关代码（绝大多数厂商算法都是）；需要固定加载地址的算法请直接选该 `.FLM`，在表单里显式指定加载地址
- 同名的 family 会覆盖内置同名描述，日志会显示加载来源

当前附带 `Holychip.HC32F4XX.1.0.7.pack` 的 3 个型号：`HC32F401BRCX`（256 KiB Flash）、
`HC32F401BREX`（512 KiB）、`HC32F401BRGX`（1024 KiB），Cortex-M4 / 192 KiB RAM。
`tools/flm_to_yaml.py` 是生成这份 bundled YAML 的离线脚本；界面内的转换器是
`src/pack.rs`（纯 Rust），单测用 `tests/fixtures/` 里的精简 pack 覆盖 ZIP 嵌套、
pdsc、ELF32 与 Registry 解析。文件类型分流由 `src/pack.rs` 的 `classify_source()`
统一负责，界面按钮、拖放和 `--load` 共用它。

### 选 `.FLM` 时需要补什么

厂商只给了 Flash 算法文件、没有完整 `.pack`（或 `.pdsc` 缺失 / 想造一个自定义型号）时，
在同一个「加载芯片支持…」按钮里选它即可，程序会弹出参数表单：

- 先读取 `.FLM` 内嵌的 `FlashDevice`，把 **Flash 起始地址、容量、页大小、扇区布局、
  擦除值、编程/擦除超时、代码与各入口函数** 自动填好并显示
- 只需补充 ELF 里没有的信息：**芯片型号**（逗号分隔可一次建多个共用同一算法的型号）、
  **系列名称**、**内核**、**RAM 起始地址 / 容量**（默认 `0x20000000` + 64 KiB）、
  AP 编号（默认 0）。Flash 两项留空即沿用算法自带值，填了则覆盖
- 可选 **固定加载地址**：仅非位置无关（非 PIC）算法需要，值为算法代码在 RAM 中的起始地址，
  必须落在声明的 RAM 区间内并给算法头部留出空间
- 提交后与 `.pack` 走同样的流程：生成 YAML → `Registry` 校验 → 写入 exe 同目录
  `targets/flm_<系列>.yaml` → 立即刷新芯片库，重启自动加载，标注「· 扩展包」

命令行（无需硬件、无需打开窗口）：

```bash
# 最小用法：内核默认 armv7em（Cortex-M4/M7），芯片名取 FLM 文件名
probe-rs-studio.exe --load MYCHIP.FLM

# 完整参数：两个型号、Cortex-M0+、自定义 RAM 与 Flash 容量
probe-rs-studio.exe --load MYCHIP.FLM --chip MYCHIP_A,MYCHIP_B --family "MYCHIP Series" \
  --core M0+ --flash-base 0x08000000 --flash-size 0x10000 \
  --ram-base 0x20000000 --ram-size 0x4000

# 非 PIC 算法：显式指定代码加载地址与 AP
probe-rs-studio.exe --load MYCHIP.FLM --chip MYCHIP_A --load-address 0x20000400 --ap 0
```

`--core` 接受 `M0` / `M0+` / `M3` / `M4` / `M7` / `M23` / `M33` / `M55` / `M85`
或 `armv6m` / `armv7m` / `armv7em` / `armv8m`。数字参数支持十进制或 `0x` 十六进制。

**注意**：RAM 必须能容纳算法代码、栈（约 1 KiB）与至少 2 个页缓冲，否则导入时会明确报错；
部分厂商的多个容量型号共用同一份 FLM，其 `FlashDevice` 声明的容量可能与目标芯片不符，
此时在弹窗或 `--flash-size` 中改成实际容量。不支持 ZIP64、非 32 位小端 ELF、非 PIC 且
未指定加载地址的算法。

## 界面：当前会话侧栏 + 16 进制查看器

工作台左侧**窄栏**就是「当前会话」，所有控件都在这里（内容多时可滚动）：

侧栏最上方、滚动区之外常驻一个**「开始烧录」主按钮**（需已载入固件且探针就绪），
不会随设置滚动而消失；下面的分组才会滚动：

| 分组 | 内容 |
| --- | --- |
| 连接配置 | 调试探针（按身份选择）、协议 / 速度、目标芯片、最近型号快捷项、复位下连接、连接测试、芯片详情 |
| 固件文件 | 标题行右侧纯「✕」关闭按钮、浏览 / 拖入固件、格式、BIN 基址、文件名 / 大小 / SHA256 |
| 烧录选项与操作 | 6 个选项开关、独立校验、读回备份、复位运行、整片擦除（二次确认）、读回地址与长度、备份路径 |
| 快捷操作 | 刷新探针、查看日志（最近固件已移到右侧固件信息卡片的右栏） |

右侧会话区是**固件 16 进制查看器**：载入 `.hex` / `.elf` / `.bin` 后立即显示
偏移、十六进制字节和 ASCII 栏，采用虚拟滚动，只渲染可见行（大文件也不会卡）；
固件信息卡片分**左右两栏**：左栏是文件名 / 摘要 / SHA256 与查看设置，右上角是纯「✕」
关闭按钮（不带文字标签，悬停提示「关闭固件」），一键卸载；**右栏是「最近固件」紧凑按钮组**（按钮紧贴无间隔、无分隔符，当前固件高亮；点选载入、
右键移除、标题栏「清空」），一行就能显示全部记录，正好填掉原本大片空白，也不会把卡片撑高。页面标题栏只保留「浏览…」。

- 「数据源」可切换 **固件镜像**（Intel HEX / BIN 解码后的连续镜像，偏移按实际烧录地址，
  如 `0x08000000`）与 **文件原始字节**；ELF 或无法解码的文件自动回退到原始字节
- 每行 8 / 16 / 32 字节可选，ASCII 栏可关闭；最多缓存 32 MiB，超过时明确提示
- 文件可直接拖进窗口；格式、SHA256、记录数等信息显示在查看器顶部

底部日志台与状态栏保持不变。其它导航页（芯片库 / 运行日志 / 设置 / 串口识别）共享
右侧会话区，左侧栏切换为该页的上下文工具；串口识别页的右侧会话区是**串口终端**。

## 日志复制与剪贴板

底部日志台和「运行日志」页都能把内容送进**系统剪贴板**：

| 入口 | 复制什么 |
| --- | --- |
| 日志台「复制全部」 / 运行日志页「复制全部日志」 | 当前显示的日志，按时间顺序，格式为 `[HH:MM:SS] [LEVEL] 消息`；被「调试信息」隐藏的 DEBUG 行会一并排除 |
| 单条日志右键 →「复制整行」 | 该行完整文本（时间 + 级别 + 消息） |
| 单条日志右键 →「只复制消息内容」 | 不带时间与级别的原始消息 |
| 选中日志文字后 Ctrl+C | egui 标签选中文本，可粘贴到其它程序 |

> **为什么之前「能选中却复制不了」**：为精简依赖，工程把 `eframe` 设成
> `default-features = false`，只启用了 `glow` / `default_fonts`。`clipboard` 属于默认特性，
> 少了它 `egui-winit` 会退化成**只在应用内部生效的假剪贴板** —— Ctrl+C 与
> `ctx.copy_text()` 都「成功」，日志也会记录已复制，但系统剪贴板始终是空的，
> 外部程序粘贴不到任何东西。现在显式启用 `eframe` 的 `clipboard` feature
> （Windows 走 `arboard` / `clipboard-win`），并加了单测
> `eframe_system_clipboard_feature_stays_enabled` 防止再次被误关。这一个修复同时让
> 日志台、芯片 ID 窗口的「复制全部」以及标签选中后的 Ctrl+C 全部真正生效。

复制成功后日志台会追加一条 `已复制 N 条日志到剪贴板` 作为可见反馈；
导出成 `.log` / `.txt` 文件仍用「运行日志」页的「导出日志」。

## 串口识别

导航里原来的「调试探针」页与工作台的探针信息重复，已替换为「串口识别」：

- 读取 `HKLM\HARDWARE\DEVICEMAP\SERIALCOMM` 判断**当前在线**的 COM 口，再用 SetupAPI
  的 PORTS 类补充友好名、硬件 ID、USB VID/PID 与接口号
- DAPLink / CMSIS-DAP / J-Link 等调试器自带的虚拟串口会单独标注，计数显示在「连接概览」
- 每 2 秒检测一次，插入 / 拔出会在日志里提示；设置页的「自动检测」开关同时控制探针与串口监视
- 全程纯 Rust Win32 FFI，无外部依赖、不启动子进程；`--ports` 可在不开窗口时打印同样结果

### 串口会话：参数、接收区与发送区

「串口识别」页沿用同一套会话式布局，左侧窄栏是**串口设置**，右侧会话区是**串口终端**。

| 分组 | 内容 |
| --- | --- |
| 串口设置（左栏） | 在线端口下拉、波特率（可手填，常用值下拉）、数据位 5–8、停止位 1 / 1.5 / 2、校验位 无 / 奇 / 偶 / Mark / Space、流控 无 / XON-XOFF / RTS-CTS、「打开串口 / 关闭串口」、在线端口列表与端口详情 |
| 串口会话（右栏） | **接收区**标题行：端口 / 参数 / RX·TX 累计字节 / 缓冲字节数、清空接收、打开 / 关闭串口、HEX 显示、自动滚动、字号；**发送区**标题行：定时发送与周期、清空输入、追加 CRLF、HEX 发送、快捷输入、字号，卡片右下角是圆形矢量**朝上纸飞机**「发送」按钮（仿聊天输入框布局）。两区都只有标题行 + 内容区，不再有独立信息条或分散的操作行 |

- 默认 `115200 8N1`；参数随 `probe-rs-studio.json` 持久化，**打开期间参数被锁定**，
  要改参数先关闭串口。端口以独占方式 `CreateFileW` 打开，被其它程序占用会明确报错
- 后台线程用 `mpsc` 收发命令：`COMMTIMEOUTS` 设成非阻塞读，8 ms 轮询输入队列，
  收到数据发 `Event::SerialRx` 给 UI，窗口空闲时也按 50 ms 持续重绘，不会漏显或卡住
- **读取不再依赖 `COMSTAT.cbInQue`**：部分 USB CDC / 虚拟串口驱动只有在真正调用
  `ReadFile` 时才把已到达的字节搬进输入队列，`cbInQue` 可能一直是 0。旧实现用它做门控，
  导致 TX/RX 短接也完全收不到数据。现在每轮先 `ClearCommError` 清掉锁存错误，
  再无条件非阻塞 `ReadFile(4096)`，超时仍保证立即返回
- 接收区支持 **文本 / HEX + ASCII 显示**、自动滚动、清空；缓冲是 64 KiB 的滚动窗口，
  长时间收发不会无限增长。文本模式下若收到非 UTF-8 字节（`from_utf8_lossy` 会显示替换符），
  标题行会黄字提示「含非文本字节 · 建议勾选 HEX 显示」，避免把二进制响应当成程序乱码
- 发送区支持**文本**或 **HEX**（如 `41 54 0D 0A`，忽略空格 / `0x` 前缀 / 分隔符）、
  「追加 CRLF」（仅文本模式生效）、清空输入；点卡片**右下角圆形纸飞机发送按钮**（机头朝上、纯矢量双机翼 + 折线，不是 emoji）或按 **Ctrl+Enter** 发出。
  仿聊天输入框：大输入区 + 底部提示行 + 右下角矢量圆形主按钮；清空输入 / CRLF / HEX /
  定时发送 / 快捷输入等辅助开关集中在**标题行**
- 每次发送回 `Event::SerialTxCount` 更新 TX 计数，并在日志台以 DEBUG 级列出实际发出的字节；
  **设备拔出会自动关闭串口**并提示，端口列表 2 秒刷新一次
- 收发全程纯 Rust Win32 FFI（`GetCommState` / `SetCommState` / `ReadFile` / `WriteFile`），
  不依赖 `serialport` 等第三方 crate

#### 定时发送

发送区标题行勾选「定时发送」并填写周期（**100 – 3 600 000 ms**，默认 1000 ms）：
串口打开后按周期自动重发当前发送框内容，标题行左侧显示「定时发送已发 N 次」。周期非法会红字提示且不发送；
关闭串口、串口被拔出或会话通道断开都会立即停发。定时发送的每帧不写 DEBUG 日志（避免刷屏），
但 RX/TX 计数与累计次数照常更新。**启用状态不持久化**，重启后默认关闭，避免无人值守时自己发数据。

#### 彩色 emoji 快捷输入

点发送区标题行「快捷输入 😀」展开可横向滚动的快捷条：28 个常用 emoji + `AT` /
`AT+GMR` / `AT+RST` / `CRLF` / `00 FF`。点一下就把字符插到**光标位置**（用 egui 的
`TextEditOutput.cursor_range`，按字符而非字节索引）；不点发送框时默认追加到末尾。
emoji 是真正的 UTF-8 字符，发送出来是 4 字节的 `F0 9F 98 80` 之类，用 HEX 显示可直接看到。

egui 0.37 的彩色字形光栅化由 epaint 的 **`color_fonts`** feature 提供（COLR / CBDT / sbix，
内部走 `vello_cpu`），而这个 feature **没有被 egui/eframe 转发**，所以 `Cargo.toml` 直接依赖
`epaint` 来开启它。字体方面 `theme::install_fonts` 把系统 `seguiemj.ttf` 注册为
Proportional / Monospace 的 fallback，并**放在 CJK 字体之前**，避免中文字体里的单色符号抢先命中。

#### Ctrl + 滚轮缩放

鼠标悬停在接收区或发送区上按住 Ctrl 滚滚轮（或触控板捏合），**只缩放该区**的文字，
范围 **8 – 32 pt**，步长由 egui 的 `zoom_delta()` 经 `log2` 映射，放大 / 缩小对称。
标题栏实时显示当前字号，两个字号分别写入 `probe-rs-studio.json`，重启后保留。
HEX 模式的行高也随字号一起变，不会出现行重叠。

#### 真机验证

DAPLink 虚拟串口 `COM10`（`c251:f001`，`MI_00`），**TX/RX 已短接**，115200 8N1：

| 场景 | 结果 |
| --- | --- |
| `--loopback COM10` | `[PASS]` 发 `Hello\r\n`，8 ms 后收到 `48 65 6C 6C 6F 0D 0A` |
| GUI 文本发送 `Hello`（追加 CRLF） | TX 7 字节，DEBUG `48 65 6C 6C 6F 0D 0A` |
| GUI HEX 发送 `41 54 0D 0A` | TX 累计 11 字节，DEBUG `41 54 0D 0A` |
| 快捷输入插入 4 个 emoji 并发送 | TX / RX 同步增长，接收区显示彩色 emoji |
| 定时发送持续运行 | RX/TX 从 18 → 396 字节同步增长，接收区回显彩色 emoji |
| Ctrl+滚轮 | 接收区 11.5 → 17.8 → 32.0 pt，发送区独立保持 13.0 pt（可单独缩放） |

排查接线与驱动问题时先跑 `--loopback`：它走的是 GUI 同一套 `Port::read/write` 代码，
能一眼区分「线没接好 / 端口选错」和「GUI 显示问题」。

## 最近确认的型号

在芯片库里点选型号、在工作台点「连接测试 / 连接 / 烧录」时，该型号会被记入历史，
保存在 `probe-rs-studio.json` 的 `recent_chips`（最多 8 条，最新在前，大小写不敏感去重）。

- **芯片库页**：搜索框下方出现「最近确认的型号」快捷按钮，且**随搜索词实时过滤**；
  输入 `stm` 就只剩历史里的 STM 型号，点一下即选中并清空搜索
- 完整型号列表中历史型号**按最近顺序置顶**，副标题追加「· 最近」，计数行提示「· 最近置顶」
- **工作台**：连接配置区有「最近型号」一行，可不切页直接换回常用芯片
- 从旧版本升级时，配置里没有 `recent_chips`，程序会把已保存的 `config.chip`
  作为首条历史种子，启动后立刻可用；已有历史不会被覆盖

需要清空时，退出程序后编辑 `probe-rs-studio.json`，把 `recent_chips` 改成 `[]`。

## 选项字节设置（Option Bytes）

顶部工具栏「选项字节」或工作台左栏「选项字节设置…」打开独立窗口；
也可以用 `probe-rs-studio.exe --ob-open` 启动后直接进入该窗口。

PY32F0xx 的布局来自 Puya 官方 CMSIS 头文件 `py32f003x8.h` 的 `OB_TypeDef`
与 `Puya.PY32F0xx_DFP.pdsc`：选项字节是 `0x1FFF0E80` 起 **16 字节**，
每个配置字节都跟着自己的按位反码，硬件用反码校验配置是否有效。

| 偏移 | 字段 | 说明 |
| --- | --- | --- |
| `0x00 / 0x02` | `RDP / nRDP` | `0xAA` = Level 0 无保护，`0x55` = Level 1 读保护 |
| `0x01 / 0x03` | `USER / nUSER` | `FLASH_OPTR[15:8]`：BOR 使能、BOR 阈值(3 bit)、IWDG_SW、WWDG_SW、NRST_MODE、nBOOT1 |
| `0x04..0x07` | `SDK_STRT / SDK_END` 及反码 | 安全区起止块（2 KiB 粒度） |
| `0x08..0x0B` | 保留 | 固定 `0xFF` |
| `0x0C..0x0F` | `WRP / nWRP` | 16 位写保护掩码（u16 LE），某位为 0 表示对应扇区被保护 |

窗口里可以：读取当前 16 字节、按字段下拉编辑、直接编辑原始 HEX、一键「补全反码」、
查看当前值 / 目标值与逐项变更 diff、复制目标 HEX、恢复为芯片当前值。
字段编辑会自动重算全部反码并把保留字节置为 `0xFF`，避免写出被 FLASH 控制器
以 `OPTVERR` 拒绝的块。

### 写入与安全护栏

- **写入算法**：普通 Flash 算法不覆盖 `0x1FFF0E80`。程序内嵌了由
  `PY32F0xx_OPT.FLM` 转换出的算法描述（`assets/PY32F0xx_OPT.yaml`），
  写入时把该算法和它的 16 字节 NVM 区临时挂到当前 target 上，
  再由 probe-rs 标准 `FlashLoader` 完成擦除 / 编程 / 校验，最后独立回读比对并复位。
  不手工戳 FLASH 控制器寄存器。它只在 `assets/` 中编译进二进制，
  不会作为可选芯片出现在芯片库里。
- **二次确认**：必须勾选「我已了解风险」，且「写入选项字节」要点击两次。
  后端会再次调用 `option_bytes::validate()` 校验，无法绕过。
- **拒绝写入的情况**：目标与当前相同、反码/保留字节不匹配、全 `0x00`、
  RDP 不是 `0xAA/0x55`、RDP 改变但未勾选风险确认、OB 基址与内置算法地址不一致。
- **编辑状态不持久化**：窗口开关、待写镜像、风险勾选、二次确认状态都不写进
  `probe-rs-studio.json`，重启后不会残留一个待写入操作。
- 写入后需要给目标板**重新上电**，选项字节才会完全生效。

> ⚠️ 错误的 RDP / BOOT1 / NRST 配置可能让芯片无法通过 SWD 再次连接，
> 恢复通常需要整片擦除（会清除用户 Flash）。**写入路径未在真机上执行验证**
> （避免损坏在用芯片）；只读路径、字段编解码、反码、校验规则和
> 内置 OPT 算法能否被 probe-rs 解析，均有单元测试与真机只读验证覆盖。
> 首次对某块板子写入前，请先用 `--option-bytes` 只读核对。

### 芯片支持范围

| 芯片 | OB 基址 | 字段编辑 | 写入 |
| --- | --- | --- | --- |
| PY32F003 / PY32F030 / PY32F0xx | `0x1FFF0E80` | 支持 | 支持（内置 `PY32F0xx_OPT.FLM`） |
| PY32F002B | `0x1FFF0080` | 支持 | 只读 / 导出，需要 `PY32F002Bxx_OPT.FLM` |
| PY32F071 / PY32F072 | `0x1FFF3080` | 支持 | 只读 / 导出，需要对应 OPT.FLM |
| 其它芯片 | 手填基址 | 仅原始 16 字节 HEX | 只读 / 导出 |

未知芯片不会臆造位定义：只显示原始字节，避免给出错误的字段含义。
`USER` 各位的含义按 Puya SDK 的 `FLASH_OPTR` 位定义映射，
个别子型号若有差异，请以对应参考手册为准。

### 真机验证（PY32F003x8 · DAPLink `c251:f001`）

```text
芯片 PY32F003x8 · PY32F003 / F030 · 官方 SDK
OB 区 0x1FFF0E80 · 16 字节
原始: AA FE 55 01 1F 00 E0 FF FF FF FF FF FF FF 00 00
反码: 校验通过
  读保护 RDP = Level 0 · 无读保护
  BOR 阈值   = 3.1 – 3.2 V
  独立看门狗 = 软件看门狗      窗口看门狗 = 软件看门狗
  NRST 引脚  = NRST 作 GPIO     BOOT1 = 系统存储器
  SDK 起始块 = 0x1F             SDK 结束块 = 0x00
  写保护 WRP = 0xFFFF
[PASS] 选项字节读取完成（只读，未写入任何数据）
```

注意：本机的这块 DAPLink 在 **1000 kHz** 下会出现 CMSIS-DAP
`Command ID in response (0x0) does not match...`，切到 GUI 保存的
**10000 kHz** 才能稳定连接。`--option-bytes` 会复用
`probe-rs-studio.json` 里的探针身份与 SWD 速度，也可以用 `--speed` 覆盖。

## 读取目标芯片 ID

顶部工具栏「芯片 ID」或工作台左栏「读取芯片 ID…」打开独立窗口，点「读取芯片 ID」
即可连接当前探针并生成报告；`--id-open` 可以启动后直接进入该窗口。窗口支持
一键复制全部结果、填写自定义地址（按小端 32 位读取）、清空旧结果；如果报告来自
另一块板子（型号与当前选择不一致），会显示醒目提示要求重新读取。

| 来源 | 寄存器 / 位置 | 得到什么 |
| --- | --- | --- |
| SWD DP | `DPIDR`（DP 寄存器 0，bank 0） | DP 版本、部件号、设计者 JEP106 厂商名 |
| CoreSight | ROM Table（由 AP 基址自动发现） | 制造商 + 部件号，**对没有内置映射的型号也可用** |
| Cortex-M | `SCB->CPUID @ 0xE000ED00` | 内核型号（M0/M0+/M3/M4/M7…）、架构、版本 |
| 厂商 | `DBGMCU_IDCODE` | `DEV_ID[11:0]`、`REV_ID[31:16]` |
| 厂商 | 96 位 UID | 出厂唯一序列号 |
| 厂商 | Flash 容量寄存器（16 位，KiB） | 标称 Flash 容量 |

内置映射：

- **PY32F0xx**（`PY32F002 / F003 / F030 / F07x`）：地址全部取自 Puya 官方 CMSIS
  头文件与 `py32f0xx_hal.c` —— `IDCODE 0x40015800`、`UID 0x1FFF0E00`、
  `FlashSize 0x1FFF0FFC`。
- **STM32F1 / F0·F3 / F4**：常见参考手册地址，界面里明确标注「请按具体型号核对」；
  不同容量与子系列的系统存储区可能不同。
- **其它型号**（MM32、HC32、GD32 等）：只使用通用 ARM 寄存器与 ROM Table，
  在窗口里填自定义地址即可补读厂商 ID 寄存器。ROM Table 扫描会走几十次 SWD
  传输，所以**只对没有内置映射的型号执行**；PY32F0xx 这类已知型号不做这一步，
  烧录前的身份采集只是 5 次轻量寄存器读。

**烧录前读取芯片 ID** 是左栏的一个可勾选选项，**默认关闭**，避免改变现有烧录流程。
勾选后在 `Flash` 任务的同一连接内先做只读采集，把身份写进日志台并刷新报告窗口；
任何单项寄存器读取失败都只记 `[WARN]`，不会中断烧录。该选项随
`probe-rs-studio.json` 持久化，旧配置文件缺少该字段时按关闭处理。所有报告都是
运行时状态，**不写入配置文件**，重启后不会拿上一次的板子冒充当前芯片。

报告还会把实测 `CPUID` 的部件号与所选芯片描述声明的内核（`Armv6m` / `Armv7m` /
`Armv7em` / `Armv8m`）做一致性核对。型号选错时不再只是「连接测试一直失败」，而是直接给出
`[WARN] 实测内核 Cortex-M3 … 与所选芯片声明的 Armv6m 不一致，可能选错了型号`。
例如在一块 MM32L373PF（Cortex-M3）上误选 `PY32F003x8` 就会触发该提示。

> 芯片 ID 与「选项字节」是两件事：前者只读识别硅片身份，后者会改写配置 NVM。
> 两者共用同一套连接参数，但读取 ID 不会改变芯片任何内容。

## 网络探针：elaphureLink 无线下载器

[elaphureLink](https://github.com/windowsair/elaphureLink) 把一颗 CMSIS-DAP 调试器
（如 wireless-esp8266-dap）桥接到 Wi-Fi：设备开一个 TCP 服务，主机用
[openocd-elaphurelink](https://github.com/windowsair/openocd-elaphurelink) 的
`cmsis-dap backend elaphurelink` 连上去。本项目的 `HaydenHu/probe-rs` fork 把同一套协议
实现成 probe-rs 的一个 CMSIS-DAP 传输层，因此 GUI、CLI 与烧录链路都能直接用无线探针。

### 协议（与 OpenOCD 实现一致）

| 步骤 | 报文 |
| --- | --- |
| 连接 | TCP `host:3240`，默认 `dap.local`（可写 IP / `IP:端口` / mDNS 域名） |
| 握手 | 发 12 字节大端 `0x8a656c70` + `0x00000000` + `0x00010000`（proxy 版本）；回包同 identifier / command，DAP 版本主版本 ≥ 1 |
| 进入 vendor 域 | 发 `88 02 00 00`，等一个 status = 0 的 vendor 帧 |
| 每条 CMSIS-DAP 命令 | 发 `88 01 <长度:u16 大端> <原始命令>`；回收 `88 <status> <长度:u16 大端> <原始响应>` |

长度字段是**网络字节序**，载荷是不带 HID report id 的原始 CMSIS-DAP 命令/响应。
单包上限按 OpenOCD 的以太网 MTU 取 1496 字节；若设备不回答 `DAP_Info(PacketSize)`，
后端自动回退到 1496，而不是报 `NoPacketSize`。分片、迟到响应、坏帧头都有单测覆盖。

### 在 GUI 里使用

「当前会话」→「连接配置」→「网络探针」输入框**默认已填 `dap.local`**（与 OpenOCD 的
`elaphurelink.cfg` 一致），所以上方「调试探针」下拉框里一开始就有 `elaphureLink [0000:0002]`
这一项，直接选中即可连接、烧录、读芯片 ID，不必先点「应用」。

- 直接写 `dap.local` / `192.168.1.50:3240` 默认按 **elaphureLink** 解析；
- 也可显式写 `elaphurelink:dap.local` 或 `tcp:192.168.1.50:4441`（后者走 probe-rs 已有的
  OpenOCD TCP backend，帧格式是 `DAP\0`）；
- **改地址时下拉框实时更新**（旧的网络项自动替换），点「应用」会把该探针设为当前选中项
  并写日志；点「清除」清空地址并回到 USB 探针。

网络探针不参与 USB 热插拔轮询，地址随 `probe-rs-studio.json` 持久化；旧配置缺该字段时
也会自动补上 `dap.local`。

#### 连不上时先点「诊断连接…」

输入框下方有「诊断连接…」按钮（后台执行，不卡界面），把结果逐行写进日志台：

| 阶段 | 报告内容 |
| --- | --- |
| `[1/3]` | mDNS / DNS 解析结果与耗时（Windows 上 `.local` 解析可能就要 2–3 秒） |
| `[2/3]` | 逐个 IP 的 TCP 连接结果；失败时列出**已连着该端口的进程**和本机 3240 的监听者 |
| `[3/3]` | elaphureLink 12 字节握手与 vendor scope 回复、代理版本 |

诊断连接用完立刻关闭，不会长期占用设备。等价的命令行是
`probe-rs-studio.exe --network-check [地址]`（省略地址则用 GUI 保存的地址）。

### 命令行

```text
probe-rs-studio.exe --connect --network dap.local
probe-rs-studio.exe --chip-id --network dap.local
probe-rs-studio.exe --selftest firmware.hex --network 192.168.1.50:3240
probe-rs-studio.exe --network-check dap.local
```

不加 `--network` 时，headless 命令复用 GUI 保存的配置（包括已启用的网络探针）。
在 probe-rs CLI 里等价写法是 `--probe 0000:0002:dap.local`。

### 排错与验证状态

连接失败会按**失败阶段**给提示（地址解析 / TCP 连接 / 握手 / vendor scope），并指向
「诊断连接…」，不会像 USB 探针那样反复重试 3 次。真机排查已知的两个坑：

- **代理只接受一个客户端**：本机同时跑 `openocd -f interface/elaphurelink.cfg
  -c "cmsis-dap elaphurelink addr dap.local"` 时，OpenOCD 会独占唯一的连接，GUI 只能等到
  `connection timed out`。先退出 OpenOCD 再连。
- **本机 3240 被 usbipd-win 占用**：usbipd 默认也监听 TCP 3240，把地址填成
  `127.0.0.1` / `localhost` 会连到它，握手直接被重置。诊断的 `[注意]` 行会点名该进程。
- **设备 Web 配置页在线 ≠ DAP 服务在线**：无线 DAP 的 `http://<设备IP>/` 配置页能打开，
  只说明 WiFi 与 HTTP 正常；3240 的 DAP 任务可能因上一次异常断连卡死，需要给设备断电重启。

- 已验证：协议层单测（握手、大端长度、分片重组、迟到响应、坏帧、超长包、分阶段错误）
  与**端到端集成测试**（模拟代理 → `CmsisDapFactory::open` → `DAP_Info` 序列 → 成功打开探针），
  8/8 通过；GUI 侧 106 项单测通过，选择器 / 持久化 / CLI 接线均有回归测试。
- **真机验证通过（2026-11-10）**：设备断电重启后 `--network-check dap.local` 报
  「握手成功（代理版本 1.0）· vendor scope status 0」；`--connect --network dap.local`
  连续 **4/4 `[PASS]`**（`PY32F003x8 · Armv6m · SWD 10000 kHz ·
  SP=0x20002000 PC=0x08000419`）；`--chip-id` 读到 `DPIDR=0x0BC11477`、
  `CPUID=0x410CC601`（Cortex-M0+，与 Armv6m 一致）、`DBGMCU_IDCODE=0x60001000`、
  96 位 UID 与 55 KiB Flash 容量。完整**只读自检**
  `--selftest <固件> --network dap.local` 也是全绿：连接测试、4096 字节读回备份、
  1432 字节独立读回校验（`读回完全一致`）全部 `[PASS]`。
  **烧录 / 写入校验尚未在无线链路上实测**，但连接与 SWD 批量读链路已完整跑通。
  此前设备因异常断连卡死（TCP 3240 只在极短窗口开放、连上也不回握手），断电重启后恢复；
  若设备不回答 `DAP_Info(PacketSize)` 会自动回退到 1496 字节；`.local` 解析不了就改用设备 IP。

## 连接测试一直失败：DAPLink HID 固件死锁

「连接测试 / 连接 / 烧录」全部卡住，日志里出现 `hidapi error: WinError: WaitTimedOut`
或 `Command ID in response (0x0) does not match sent command ID`，且**换芯片型号、降速都无效**时，
通常不是芯片描述错了，而是 DAPLink 的 HID 端点固件死锁。判断方法：

1. 用 `--connect` 复现 GUI 同一条路径（含探针重新枚举与任务级重试），看错误链里的
   实际原因；`probe-rs` 与 `pyOCD` 都卡在 `DAP_Info` 即可确认是探针侧问题。
2. 检查是否有别的进程占着探针：`pyOCD` / OpenOCD / 另一个 probe-rs / 备份脚本。
   结束这些进程（例如残留的 `python3.13.exe` 备份任务）后重试。
3. 若仍然零响应，需要让探针**重新上电枚举**：直接重新插拔 USB 最可靠。
   本机受系统策略限制时 `pnputil /restart-device`、禁用/启用 HID 接口、
   `CM_Reenumerate_DevNode` 都可能返回 `CR_ACCESS_DENIED`；CDC 虚拟串口的
   1200bps/DTR 复位也不一定有效。对**外置 USB Hub**（本机 `VID_05E3&PID_0608`）
   的对应端口下发 `IOCTL_USB_HUB_CYCLE_PORT` 做电气复位可以等效于物理插拔。
   恢复后 `--connect` 会立即 `[PASS]`。

软件侧为此做的改进：

- 错误提示区分「探针固件无响应（HID 超时，需重新插拔）」和「被其它程序独占（关闭
  pyOCD 等）」，不再把超时笼统说成占用。用 `anyhow` 展开 `DebugProbeError` 的
  完整 cause chain，日志里能看到真正的 `WinError`。
- `open_probe()` 每次尝试都重新 `Lister::new()` / `list_all()`，最多 3 次（400/800 ms 退避），
  按稳定身份匹配探针；重新插拔后身份变化且只发现一个探针时自动改用它。
- 确认探针已死锁后返回 `ProbeUnresponsive`，该错误**不进入**任务级退避重试，
  避免把同一个无望的连接重复 9 次才报错。

## 自检与诊断

```bash
# 只读自检：探针枚举、连接、读回、独立校验（不写入 Flash）
probe-rs-studio.exe --selftest <firmware.hex> PY32F003x8

# 完整自检，含真实烧录与独立读回校验
probe-rs-studio.exe --selftest <firmware.hex> PY32F003x8 --write

# 自检默认复用 probe-rs-studio.json 里保存的探针身份与 SWD 速度（与 GUI 完全一致），
# 可用 --speed 临时覆盖，便于对比不同时钟下的稳定性
probe-rs-studio.exe --selftest <firmware.hex> --write --speed 4000

# 打印调试探针热插拔事件，N 秒内可插拔 USB 观察
probe-rs-studio.exe --watch 8

# 芯片库检查（无需硬件）/ 统一入口导入 pack 或 FLM
probe-rs-studio.exe --chips
probe-rs-studio.exe --load <厂商.pack.zip>
probe-rs-studio.exe --load <厂商.FLM> --chip <型号> --core M4

# 列出当前在线串口（含调试器虚拟串口），无需硬件连接
probe-rs-studio.exe --ports

# TX/RX 短接后做回环自检：走 GUI 同一套 Port::read/write，打印驱动队列与收到的字节
probe-rs-studio.exe --loopback COM10
probe-rs-studio.exe --loopback COM10 --baud 9600 --text "AT\r\n"

# 选项字节只读自检：打印 16 字节原始值、反码校验结果与逐字段解析（不写入）
probe-rs-studio.exe --option-bytes
probe-rs-studio.exe --option-bytes PY32F003x8 --ob-base 0x1FFF0E80 --speed 10000

# 芯片 ID 只读自检：打印 DPIDR / ROM Table / CPUID / IDCODE / UID / Flash 容量
probe-rs-studio.exe --chip-id
probe-rs-studio.exe --chip-id PY32F003x8 --speed 10000
probe-rs-studio.exe --chip-id MM32L373PF --id-addr 0x40015800

# 无界面连接测试：复用配置里的探针身份与速度，走 GUI「连接测试」同一路径
probe-rs-studio.exe --connect
probe-rs-studio.exe --connect MM32L373PF --speed 10000

# 启动 GUI 并直接打开「选项字节设置」/「目标芯片 ID」窗口
probe-rs-studio.exe --ob-open
probe-rs-studio.exe --id-open

# 单元测试（hex 查看器、pack 转换、串口枚举 / 参数 / HEX 解析、定时发送、缩放、快捷输入、
# 选项字节编解码与反码、芯片 ID 解码与预设、探针任务重试策略、矢量图标、主题配色、
# 热插拔 diff、Registry 解析 OPT 算法）
cargo test --release --bin probe-rs-studio
```

自检不启动 GUI 窗口；release 构建为 Windows 子系统程序，输出需重定向到文件查看。

## 已验证硬件

- 探针：DAPLink `c251:f001`，序列号 `LU_2022_8888`，CMSIS-DAP v1 HID（64 字节报告）
- 目标：PY32F003x8（Armv6-M，64KB Flash / 8KB RAM）
- 结果：烧录约 2 秒，库内校验 + 独立读回 SHA256 一致 + 复位成功
- **烧录稳定性（v0.2.0）**：修复前 10 次连接仅 6 次成功，烧录常在 Erase/Program 阶段报
  `Command ID in response (0x0) does not match sent command ID (Transfer - 0x5)`；修复后
  `--option-bytes` 连续 10 次连接 **10/10 成功**，完整 `--selftest --write`
  （备份 → 烧录 4111 字节 → 独立读回 SHA256 比对）零重试通过，`[PASS] 读回完全一致`。
- DAPLink 虚拟串口 `COM10`（`c251:f001`，`MI_00`）：识别为「调试器虚拟串口」；**TX/RX 短接回环**
  115200 8N1 下文本 / HEX / 彩色 emoji 发送与接收、定时发送、Ctrl+滚轮缩放全部验证通过
- 芯片 ID（v0.2.1）：窗口渲染、寄存器解码与内核一致性单测（现共 94 项）、CLI 接线已验证；
  采集路径与 `--option-bytes` 共用连接层，为纯只读。若探针被其它调试工具独占，
  `DAP_Info` 会返回 `WinError: WaitTimedOut`；释放探针或重新插拔 USB 后执行 `--chip-id` 即可核对。
- **MM32L373PF**（MindMotion DFP 扩展包，Cortex-M3 / 128KB Flash，10000 kHz）：
  死锁恢复后 `--connect` 连续 3 次 `[PASS]`，向量表 `SP=0x20000900 PC=0x080032D5`；
  `--chip-id` 真机读到 `DPIDR=0x2BA01477`、ROM Table `0xE00FF000=0x04C3`、
  `CPUID=0x412FC231`（ARM Cortex-M3 / ARMv7-M，与芯片描述 `Armv7m` 一致，无告警）；
  `--chip-id --id-addr 0xE0042000` 自定义地址读到 `0x00000000`。
  故意误选 `PY32F003x8` 时正确给出 `[WARN] … 与所选芯片声明的 Armv6m 不一致`。
- **GUI 交互（本轮）**：`cargo test --release --offline` **96 passed / 0 failed**；截图确认
  「开始烧录」固定在「当前会话」顶部（滚动区之外且全界面唯一）、纯「✕」关闭按钮
  （左栏标题行 + 右侧卡片右上角）、日志台「复制全部」均正常渲染；串口页移除独立信息条，
  接收区标题行集成端口 / 参数 / 计数与「清空接收 / 打开串口」，发送区标题行集成定时发送 /
  清空输入 / CRLF / HEX / 快捷输入，右下角为圆形矢量朝上纸飞机发送按钮，两区卡片内只剩内容区，
  控件无重叠或截断，接收区高度随之增加。启用 `clipboard` feature 后点击「复制全部」，
  从系统剪贴板读回 **516 字节**日志文本（修复前为空），日志台同时追加
  「已复制 N 条日志到剪贴板」反馈。
- 选项字节：`--option-bytes` 在 10000 kHz 下只读读出 `0x1FFF0E80` 的 16 字节，
  反码校验通过，字段解析与厂商 SDK 位定义一致；GUI 窗口渲染、字段编辑与风险护栏已验证。
  **写入路径未做真机执行验证**（避免误锁芯片），见上文说明。
- Holychip HC32F4XX：已完成芯片描述解析与 Flash 算法完整性验证（单元测试覆盖 3 个型号、
  指令 blob、入口偏移、NVM/RAM 区间）。本机暂无该系列硬件，**未做真机烧录验证**。

若所有 DAP 命令零超时响应，多为调试器固件卡死，物理拔插 USB 后恢复；软件层重启设备通常被系统拒绝。

## 目录结构

```
src/main.rs       入口、窗口配置、--selftest / --watch / --chips / --load / --ports / --loopback / --option-bytes / --chip-id / --connect / --network-check / --network / --ob-open / --id-open 分派
src/app.rs        会话侧栏（顶部固定烧录按钮 / 固件卸载 / 快捷操作）、16 进制查看器（固件信息卡片左右分栏：左栏详情 + 右栏最近固件紧凑按钮组、右上角纯「✕」）、串口终端（接收区 / 发送区标题行集成控制、定时发送 / 彩色 emoji 快捷输入 / Ctrl+滚轮缩放）、日志剪贴板复制与右键菜单、选项字节与芯片 ID 窗口、主题、配置持久化、热插拔事件、统一的「加载芯片支持」按钮
src/backend.rs    probe-rs 库调用、工作线程、烧录/校验/备份、芯片 ID 只读采集（DPIDR/ROM Table/CPUID/IDCODE/UID）、内核一致性核对、网络探针选择器解析与直连及分阶段诊断（解析/TCP/握手/端口占用）、探针重新枚举与死锁识别、选项字节读取与 OPT.FLM 写入、探针与串口监视、串口会话线程、芯片描述导入
src/chip_id.rs    芯片 ID 寄存器预设与 CPUID / DPIDR / IDCODE / UID / FlashSize 解码、内核型号与芯片描述一致性判定、报告格式化（纯逻辑，可单测）
src/option_bytes.rs  选项字节 16 字节镜像、字段编解码、反码维护、变更 diff 与安全校验（纯逻辑，可单测）
src/pack.rs       CMSIS-Pack 转换器：ZIP / pdsc XML / ELF32 FLM / target YAML（纯 Rust）
src/firmware.rs   HEX / ELF / BIN 解析与 SHA256
src/serial.rs     纯 Rust Win32 FFI 串口枚举、线路参数、独占打开、非阻塞收发与队列诊断
src/theme.rs      深浅两套配色、CJK 与彩色 emoji 系统字体 fallback（emoji 排在 CJK 之前）
src/icons.rs      SVG 子集解析 + epaint 矢量绘制
assets/icons/     6 个 SVG 图标 + 窗口图标 RGBA + 多尺寸 exe 图标 ICO
assets/app-icon.rc / .res  Windows exe 图标资源脚本与预编译资源（build.rs 链接）
build.rs          把 app-icon.res 链接进 Windows exe（非 Windows 目标跳过）
assets/PY32F0xx_OPT.yaml  由 PY32F0xx_OPT.FLM 生成的内置选项字节算法描述（编译进二进制）
targets/          CMSIS-Pack 转换出的扩展芯片描述 YAML
tests/fixtures/   精简 pack 与 .pack.zip 测试样本
tools/gen_icons.py  由统一几何定义生成 SVG、窗口图标 RGBA 与多尺寸 ICO
tools/flm_to_yaml.py 生成 bundled HC32F4XX.yaml 的离线脚本（历史用途）
```
