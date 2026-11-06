//! Serial-port discovery for the "串口识别" page.
//!
//! Pure Rust, no external crate and no child process: a handful of Win32 FFI
//! calls read the registry and the PnP device information set directly.
//!
//! `HKLM\HARDWARE\DEVICEMAP\SERIALCOMM` only lists ports that are **currently
//! attached**, which makes it the authoritative source for "connected" ports.
//! SetupAPI's PORTS class (with DIGCF_PRESENT) adds the friendly name, hardware
//! ids and the USB VID/PID, so a debugger's built-in USB-to-serial (the virtual
//! COM port shipped by DAPLink / CMSIS-DAP) can be recognised explicitly.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortKind {
    /// Virtual COM port provided by a debug probe (DAPLink / CMSIS-DAP / ...).
    DebugVcp,
    UsbSerial,
    Bluetooth,
    Legacy,
    Other,
}

impl PortKind {
    pub fn label(self) -> &'static str {
        match self {
            PortKind::DebugVcp => "调试器虚拟串口",
            PortKind::UsbSerial => "USB 串口",
            PortKind::Bluetooth => "蓝牙串口",
            PortKind::Legacy => "板载 / 传统串口",
            PortKind::Other => "其它串口",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SerialPort {
    /// e.g. `COM10`
    pub name: String,
    /// e.g. `\Device\USBSER000`, from SERIALCOMM's value name.
    pub device_object: String,
    pub description: String,
    pub instance_id: String,
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    /// USB interface number (`MI_00`), when the port is part of a composite
    /// device such as a DAPLink with both CMSIS-DAP and CDC ACM.
    pub interface: Option<u8>,
    /// `CreateFileW` with zero desired access succeeded, i.e. the port is not
    /// exclusively held by another application.
    pub openable: bool,
    pub kind: PortKind,
}

impl SerialPort {
    pub fn vid_pid(&self) -> Option<String> {
        Some(format!(
            "{:04x}:{:04x}",
            self.vid?,
            self.pid?
        ))
    }

    pub fn is_debug_vcp(&self) -> bool {
        self.kind == PortKind::DebugVcp
    }

    /// Cheap change-detection key used by the polling watcher.
    pub fn signature(&self) -> String {
        format!(
            "{}|{}|{}|{}|{:?}|{:?}|{}|{}",
            self.name,
            self.device_object,
            self.description,
            self.instance_id,
            self.vid,
            self.pid,
            self.openable,
            self.kind.label()
        )
    }
}

/// Pull the `COMn` token out of a friendly name like
/// `USB 串行设备 (COM10)` or `mbed Serial Port (COM7)`.
pub fn parse_com(s: &str) -> Option<String> {
    let upper = s.to_ascii_uppercase();
    let b = upper.as_bytes();
    let mut i = 0;
    while i + 3 <= b.len() {
        if &b[i..i + 3] == b"COM" {
            let mut j = i + 3;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 3 {
                return Some(upper[i..j].to_string());
            }
        }
        i += 1;
    }
    None
}

fn hex_after(s: &str, key: &str) -> Option<u16> {
    let upper = s.to_ascii_uppercase();
    let i = upper.find(key)? + key.len();
    let tail = &upper[i..];
    let n = tail.chars().take_while(|c| c.is_ascii_hexdigit()).count();
    if n == 0 {
        return None;
    }
    u16::from_str_radix(&tail[..n], 16).ok()
}

/// Parse `VID_xxxx` / `PID_xxxx` out of a hardware id or device instance id.
pub fn parse_vid_pid(s: &str) -> (Option<u16>, Option<u16>) {
    (hex_after(s, "VID_"), hex_after(s, "PID_"))
}

/// Parse the `MI_xx` interface number of a composite USB device.
pub fn parse_interface(s: &str) -> Option<u8> {
    hex_after(s, "MI_").map(|v| v as u8)
}

/// Known debugger VID:PID pairs that expose a virtual COM port.
/// 0D28:0204 is the ARM/mbed DAPLink CMSIS-DAP VCP, C251:F001 is the
/// DAPLink used by this project's test board, 1A86:8012 is WCH-Link,
/// 0483:374B is ST-LINK/V2-1 VCP and 1366:0101/0105 are J-Link VCOM.
const DEBUG_VCP_PAIRS: &[(u16, u16)] = &[
    (0x0D28, 0x0204),
    (0x0D28, 0x0203),
    (0xC251, 0xF001),
    (0x1A86, 0x8012),
    (0x0483, 0x374B),
    (0x1366, 0x0101),
    (0x1366, 0x0105),
];

const DEBUG_VCP_WORDS: &[&str] = &[
    "DAPLINK",
    "CMSIS-DAP",
    "CMSIS_DAP",
    "MBED",
    "WCH-LINK",
    "ST-LINK",
    "J-LINK",
    "PYOCD",
    "VIRTUAL COM",
];

/// Classify a port from its USB ids and all descriptive strings.
pub fn classify(
    vid: Option<u16>,
    pid: Option<u16>,
    text: &str,
    device_object: &str,
) -> PortKind {
    let upper = text.to_ascii_uppercase();
    if DEBUG_VCP_WORDS.iter().any(|w| upper.contains(w)) {
        return PortKind::DebugVcp;
    }
    if let (Some(v), Some(p)) = (vid, pid) {
        if DEBUG_VCP_PAIRS.iter().any(|&(a, b)| a == v && b == p) {
            return PortKind::DebugVcp;
        }
    }
    let dev = device_object.to_ascii_uppercase();
    if upper.contains("BLUETOOTH") || dev.contains("BTH") {
        return PortKind::Bluetooth;
    }
    if dev.contains("USBSER") || vid.is_some() {
        return PortKind::UsbSerial;
    }
    if dev.contains("SERIAL") || upper.contains("ACPI") || upper.contains("PNP0501") {
        return PortKind::Legacy;
    }
    PortKind::Other
}

/// Natural ordering for COM names: COM2 before COM10.
pub fn com_number(name: &str) -> u64 {
    name.trim_start_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .unwrap_or(u64::MAX)
}

// ---------------------------------------------------------------------------
// Serial-port configuration and I/O (still pure Rust + Win32 FFI)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Parity {
    None,
    Odd,
    Even,
    Mark,
    Space,
}

impl Parity {
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "无 (N)",
            Self::Odd => "奇校验 (O)",
            Self::Even => "偶校验 (E)",
            Self::Mark => "Mark (M)",
            Self::Space => "Space (S)",
        }
    }

    fn code(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Odd => 1,
            Self::Even => 2,
            Self::Mark => 3,
            Self::Space => 4,
        }
    }

    fn letter(self) -> char {
        match self {
            Self::None => 'N',
            Self::Odd => 'O',
            Self::Even => 'E',
            Self::Mark => 'M',
            Self::Space => 'S',
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StopBits {
    One,
    OneFive,
    Two,
}

impl StopBits {
    pub fn label(self) -> &'static str {
        match self {
            Self::One => "1",
            Self::OneFive => "1.5",
            Self::Two => "2",
        }
    }

    fn code(self) -> u8 {
        match self {
            Self::One => 0,
            Self::OneFive => 1,
            Self::Two => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FlowControl {
    None,
    XonXoff,
    RtsCts,
}

impl FlowControl {
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "无",
            Self::XonXoff => "XON / XOFF",
            Self::RtsCts => "RTS / CTS",
        }
    }
}

pub const BAUD_PRESETS: &[u32] = &[
    1200, 2400, 4800, 9600, 19200, 38400, 57600, 115200, 230400, 460800, 921600,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SerialSettings {
    pub baud: u32,
    pub data_bits: u8,
    pub parity: Parity,
    pub stop_bits: StopBits,
    pub flow: FlowControl,
}

impl Default for SerialSettings {
    fn default() -> Self {
        Self {
            baud: 115200,
            data_bits: 8,
            parity: Parity::None,
            stop_bits: StopBits::One,
            flow: FlowControl::None,
        }
    }
}

impl SerialSettings {
    pub fn validate(&self) -> Result<(), String> {
        if self.baud == 0 {
            return Err("波特率必须大于 0".to_owned());
        }
        if !(5..=8).contains(&self.data_bits) {
            return Err("数据位只能是 5 / 6 / 7 / 8".to_owned());
        }
        Ok(())
    }

    /// Short terminal-style label, e.g. `115200 8N1`.
    pub fn label(&self) -> String {
        format!(
            "{} {}{}{}",
            self.baud,
            self.data_bits,
            self.parity.letter(),
            self.stop_bits.label()
        )
    }
}

/// DCB bit-field: fBinary (bit0) + DTR enable (bits4-5) + RTS control
/// (bits12-13), plus the flow-control bits. Kept pure so it is unit-testable.
fn dcb_flags(flow: FlowControl) -> u32 {
    const BINARY: u32 = 1 << 0;
    const OUT_X_CTS_FLOW: u32 = 1 << 2;
    const DTR_ENABLE: u32 = 1 << 4;
    const OUT_X: u32 = 1 << 8;
    const IN_X: u32 = 1 << 9;
    const RTS_ENABLE: u32 = 1 << 12;
    const RTS_HANDSHAKE: u32 = 2 << 12;
    match flow {
        FlowControl::None => BINARY | DTR_ENABLE | RTS_ENABLE,
        FlowControl::XonXoff => BINARY | DTR_ENABLE | RTS_ENABLE | OUT_X | IN_X,
        FlowControl::RtsCts => BINARY | DTR_ENABLE | RTS_HANDSHAKE | OUT_X_CTS_FLOW,
    }
}

/// Parse a HEX-send text box: whitespace, commas, dashes, colons and `0x`
/// prefixes are ignored, so `48 65 6C 6C 6F` and `0x48,0x65` both work.
pub fn parse_hex_text(text: &str) -> Result<Vec<u8>, String> {
    let mut digits = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() || matches!(b, b',' | b'-' | b':' | b';' | b'_') {
            i += 1;
            continue;
        }
        if (b == b'0')
            && i + 1 < bytes.len()
            && (bytes[i + 1] == b'x' || bytes[i + 1] == b'X')
        {
            i += 2;
            continue;
        }
        digits.push(b);
        i += 1;
    }
    if digits.is_empty() {
        return Ok(Vec::new());
    }
    if digits.len() % 2 != 0 {
        return Err("HEX 字节数必须为偶数（每两个十六进制字符一组）".to_owned());
    }
    let nibble = |c: u8| -> Result<u8, String> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            _ => Err(format!("非法十六进制字符 '{}'", char::from(c))),
        }
    };
    let mut out = Vec::with_capacity(digits.len() / 2);
    for pair in digits.chunks_exact(2) {
        out.push((nibble(pair[0])? << 4) | nibble(pair[1])?);
    }
    Ok(out)
}

/// An open COM port. `Drop` closes the Win32 handle, so the worker thread
/// simply ends its loop when the session is closed.
pub struct Port {
    handle: isize,
}

// The raw HANDLE is only ever used by the single worker thread that owns it.
unsafe impl Send for Port {}

impl Drop for Port {
    fn drop(&mut self) {
        #[cfg(windows)]
        windows::close_raw(self.handle);
    }
}

impl Port {
    /// Non-blocking read: returns whatever bytes are already in the driver's
    /// input queue (never waits for more).
    pub fn read(&self) -> anyhow::Result<Vec<u8>> {
        #[cfg(windows)]
        {
            windows::read_raw(self.handle).map_err(anyhow::Error::msg)
        }
        #[cfg(not(windows))]
        {
            anyhow::bail!("串口 I/O 仅支持 Windows")
        }
    }

    pub fn write(&self, data: &[u8]) -> anyhow::Result<usize> {
        #[cfg(windows)]
        {
            windows::write_raw(self.handle, data).map_err(anyhow::Error::msg)
        }
        #[cfg(not(windows))]
        {
            let _ = data;
            anyhow::bail!("串口 I/O 仅支持 Windows")
        }
    }
}

pub fn open_port(name: &str, cfg: SerialSettings) -> anyhow::Result<Port> {
    cfg.validate().map_err(anyhow::Error::msg)?;
    let number = name
        .strip_prefix("COM")
        .or_else(|| name.strip_prefix("com"))
        .and_then(|s| s.parse::<u32>().ok());
    if number.is_none() {
        anyhow::bail!("不是有效的 COM 端口名: {name}");
    }
    #[cfg(windows)]
    {
        let handle = windows::open_raw(name, cfg).map_err(anyhow::Error::msg)?;
        Ok(Port { handle })
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!("串口 I/O 仅支持 Windows")
    }
}

/// Enumerate the currently attached serial ports. Returns an empty list on
/// non-Windows platforms and when the registry key cannot be opened.
pub fn enumerate() -> Vec<SerialPort> {
    #[cfg(windows)]
    {
        windows::enumerate()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// Win32 implementation
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod windows {
    use super::*;

    type Handle = isize;
    type Hkey = isize;

    const HKEY_LOCAL_MACHINE: Hkey = 0x8000_0002;
    const KEY_READ: u32 = 0x0002_0019;
    const ERROR_NO_MORE_ITEMS: i32 = 259;
    const DIGCF_PRESENT: u32 = 0x0000_0002;
    const INVALID_HANDLE_VALUE: Handle = -1;
    const OPEN_EXISTING: u32 = 3;
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
    const SPDRP_DEVICEDESC: u32 = 0;
    const SPDRP_HARDWAREID: u32 = 1;
    const SPDRP_FRIENDLYNAME: u32 = 12;

    #[repr(C)]
    #[derive(Clone, Copy)]
    #[allow(dead_code)] // layout is consumed by the Win32 API, not by Rust code
    struct Guid {
        data1: u32,
        data2: u16,
        data3: u16,
        data4: [u8; 8],
    }

    /// GUID_DEVCLASS_PORTS {4D36E978-E325-11CE-BFC1-08002BE10318}
    const GUID_DEVCLASS_PORTS: Guid = Guid {
        data1: 0x4D36_E978,
        data2: 0xE325,
        data3: 0x11CE,
        data4: [0xBF, 0xC1, 0x08, 0x00, 0x2B, 0xE1, 0x03, 0x18],
    };

    #[repr(C)]
    struct DevInfoData {
        cb_size: u32,
        class_guid: Guid,
        dev_inst: u32,
        reserved: usize,
    }

    #[link(name = "advapi32")]
    extern "system" {
        fn RegOpenKeyExW(
            key: Hkey,
            sub_key: *const u16,
            options: u32,
            desired: u32,
            result: *mut Hkey,
        ) -> i32;
        fn RegEnumValueW(
            key: Hkey,
            index: u32,
            value_name: *mut u16,
            value_name_len: *mut u32,
            reserved: *mut u32,
            value_type: *mut u32,
            data: *mut u8,
            data_len: *mut u32,
        ) -> i32;
        fn RegCloseKey(key: Hkey) -> i32;
    }

    #[link(name = "setupapi")]
    extern "system" {
        fn SetupDiGetClassDevsW(
            class_guid: *const Guid,
            enumerator: *const u16,
            parent: Handle,
            flags: u32,
        ) -> Handle;
        fn SetupDiEnumDeviceInfo(
            set: Handle,
            index: u32,
            data: *mut DevInfoData,
        ) -> i32;
        fn SetupDiGetDeviceRegistryPropertyW(
            set: Handle,
            data: *const DevInfoData,
            property: u32,
            reg_type: *mut u32,
            buffer: *mut u8,
            buffer_size: u32,
            required: *mut u32,
        ) -> i32;
        fn SetupDiGetDeviceInstanceIdW(
            set: Handle,
            data: *const DevInfoData,
            id: *mut u16,
            id_size: u32,
            required: *mut u32,
        ) -> i32;
        fn SetupDiDestroyDeviceInfoList(set: Handle) -> i32;
    }

    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const PURGE_ALL: u32 = 0x0000_000F;
    const MAX_DWORD: u32 = u32::MAX;

    /// Win32 DCB, 28 bytes. The bit-field word is built by `dcb_flags`.
    #[repr(C)]
    #[allow(dead_code)]
    struct Dcb {
        dc_blength: u32,
        baud_rate: u32,
        flags: u32,
        w_reserved: u16,
        xon_lim: u16,
        xoff_lim: u16,
        byte_size: u8,
        parity: u8,
        stop_bits: u8,
        xon_char: u8,
        xoff_char: u8,
        error_char: u8,
        eof_char: u8,
        evt_char: u8,
        w_reserved1: u16,
    }

    #[repr(C)]
    #[allow(dead_code)]
    struct ComStat {
        flags: u32,
        cb_in_que: u32,
        cb_out_que: u32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileW(
            name: *const u16,
            desired_access: u32,
            share_mode: u32,
            security: *mut std::ffi::c_void,
            creation: u32,
            flags: u32,
            template: Handle,
        ) -> Handle;
        fn CloseHandle(handle: Handle) -> i32;
        fn GetCommState(handle: Handle, dcb: *mut Dcb) -> i32;
        fn SetCommState(handle: Handle, dcb: *const Dcb) -> i32;
        fn SetCommTimeouts(handle: Handle, timeouts: *const u32) -> i32;
        fn SetupComm(handle: Handle, in_queue: u32, out_queue: u32) -> i32;
        fn PurgeComm(handle: Handle, flags: u32) -> i32;
        fn ClearCommError(handle: Handle, errors: *mut u32, stat: *mut ComStat) -> i32;
        fn ReadFile(
            handle: Handle,
            buffer: *mut u8,
            bytes_to_read: u32,
            bytes_read: *mut u32,
            overlapped: *mut std::ffi::c_void,
        ) -> i32;
        fn WriteFile(
            handle: Handle,
            buffer: *const u8,
            bytes_to_write: u32,
            bytes_written: *mut u32,
            overlapped: *mut std::ffi::c_void,
        ) -> i32;
        fn GetLastError() -> u32;
    }

    fn last_error(what: &str) -> String {
        let code = unsafe { GetLastError() };
        format!("{what} 失败 (GetLastError={code})")
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn from_wide(buf: &[u16]) -> String {
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len])
    }

    fn bytes_to_wide(bytes: &[u8]) -> Vec<u16> {
        bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect()
    }

    /// (device object name, COM name) pairs for ports attached right now.
    fn serial_comm() -> Vec<(String, String)> {
        let mut key: Hkey = 0;
        let sub = wide(r"HARDWARE\DEVICEMAP\SERIALCOMM");
        let rc = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, sub.as_ptr(), 0, KEY_READ, &mut key) };
        if rc != 0 {
            return Vec::new();
        }
        let mut out = Vec::new();
        for index in 0..512u32 {
            let mut name = vec![0u16; 512];
            let mut name_len = name.len() as u32;
            let mut value_type: u32 = 0;
            let mut data = vec![0u8; 1024];
            let mut data_len = data.len() as u32;
            let rc = unsafe {
                RegEnumValueW(
                    key,
                    index,
                    name.as_mut_ptr(),
                    &mut name_len,
                    std::ptr::null_mut(),
                    &mut value_type,
                    data.as_mut_ptr(),
                    &mut data_len,
                )
            };
            if rc == ERROR_NO_MORE_ITEMS {
                break;
            }
            if rc != 0 {
                continue;
            }
            let device = from_wide(&name[..(name_len as usize).min(name.len())]);
            let com = from_wide(&bytes_to_wide(&data[..(data_len as usize).min(data.len())]));
            if let Some(com) = parse_com(&com) {
                out.push((device, com));
            }
        }
        unsafe {
            RegCloseKey(key);
        }
        out
    }

    #[derive(Default)]
    struct PnpMeta {
        description: String,
        instance_id: String,
        text: String,
    }

    fn device_property(set: Handle, dev: &DevInfoData, property: u32) -> Option<Vec<u16>> {
        let mut required: u32 = 0;
        let mut reg_type: u32 = 0;
        unsafe {
            SetupDiGetDeviceRegistryPropertyW(
                set,
                dev,
                property,
                &mut reg_type,
                std::ptr::null_mut(),
                0,
                &mut required,
            );
        }
        if required == 0 {
            return None;
        }
        let mut buffer = vec![0u8; required as usize + 2];
        let ok = unsafe {
            SetupDiGetDeviceRegistryPropertyW(
                set,
                dev,
                property,
                &mut reg_type,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return None;
        }
        Some(bytes_to_wide(&buffer))
    }

    fn multi_string(units: &[u16]) -> Vec<String> {
        let mut out = Vec::new();
        let mut start = 0;
        for (i, &c) in units.iter().enumerate() {
            if c == 0 {
                if i == start {
                    break;
                }
                out.push(from_wide(&units[start..i]));
                start = i + 1;
            }
        }
        out
    }

    fn instance_id(set: Handle, dev: &DevInfoData) -> String {
        let mut required: u32 = 0;
        unsafe {
            SetupDiGetDeviceInstanceIdW(set, dev, std::ptr::null_mut(), 0, &mut required);
        }
        if required == 0 {
            return String::new();
        }
        let mut buffer = vec![0u16; required as usize + 1];
        let ok = unsafe {
            SetupDiGetDeviceInstanceIdW(
                set,
                dev,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return String::new();
        }
        from_wide(&buffer)
    }

    /// Metadata for present PORTS-class devices, keyed by COM name.
    fn pnp_ports() -> std::collections::HashMap<String, PnpMeta> {
        let mut map = std::collections::HashMap::new();
        let set = unsafe {
            SetupDiGetClassDevsW(
                &GUID_DEVCLASS_PORTS,
                std::ptr::null(),
                0,
                DIGCF_PRESENT,
            )
        };
        if set == 0 || set == INVALID_HANDLE_VALUE {
            return map;
        }
        for index in 0..512u32 {
            let mut dev: DevInfoData = unsafe { std::mem::zeroed() };
            dev.cb_size = std::mem::size_of::<DevInfoData>() as u32;
            if unsafe { SetupDiEnumDeviceInfo(set, index, &mut dev) } == 0 {
                break;
            }
            let friendly = device_property(set, &dev, SPDRP_FRIENDLYNAME)
                .map(|u| from_wide(&u))
                .unwrap_or_default();
            let desc = device_property(set, &dev, SPDRP_DEVICEDESC)
                .map(|u| from_wide(&u))
                .unwrap_or_default();
            let hardware = device_property(set, &dev, SPDRP_HARDWAREID)
                .map(|u| multi_string(&u).join(" "))
                .unwrap_or_default();
            let iid = instance_id(set, &dev);
            let com = parse_com(&friendly)
                .or_else(|| parse_com(&iid))
                .or_else(|| parse_com(&hardware));
            let Some(com) = com else { continue };
            let mut text = friendly.clone();
            text.push(' ');
            text.push_str(&desc);
            text.push(' ');
            text.push_str(&hardware);
            text.push(' ');
            text.push_str(&iid);
            map.insert(
                com,
                PnpMeta {
                    description: if friendly.is_empty() { desc } else { friendly },
                    instance_id: iid,
                    text,
                },
            );
        }
        unsafe {
            SetupDiDestroyDeviceInfoList(set);
        }
        map
    }

    fn is_openable(com: &str) -> bool {
        let path = wide(&format!("\\\\.\\{com}"));
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                0,
                0,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                0,
            )
        };
        if handle == 0 || handle == INVALID_HANDLE_VALUE {
            return false;
        }
        unsafe {
            CloseHandle(handle);
        }
        true
    }

    pub(super) fn open_raw(name: &str, cfg: SerialSettings) -> Result<Handle, String> {
        let path = wide(&format!("\\\\.\\{name}"));
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0, // COM ports cannot be shared
                std::ptr::null_mut(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                0,
            )
        };
        if handle == 0 || handle == INVALID_HANDLE_VALUE {
            return Err(last_error(&format!("打开 {name}")));
        }
        let fail = |what: &str| -> String {
            let msg = last_error(what);
            unsafe {
                CloseHandle(handle);
            }
            msg
        };
        unsafe {
            SetupComm(handle, 8192, 8192);
            let mut dcb: Dcb = std::mem::zeroed();
            if GetCommState(handle, &mut dcb) == 0 {
                return Err(fail("GetCommState"));
            }
            dcb.dc_blength = std::mem::size_of::<Dcb>() as u32;
            dcb.baud_rate = cfg.baud;
            dcb.byte_size = cfg.data_bits;
            dcb.parity = cfg.parity.code();
            dcb.stop_bits = cfg.stop_bits.code();
            dcb.flags = dcb_flags(cfg.flow);
            dcb.xon_char = 0x11;
            dcb.xoff_char = 0x13;
            dcb.xon_lim = 128;
            dcb.xoff_lim = 128;
            if SetCommState(handle, &dcb) == 0 {
                return Err(fail("SetCommState"));
            }
            // MAXDWORD + zero totals: ReadFile returns immediately with the
            // bytes already queued, so the worker can poll without blocking.
            let timeouts = [MAX_DWORD, 0, 0, 0, 1000];
            if SetCommTimeouts(handle, timeouts.as_ptr()) == 0 {
                return Err(fail("SetCommTimeouts"));
            }
            PurgeComm(handle, PURGE_ALL);
        }
        Ok(handle)
    }

    pub(super) fn read_raw(handle: Handle) -> Result<Vec<u8>, String> {
        let mut errors = 0u32;
        let mut stat: ComStat = unsafe { std::mem::zeroed() };
        if unsafe { ClearCommError(handle, &mut errors, &mut stat) } == 0 {
            return Err(last_error("ClearCommError"));
        }
        let queued = stat.cb_in_que.min(4096);
        if queued == 0 {
            return Ok(Vec::new());
        }
        let mut buf = vec![0u8; queued as usize];
        let mut read = 0u32;
        let ok = unsafe {
            ReadFile(
                handle,
                buf.as_mut_ptr(),
                queued,
                &mut read,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(last_error("ReadFile"));
        }
        buf.truncate(read as usize);
        Ok(buf)
    }

    pub(super) fn write_raw(handle: Handle, data: &[u8]) -> Result<usize, String> {
        let mut written = 0u32;
        let ok = unsafe {
            WriteFile(
                handle,
                data.as_ptr(),
                data.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(last_error("WriteFile"));
        }
        Ok(written as usize)
    }

    pub(super) fn close_raw(handle: Handle) {
        unsafe {
            CloseHandle(handle);
        }
    }

    pub(super) fn enumerate() -> Vec<SerialPort> {
        let meta = pnp_ports();
        let mut ports: Vec<SerialPort> = serial_comm()
            .into_iter()
            .map(|(device_object, name)| {
                let m = meta.get(&name);
                let instance_id = m.map(|x| x.instance_id.clone()).unwrap_or_default();
                let description = m
                    .map(|x| x.description.clone())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| name.clone());
                let ids = m
                    .map(|x| x.text.clone())
                    .unwrap_or_else(|| device_object.clone());
                let (vid, pid) = parse_vid_pid(&ids);
                let text = format!("{description} {ids}");
                SerialPort {
                    openable: is_openable(&name),
                    kind: classify(vid, pid, &text, &device_object),
                    name,
                    device_object,
                    description,
                    instance_id,
                    vid,
                    pid,
                    interface: parse_interface(&ids),
                }
            })
            .collect();
        ports.sort_by_key(|p| (com_number(&p.name), p.name.clone()));
        ports
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn com_name_and_number_parsing() {
        assert_eq!(parse_com("USB 串行设备 (COM10)"), Some("COM10".to_owned()));
        assert_eq!(parse_com("mbed Serial Port (COM7)"), Some("COM7".to_owned()));
        assert_eq!(parse_com("通信端口 (com1)"), Some("COM1".to_owned()));
        assert_eq!(parse_com("no port here"), None);
        assert_eq!(com_number("COM2"), 2);
        assert!(com_number("COM2") < com_number("COM10"));
    }

    #[test]
    fn usb_ids_are_parsed_from_instance_paths() {
        let id = r"USB\VID_C251&PID_F001&MI_00\7&16D6494D&0&0000";
        assert_eq!(parse_vid_pid(id), (Some(0xC251), Some(0xF001)));
        assert_eq!(parse_interface(id), Some(0));
        let wch = r"USB\VID_1A86&PID_8012&MI_01\7&C6F6BA7&1&0001";
        assert_eq!(parse_vid_pid(wch), (Some(0x1A86), Some(0x8012)));
        assert_eq!(parse_interface(wch), Some(1));
        assert_eq!(parse_vid_pid(r"ACPI\PNP0501\0"), (None, None));
    }

    #[test]
    fn daplink_virtual_com_ports_are_recognised() {
        // The DAPLink attached to this project: USB VID C251 / PID F001, the
        // CDC interface is MI_00 of the very same composite USB device.
        let kind = classify(
            Some(0xC251),
            Some(0xF001),
            r"USB 串行设备 (COM10) USB\VID_C251&PID_F001&MI_00",
            r"\Device\USBSER000",
        );
        assert_eq!(kind, PortKind::DebugVcp);
        // ARM's official DAPLink VCP, and keyword-only detection.
        assert_eq!(
            classify(Some(0x0D28), Some(0x0204), "USB Serial Device (COM7)", ""),
            PortKind::DebugVcp
        );
        assert_eq!(
            classify(None, None, "DAPLink CMSIS-DAP VCP (COM3)", ""),
            PortKind::DebugVcp
        );
        assert_eq!(
            classify(Some(0x1A86), Some(0x7522), "USB-SERIAL CH340K (COM5)", r"\Device\USBSER001"),
            PortKind::UsbSerial
        );
        assert_eq!(
            classify(None, None, "通信端口 (COM1)", r"\Device\Serial0"),
            PortKind::Legacy
        );
        assert_eq!(
            classify(None, None, "Standard Serial over Bluetooth link (COM12)", r"\Device\BthModem0"),
            PortKind::Bluetooth
        );
    }

    #[test]
    fn signature_changes_when_a_port_is_added_or_occupied() {
        let mut port = SerialPort {
            name: "COM10".to_owned(),
            device_object: r"\Device\USBSER000".to_owned(),
            description: "USB 串行设备 (COM10)".to_owned(),
            instance_id: r"USB\VID_C251&PID_F001&MI_00\7&16D6494D&0&0000".to_owned(),
            vid: Some(0xC251),
            pid: Some(0xF001),
            interface: Some(0),
            openable: true,
            kind: PortKind::DebugVcp,
        };
        assert_eq!(port.vid_pid().as_deref(), Some("c251:f001"));
        assert!(port.is_debug_vcp());
        let before = port.signature();
        port.openable = false;
        assert_ne!(before, port.signature());
        port.openable = true;
        assert_eq!(before, port.signature());
    }

    #[test]
    fn default_settings_and_terminal_label() {
        let s = SerialSettings::default();
        assert_eq!(s.label(), "115200 8N1");
        assert_eq!(s.parity, Parity::None);
        assert_eq!(s.stop_bits, StopBits::One);
        assert!(s.validate().is_ok());
        assert!(SerialSettings { baud: 0, ..s }.validate().is_err());
        assert!(SerialSettings { data_bits: 9, ..s }.validate().is_err());
        assert_eq!(
            SerialSettings {
                baud: 9600,
                data_bits: 7,
                parity: Parity::Even,
                stop_bits: StopBits::Two,
                ..s
            }
            .label(),
            "9600 7E2"
        );
    }

    #[test]
    fn dcb_flow_control_bits_are_distinct() {
        let none = dcb_flags(FlowControl::None);
        let xon = dcb_flags(FlowControl::XonXoff);
        let rts = dcb_flags(FlowControl::RtsCts);
        assert_ne!(none, xon);
        assert_ne!(none, rts);
        assert_ne!(xon, rts);
        // fBinary must always be set, DTR always enabled.
        for flags in [none, xon, rts] {
            assert_ne!(flags & 0x0000_0001, 0);
            assert_ne!(flags & 0x0000_0010, 0);
        }
        assert_ne!(xon & (1 << 8), 0); // fOutX
        assert_ne!(xon & (1 << 9), 0); // fInX
        assert_ne!(rts & (1 << 2), 0); // fOutxCtsFlow
        assert_eq!((rts >> 12) & 0b11, 2); // RTS_CONTROL_HANDSHAKE
        assert_eq!((none >> 12) & 0b11, 1); // RTS_CONTROL_ENABLE
    }

    #[test]
    fn hex_send_text_accepts_separators_and_0x_prefix() {
        assert_eq!(parse_hex_text("48 65 6C 6C 6F").unwrap(), b"Hello".to_vec());
        assert_eq!(
            parse_hex_text("0x48, 0x65-6C:6C_6F").unwrap(),
            b"Hello".to_vec()
        );
        assert!(parse_hex_text("  \n ").unwrap().is_empty());
        assert!(parse_hex_text("4").is_err());
        assert!(parse_hex_text("GG").is_err());
    }

    #[test]
    fn opening_an_invalid_port_or_name_fails_cleanly() {
        assert!(open_port("NOT_A_PORT", SerialSettings::default()).is_err());
        assert!(open_port("COM7", SerialSettings { baud: 0, ..Default::default() }).is_err());
        #[cfg(windows)]
        assert!(open_port("COM98765", SerialSettings::default()).is_err());
    }
}
