# ProbeRS Studio

纯 Rust 编写的桌面固件烧录工作台。直接链接 `probe-rs` 库，不调用外部 CLI、不启动子进程、不依赖 WebView/Electron。

## 两个仓库

本项目的代码拆成两个独立仓库维护：

| 仓库 | 内容 |
| --- | --- |
| [HaydenHu/probe-rs](https://github.com/HaydenHu/probe-rs) | probe-rs 的 fork。`daplink-hid-fallback` 分支包含国产 DAPLink 兼容补丁；`master` 保持与官方上游同步 |
| [HaydenHu/probe-rs-studio](https://github.com/HaydenHu/probe-rs-studio) | 本仓库，GUI 烧录软件全部源码 |

本仓库 `Cargo.toml` 通过 git 依赖引用 fork 的 `daplink-hid-fallback` 分支，因此单独 clone 本仓库即可构建。

补丁内容：`find_packet_size()` 在 16 次 `DAP_Info(PacketSize)` 超时后，对 CMSIS-DAP v1 HID 设备回退到 HID 报告描述符给出的 `report_size`，而不是报 `NoPacketSize`。部分 DAPLink 固件不实现该命令。

## 功能

- 探针枚举、按设备身份选择、连接测试；USB 插入/拔出自动检测（1.5 秒轮询 + 去抖），烧录期间自动暂停轮询
- 内置约 4700 个芯片型号，另附 Holychip HC32F4XX 系列；**内置库没有的芯片用同一个「加载芯片支持…」入口导入厂商 `.pack` / `.pack.zip`、已解压目录，或只给一个 `.FLM` 算法**（按文件类型自动分流，纯 Rust 解析并永久保存），支持外部 `.yaml` 芯片描述
- HEX / ELF / BIN 固件解析（地址区间、记录数、SHA256），支持拖拽载入
- 一键烧录、库内 verify、独立读回 SHA256 比对、烧录前读回备份、复位运行、整片擦除（二次确认）
- 深浅双主题、SVG 矢量导航图标（运行时用 epaint 直接绘制，无位图纹理）、可折叠日志台、进度与状态栏
- 配置自动保存到可执行文件同目录的 `probe-rs-studio.json`

## 环境要求

- Rust **1.95+**（egui / eframe 0.37 的 MSRV 为 1.95）
- 支持 OpenGL 的桌面环境（egui / eframe 0.37 的 glow 后端，未启用 wgpu）
- Windows：运行时加载 `C:/Windows/Fonts/Deng.ttf`（或 `msyh.ttc` / `simhei.ttf`）作为中文字体回退；egui 内置字体不含 CJK 字形
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

## 自检与诊断

```bash
# 只读自检：探针枚举、连接、读回、独立校验（不写入 Flash）
probe-rs-studio.exe --selftest <firmware.hex> PY32F003x8

# 完整自检，含真实烧录与复位
probe-rs-studio.exe --selftest <firmware.hex> PY32F003x8 --write

# 打印调试探针热插拔事件，N 秒内可插拔 USB 观察
probe-rs-studio.exe --watch 8

# 芯片库检查（无需硬件）/ 统一入口导入 pack 或 FLM
probe-rs-studio.exe --chips
probe-rs-studio.exe --load <厂商.pack.zip>
probe-rs-studio.exe --load <厂商.FLM> --chip <型号> --core M4

# 单元测试（pack 转换、矢量图标、主题配色、热插拔 diff、Registry 解析）
cargo test --release --bin probe-rs-studio
```

自检不启动 GUI 窗口；release 构建为 Windows 子系统程序，输出需重定向到文件查看。

## 已验证硬件

- 探针：DAPLink `c251:f001`，序列号 `LU_2022_8888`，CMSIS-DAP v1 HID（64 字节报告）
- 目标：PY32F003x8（Armv6-M，64KB Flash / 8KB RAM）
- 结果：烧录约 2.54 秒，库内校验 + 独立读回 SHA256 一致 + 复位成功
- Holychip HC32F4XX：已完成芯片描述解析与 Flash 算法完整性验证（单元测试覆盖 3 个型号、
  指令 blob、入口偏移、NVM/RAM 区间）。本机暂无该系列硬件，**未做真机烧录验证**。

若所有 DAP 命令零超时响应，多为调试器固件卡死，物理拔插 USB 后恢复；软件层重启设备通常被系统拒绝。

## 目录结构

```
src/main.rs       入口、窗口配置、--selftest / --watch / --chips / --load 分派
src/app.rs        主题、配置持久化、热插拔事件处理、统一的「加载芯片支持」按钮
src/backend.rs    probe-rs 库调用、工作线程、烧录/校验/备份、设备监视、芯片描述导入
src/pack.rs       CMSIS-Pack 转换器：ZIP / pdsc XML / ELF32 FLM / target YAML（纯 Rust）
src/firmware.rs   HEX / ELF / BIN 解析与 SHA256
src/theme.rs      深浅两套配色、CJK 字体安装
src/icons.rs      SVG 子集解析 + epaint 矢量绘制
assets/icons/     6 个 SVG 图标 + Windows 任务栏图标 RGBA
targets/          CMSIS-Pack 转换出的扩展芯片描述 YAML
tests/fixtures/   精简 pack 与 .pack.zip 测试样本
tools/gen_icons.py  由统一几何定义生成 SVG 与 OS 图标
tools/flm_to_yaml.py 生成 bundled HC32F4XX.yaml 的离线脚本（历史用途）
```
