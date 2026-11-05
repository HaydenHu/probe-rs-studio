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
- 内置约 4700 个芯片型号，另附 Holychip HC32F4XX 系列（由 CMSIS-Pack 转换）；可搜索、查看内存布局与 Flash 算法，支持外部 `.yaml` 芯片描述
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

## 扩展芯片包

内置芯片描述之外，程序还会加载由 CMSIS-Pack 转换出的 probe-rs target YAML：

| 来源 | 位置 | 说明 |
| --- | --- | --- |
| 随程序编译 | `targets/HC32F4XX.yaml` | 通过 `include_str!` 嵌入 exe，单个可执行文件即可运行 |
| 用户扩展 | exe 同目录 `targets/*.yaml`（或运行目录 `targets/*.yaml`） | 启动时自动扫描，放进去即生效，无需重新编译 |
| 临时指定 | 设置页「外部芯片描述 (.yaml)」 | 单文件，优先级最高，可覆盖同名 family |

当前附带 `Holychip.HC32F4XX.1.0.7.pack` 生成的 3 个型号：`HC32F401BRCX`（256 KiB Flash）、
`HC32F401BREX`（512 KiB）、`HC32F401BRGX`（1024 KiB），Cortex-M4 / 192 KiB RAM，
每个型号带各自容量的 Flash 算法。芯片库中这些条目会标注「· 扩展包」。

添加其它系列时，在 probe-rs 源码中用官方工具转换：

```bash
cargo run -p target-gen -- pack <厂商.pack> <输出目录>
```

把生成的 YAML 放进 `targets/` 就会被自动加载；若希望像 HC32F4XX 一样编进 exe，
把文件加入 `src/backend.rs` 的 `BUNDLED_FAMILIES` 常量即可。

> 注意：`target-gen pack` 会把代码相同、仅 `FlashDevice` 参数不同的算法去重。
> 本 pack 的 3 个 FLM 恰好属于这种情况，官方输出漏掉了 256K 算法，让
> `HC32F401BRCX` 误用 512K 算法。仓库里的 `HC32F4XX.yaml` 因此改由
> `tools/flm_to_yaml.py` 生成：它直接解析解包后的 `.FLM`，按同样规则为每个容量
> 生成独立算法。脚本输出与官方输出在 512K / 1024K 上逐字段一致（指令 blob、
> 各入口偏移、超时、扇区）。

## 自检与诊断

```bash
# 只读自检：探针枚举、连接、读回、独立校验（不写入 Flash）
probe-rs-studio.exe --selftest <firmware.hex> PY32F003x8

# 完整自检，含真实烧录与复位
probe-rs-studio.exe --selftest <firmware.hex> PY32F003x8 --write

# 打印调试探针热插拔事件，N 秒内可插拔 USB 观察
probe-rs-studio.exe --watch 8

# 单元测试（矢量图标解析、多边形三角化、主题配色、热插拔比较与 diff）
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
src/main.rs       入口、窗口配置、--selftest / --watch 分派
src/app.rs        主题、配置持久化、热插拔事件处理
src/backend.rs    probe-rs 库调用、工作线程、烧录/校验/备份、设备监视
src/firmware.rs   HEX / ELF / BIN 解析与 SHA256
src/theme.rs      深浅两套配色、CJK 字体安装
src/icons.rs      SVG 子集解析 + epaint 矢量绘制
assets/icons/     6 个 SVG 图标 + Windows 任务栏图标 RGBA
targets/          CMSIS-Pack 转换出的扩展芯片描述 YAML
tools/gen_icons.py  由统一几何定义生成 SVG 与 OS 图标
tools/flm_to_yaml.py 从解包后的 CMSIS-Pack .FLM 离线生成 target YAML
```
