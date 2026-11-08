//! e1000e_diag: settings, reports and tests for the e1000e driver (the
//! e1000e_diag.device build), from the Shell. Put it in SYS:C.
//!
//! ```text
//! e1000e_diag                     settings, then the driver's report
//! e1000e_diag MTU=9000            jumbo frames (JUMBO=ON is 9000, OFF 1500)
//! e1000e_diag ITR=196             interrupt throttling interval (x 256 ns)
//! e1000e_diag BATCH=OFF           one speed switch (BATCH, WRITERRX,
//!                                 ADAPTIVE, LAZY, GROUPINV, LEAN: ON|OFF)
//! e1000e_diag TUNE=0x3f           all speed switches as one number
//! e1000e_diag ... SAVE            keep the settings over a reboot (ENVARC:)
//! e1000e_diag DEFAULTS [SAVE]     back to the built-in settings
//! e1000e_diag SHOW                the settings only
//! e1000e_diag REPORT              the driver's report only
//! e1000e_diag RAW [FULL]          the card without any driver
//! e1000e_diag PRI=n | TXMUTE      tests: unit task priority, stop the
//!                                 transmit tail (watchdog)
//! UNIT=n                          the unit (default 0)
//! DEVICE=name                     the device (default e1000e_diag.device)
//! ```
//!
//! Settings live in ENV:e1000e_diag/MTU, TUNE and ITR (and ENVARC: with
//! SAVE). The driver reads them when a unit starts. TUNE and ITR also go to
//! a running unit at once; MTU takes effect when the unit next starts (when
//! every user has closed it, or after a reboot).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::{self, Write};
use core::panic::PanicInfo;

use amigaos4::allocator::ExecAllocator;
use amigaos4::port::AmigaMsgPort;
use amigaos4::sana2::*;
use amigaos4::sys::{self, PCIDevice, TagItem};

#[global_allocator]
static ALLOCATOR: ExecAllocator = ExecAllocator;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    amigaos4::panic::default_panic_handler(info)
}

struct Out;
impl Write for Out {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut buf = [0u8; 128];
        for chunk in s.as_bytes().chunks(buf.len() - 1) {
            buf[..chunk.len()].copy_from_slice(chunk);
            buf[chunk.len()] = 0;
            // SAFETY: a NUL-terminated buffer on the stack.
            unsafe { sys::dos_put_str(buf.as_ptr() as _) };
        }
        Ok(())
    }
}
macro_rules! say {
    ($($arg:tt)*) => {{ let _ = writeln!(Out, $($arg)*); }};
}

macro_rules! pci {
    ($dev:expr, $method:ident $(, $arg:expr)*) => {
        ((*$dev).$method)($dev $(, $arg)*)
    };
}

const CMD_DIAG: u16 = 0x8002;
const CMD_TXMUTE: u16 = 0x8003;
const CMD_TUNE: u16 = 0x8004;
const CMD_ITR: u16 = 0x8005;
const CMD_PRI: u16 = 0x8006;
const REPORT_MAX: usize = 16384;
const IDS: [u32; 2] = [0x10D3, 0x10F6];

/// dos/var.h: GVF_SAVE_VAR = 1 << GVB_SAVE_VAR (12): also write ENVARC:.
const GVF_SAVE_VAR: u32 = 1 << 12;

/// The speed switches (the driver's TUNE_* bits) by name.
const SWITCHES: [(&str, u32, &str); 6] = [
    ("LAZY", 1 << 0, "read the transmit head only when slots run low"),
    ("GROUPINV", 1 << 1, "invalidate receive descriptors a group at a time"),
    ("LEAN", 1 << 2, "one-register health check"),
    ("WRITERRX", 1 << 3, "the writer takes received frames itself"),
    ("ADAPTIVE", 1 << 4, "unit task at the stack's priority while mostly sending"),
    ("BATCH", 1 << 5, "transmit batching (tail written when the card is idle)"),
];
/// The diag build's built-in settings.
const TUNE_DEFAULT: u32 = 0x3f;
const ITR_DEFAULT: u32 = 196;
const MTU_DEFAULT: u32 = 1500;
const MTU_MAX: u32 = 9000;

struct Args {
    words: Vec<String>,
}

impl Args {
    /// "KEY=VALUE" or "KEY VALUE".
    fn value(&self, key: &str) -> Option<&str> {
        let pre = alloc::format!("{}=", key);
        for (i, w) in self.words.iter().enumerate() {
            if let Some(v) = w.strip_prefix(pre.as_str()) {
                return Some(v);
            }
            if w == key {
                return self.words.get(i + 1).map(|s| s.as_str());
            }
        }
        None
    }
    fn has(&self, key: &str) -> bool {
        self.words.iter().any(|w| w == key)
    }
}

fn number(v: &str) -> Option<u32> {
    match v.strip_prefix("0X") {
        Some(h) => u32::from_str_radix(h, 16).ok(),
        None => v.parse().ok(),
    }
}

fn on_off(v: &str) -> Option<bool> {
    match v {
        "ON" | "YES" | "1" => Some(true),
        "OFF" | "NO" | "0" => Some(false),
        _ => None,
    }
}

fn cstr(s: &str) -> Vec<u8> {
    let mut v: Vec<u8> = s.as_bytes().to_vec();
    v.push(0);
    v
}

unsafe fn get_var(name: &str) -> Option<u32> {
    let n = cstr(name);
    let mut buf = [0u8; 16];
    let got = sys::dos_get_var(n.as_ptr() as _, buf.as_mut_ptr() as _, buf.len() as i32, sys::GVF_GLOBAL_ONLY);
    if got <= 0 {
        return None;
    }
    let t = core::str::from_utf8(&buf[..got as usize]).ok()?.trim().to_ascii_uppercase();
    number(&t)
}

unsafe fn set_var(name: &str, value: &str, save: bool) -> bool {
    let n = cstr(name);
    let flags = sys::GVF_GLOBAL_ONLY | if save { GVF_SAVE_VAR } else { 0 };
    sys::dos_set_var(n.as_ptr() as _, value.as_ptr() as _, value.len() as i32, flags) != 0
}

unsafe fn delete_var(name: &str, save: bool) {
    let n = cstr(name);
    let flags = sys::GVF_GLOBAL_ONLY | if save { GVF_SAVE_VAR } else { 0 };
    sys::dos_delete_var(n.as_ptr() as _, flags);
}

#[no_mangle]
pub extern "C" fn rust_main(args: *const u8, arglen: i32) -> i32 {
    // SAFETY: the Shell's argument string, arglen bytes.
    let a = unsafe { core::slice::from_raw_parts(args, arglen.max(0) as usize) };
    let a = String::from_utf8_lossy(a).to_ascii_uppercase();
    let args = Args { words: a.split_whitespace().map(|s| s.to_string()).collect() };
    if args.has("?") || args.has("HELP") {
        say!("e1000e_diag [UNIT=n] [DEVICE=name] [MTU=n | JUMBO=ON|OFF] [ITR=n] [TUNE=n]");
        say!("            [BATCH|WRITERRX|ADAPTIVE|LAZY|GROUPINV|LEAN=ON|OFF] [SAVE] [DEFAULTS]");
        say!("            [SHOW] [REPORT] [RAW [FULL]] [PRI=n] [TXMUTE]");
        return 0;
    }
    if args.has("RAW") {
        // SAFETY: PCI reads; no clear-on-read register is touched.
        return unsafe { raw(args.has("FULL")) };
    }
    let unit = args.value("UNIT").and_then(number).unwrap_or(0);
    let device = args.value("DEVICE").map(|d| d.to_ascii_lowercase()).unwrap_or_else(|| "e1000e_diag.device".into());
    let device = if device.contains(':') || device.contains('/') { device } else { alloc::format!("networks/{}", device) };
    let save = args.has("SAVE");
    // SAFETY: DOS and exec calls in this Shell process.
    unsafe { settings_and_report(&args, unit, &device, save) }
}

unsafe fn settings_and_report(args: &Args, unit: u32, device: &str, save: bool) -> i32 {
    let mut changed_live: Vec<(u16, u32)> = Vec::new();
    let mut changed = false;
    let mut rc = 0;

    if args.has("DEFAULTS") {
        for v in ["e1000e_diag/MTU", "e1000e_diag/TUNE", "e1000e_diag/ITR"] {
            delete_var(v, save);
        }
        changed_live.push((CMD_TUNE, TUNE_DEFAULT));
        changed_live.push((CMD_ITR, ITR_DEFAULT));
        say!("Settings back to the built-in ones{}.", if save { " (ENV: and ENVARC:)" } else { " (ENV:)" });
        changed = true;
    }

    // MTU (jumbo frames).
    let mtu = match (args.value("MTU"), args.value("JUMBO")) {
        (Some(v), _) => match number(v) {
            Some(m) if (MTU_DEFAULT..=MTU_MAX).contains(&m) => Some(m),
            _ => {
                say!("MTU must be {}..{}", MTU_DEFAULT, MTU_MAX);
                return 10;
            }
        },
        (None, Some(v)) => match on_off(v) {
            Some(true) => Some(MTU_MAX),
            Some(false) => Some(MTU_DEFAULT),
            None => {
                say!("JUMBO must be ON or OFF");
                return 10;
            }
        },
        _ => None,
    };
    if let Some(m) = mtu {
        if m == MTU_DEFAULT {
            delete_var("e1000e_diag/MTU", save);
        } else if !set_var("e1000e_diag/MTU", &m.to_string(), save) {
            say!("Could not write ENV:e1000e_diag/MTU");
            rc = 10;
        }
        say!("MTU {}{}: takes effect when unit {} next starts (all users closed, or a reboot).",
            m, if m > MTU_DEFAULT { " (jumbo frames; the switch and the other end must take them too)" } else { "" }, unit);
        changed = true;
    }

    // Speed switches.
    let mut tune = get_var("e1000e_diag/TUNE").unwrap_or(TUNE_DEFAULT);
    let mut tune_set = false;
    if let Some(v) = args.value("TUNE") {
        match number(v) {
            Some(t) => {
                tune = t;
                tune_set = true;
            }
            None => {
                say!("TUNE must be a number (0x hex allowed)");
                return 10;
            }
        }
    }
    for (name, bit, _) in SWITCHES {
        if let Some(v) = args.value(name) {
            match on_off(v) {
                Some(true) => tune |= bit,
                Some(false) => tune &= !bit,
                None => {
                    say!("{} must be ON or OFF", name);
                    return 10;
                }
            }
            tune_set = true;
        }
    }
    if tune_set {
        if !set_var("e1000e_diag/TUNE", &alloc::format!("0x{:x}", tune), save) {
            say!("Could not write ENV:e1000e_diag/TUNE");
            rc = 10;
        }
        changed_live.push((CMD_TUNE, tune));
        changed = true;
    }

    // Interrupt throttling.
    if let Some(v) = args.value("ITR") {
        match number(v) {
            Some(i) if i <= 0xFFFF => {
                if !set_var("e1000e_diag/ITR", &i.to_string(), save) {
                    say!("Could not write ENV:e1000e_diag/ITR");
                    rc = 10;
                }
                changed_live.push((CMD_ITR, i));
                changed = true;
            }
            _ => {
                say!("ITR must be 0..65535 (x 256 ns; 0 = no throttling)");
                return 10;
            }
        }
    }

    // Tests (live only, not saved).
    let mut test = false;
    if let Some(v) = args.value("PRI") {
        match v.parse::<i32>() {
            Ok(p) => {
                changed_live.push((CMD_PRI, p as u32));
                test = true;
            }
            Err(_) => {
                say!("PRI must be a number");
                return 10;
            }
        }
    }
    if args.has("TXMUTE") {
        changed_live.push((CMD_TXMUTE, 0));
        test = true;
    }

    // Live: only to a unit that is running (opening a stopped one would
    // start it just to stop it again; it reads ENV: when it starts).
    if !changed_live.is_empty() {
        if unit_running(device, unit) {
            let mut ok = true;
            for (cmd, val) in &changed_live {
                if let Err(e) = send(device, unit, *cmd, *val, None) {
                    say!("{}", e);
                    ok = false;
                    rc = 10;
                }
            }
            if ok {
                say!("Applied to the running unit {}.", unit);
            }
        } else if test {
            say!("Unit {} is not running: PRI and TXMUTE need a running unit.", unit);
            rc = 5;
        } else {
            say!("Unit {} is not running: the settings apply when it starts.", unit);
        }
    }

    let want_show = args.has("SHOW") || changed || !args.has("REPORT");
    let want_report = args.has("REPORT") || (!changed && !test && !args.has("SHOW"));
    if want_show && !test {
        show_settings();
    }
    if want_report {
        let mut buf = vec![0u8; REPORT_MAX];
        match send(device, unit, CMD_DIAG, 0, Some(&mut buf)) {
            Ok(()) => {
                let n = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                let _ = Out.write_str(&String::from_utf8_lossy(&buf[..n]));
            }
            Err(e) => {
                say!("{} (RAW reads the card without the driver)", e);
                rc = 10;
            }
        }
    }
    rc
}

unsafe fn show_settings() {
    let mtu = get_var("e1000e_diag/MTU");
    let tune = get_var("e1000e_diag/TUNE");
    let itr = get_var("e1000e_diag/ITR");
    let t = tune.unwrap_or(TUNE_DEFAULT);
    say!("Settings (ENV:e1000e_diag/; \"built-in\" when unset):");
    let m = mtu.unwrap_or(MTU_DEFAULT);
    say!("  MTU   {}{}", m, if mtu.is_none() { " (built-in)" } else if m > MTU_DEFAULT { " (jumbo)" } else { "" });
    let i = itr.unwrap_or(ITR_DEFAULT);
    say!("  ITR   {}{} ({} interrupts/s at most)", i, if itr.is_none() { " (built-in)" } else { "" }, if i == 0 { 0 } else { 3_906_250 / i });
    say!("  TUNE  0x{:x}{}", t, if tune.is_none() { " (built-in)" } else { "" });
    for (name, bit, what) in SWITCHES {
        say!("    {:<9} {:<3} {}", name, if t & bit != 0 { "ON" } else { "OFF" }, what);
    }
}

/// The unit's task exists ("<device> unit N").
unsafe fn unit_running(device: &str, unit: u32) -> bool {
    let base = device.rsplit(['/', ':']).next().unwrap_or(device);
    let name = cstr(&alloc::format!("{} unit {}", base, unit));
    !sys::exec_find_task(name.as_ptr() as _).is_null()
}

/// One private command to the driver; `buf` receives a text report.
unsafe fn send(device: &str, unit: u32, cmd: u16, val: u32, buf: Option<&mut Vec<u8>>) -> Result<(), String> {
    let port = AmigaMsgPort::new().map_err(|_| String::from("no message port"))?;
    let ior = sys::exec_create_iorequest(port.as_ptr(), 88);
    let r = S2Req::from_raw(ior).ok_or_else(|| String::from("no I/O request"))?;
    // No buffer management: this opener only sends private commands.
    let tags = [TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 }];
    r.set_buffer_management(tags.as_ptr() as *mut _);
    let dn = cstr(device);
    let e = sys::exec_open_device(dn.as_ptr() as _, unit, r.as_ptr(), 0);
    if e != 0 {
        sys::exec_delete_iorequest(r.as_ptr());
        return Err(alloc::format!("OpenDevice({}, {}) failed: {}", device, unit, e));
    }
    r.io().set_command(cmd);
    r.set_wire_error(val);
    let (ptr, cap) = match buf {
        Some(b) => (b.as_mut_ptr(), b.len()),
        None => (core::ptr::null_mut(), 0),
    };
    r.set_data(ptr as *mut _);
    r.set_data_length(cap as u32);
    sys::exec_do_io(r.as_ptr());
    let err = r.io().error();
    sys::exec_close_device(r.as_ptr());
    sys::exec_delete_iorequest(r.as_ptr());
    if err != 0 {
        return Err(alloc::format!("command 0x{:x} failed: {} (is it the e1000e_diag.device build?)", cmd, err));
    }
    Ok(())
}

unsafe fn mmio_r32(base: u32, off: u32) -> u32 {
    amigaos4::arch::mmio_r32(base + off)
}
unsafe fn mmio_w32(base: u32, off: u32, v: u32) {
    amigaos4::arch::mmio_w32(base + off, v)
}

unsafe fn raw(full: bool) -> i32 {
    if sys::IPCI.is_null() {
        say!("e1000e_diag: no expansion.library \"pci\" interface");
        return 20;
    }
    let mut found = 0;
    for id in IDS {
        for index in 0.. {
            let tags = [
                TagItem { ti_Tag: sys::FDT_VENDOR_ID, ti_Data: 0x8086 },
                TagItem { ti_Tag: sys::FDT_DEVICE_ID, ti_Data: id },
                TagItem { ti_Tag: sys::FDT_INDEX, ti_Data: index },
                TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 },
            ];
            let dev = sys::wrappers::pci::pci_find_device(tags.as_ptr());
            if dev.is_null() {
                break;
            }
            found += 1;
            raw_card(dev, id, index, full);
            sys::wrappers::pci::pci_free_device(dev);
        }
    }
    if found == 0 {
        say!("e1000e_diag: no Intel 82574 card (8086:10d3 or 8086:10f6) on the PCI bus");
        return 5;
    }
    0
}

unsafe fn raw_card(dev: *mut PCIDevice, id: u32, index: u32, full: bool) {
    say!("== 8086:{:04x} #{} ==", id, index);
    say!(
        "PCI rev {:02x}, command {:04x}, status {:04x}, IRQ line {}, subsystem {:04x}:{:04x}",
        pci!(dev, ReadConfigByte, 8), pci!(dev, ReadConfigWord, 4), pci!(dev, ReadConfigWord, 6),
        pci!(dev, ReadConfigByte, 0x3C), pci!(dev, ReadConfigWord, 0x2C), pci!(dev, ReadConfigWord, 0x2E)
    );
    for bar in 0..6u8 {
        let r = pci!(dev, GetResourceRange, bar);
        if r.is_null() {
            continue;
        }
        let base = core::ptr::addr_of!((*r).BaseAddress).read_unaligned();
        let phys = core::ptr::addr_of!((*r).Physical).read_unaligned();
        let size = core::ptr::addr_of!((*r).Size).read_unaligned();
        let flags = core::ptr::addr_of!((*r).Flags).read_unaligned();
        say!("BAR {}: {} at 0x{:08x} (physical 0x{:08x}), {} KB",
            bar, if flags & sys::PCI_RANGE_IO != 0 { "I/O" } else { "memory" }, base, phys, size / 1024);
        pci!(dev, FreeResourceRange, r);
    }
    if pci!(dev, ReadConfigWord, 6) & 0x10 != 0 {
        let mut p = (pci!(dev, ReadConfigByte, 0x34) & 0xFC) as u32;
        let mut hops = 0;
        while p >= 0x40 && hops < 48 {
            if pci!(dev, ReadConfigByte, p) == 0x10 {
                let lcap = pci!(dev, ReadConfigLong, p + 0x0C);
                let ls = pci!(dev, ReadConfigWord, p + 0x12);
                let lc = pci!(dev, ReadConfigWord, p + 0x10);
                say!("PCIe link: gen {} x{} (card can do gen {} x{}), ASPM control {} (supported {})",
                    ls & 0xF, (ls >> 4) & 0x3F, lcap & 0xF, (lcap >> 4) & 0x3F, lc & 3, (lcap >> 10) & 3);
                break;
            }
            p = (pci!(dev, ReadConfigByte, p + 1) & 0xFC) as u32;
            hops += 1;
        }
    }
    let r0 = pci!(dev, GetResourceRange, 0);
    if r0.is_null() {
        say!("no BAR 0");
        return;
    }
    let base = core::ptr::addr_of!((*r0).BaseAddress).read_unaligned();
    let flags = core::ptr::addr_of!((*r0).Flags).read_unaligned();
    pci!(dev, FreeResourceRange, r0);
    if flags & sys::PCI_RANGE_IO != 0 || base == 0 {
        say!("BAR 0 is not a memory range");
        return;
    }
    if pci!(dev, ReadConfigWord, 4) & 0x2 == 0 {
        say!("memory decoding is off (command bit 1): registers not read");
        return;
    }
    let regs: [(&str, u32); 26] = [
        ("CTRL", 0x0000), ("STATUS", 0x0008), ("EECD", 0x0010), ("CTRL_EXT", 0x0018),
        ("ICS", 0x00C8), ("IMS", 0x00D0), ("ITR", 0x00C4), ("RCTL", 0x0100), ("TCTL", 0x0400),
        ("TIPG", 0x0410), ("EXTCNF", 0x0F00), ("PBA", 0x1000), ("FCRTL", 0x2160), ("FCRTH", 0x2168),
        ("RDBAL", 0x2800), ("RDLEN", 0x2808), ("RDH", 0x2810), ("RDT", 0x2818), ("TDBAL", 0x3800),
        ("TDLEN", 0x3808), ("TDH", 0x3810), ("TDT", 0x3818), ("GCR", 0x5B00), ("GCR2", 0x5B64),
        ("RAL0", 0x5400), ("RAH0", 0x5404),
    ];
    let mut line = String::new();
    for (i, (name, off)) in regs.iter().enumerate() {
        let _ = write!(line, "{:>8} {:08x}  ", name, mmio_r32(base, *off));
        if i % 4 == 3 {
            say!("{}", line);
            line.clear();
        }
    }
    if !line.is_empty() {
        say!("{}", line);
    }
    let st = mmio_r32(base, 0x0008);
    if st == 0xFFFF_FFFF {
        say!("STATUS reads all ones: the card does not answer at 0x{:08x}", base);
        return;
    }
    let speed = match st & 0xC0 { 0x80 => 1000, 0x40 => 100, _ => 10 };
    say!("link {}, {} Mbit/s {} duplex; driver loaded flag {}",
        if st & 2 != 0 { "up" } else { "down" }, speed, if st & 1 != 0 { "full" } else { "half" },
        if mmio_r32(base, 0x0018) & 0x1000_0000 != 0 { "SET (a driver runs the card)" } else { "clear" });
    if !full {
        return;
    }
    if mmio_r32(base, 0x0018) & 0x1000_0000 != 0 {
        say!("FULL skipped: a driver runs this card (use e1000e_diag REPORT)");
        return;
    }
    let take = || -> bool {
        for _ in 0..200 {
            let e = mmio_r32(base, 0x0F00);
            mmio_w32(base, 0x0F00, e | 0x20);
            if mmio_r32(base, 0x0F00) & 0x20 != 0 {
                return true;
            }
            amigaos4::timer::micro_delay(1000);
        }
        false
    };
    if take() {
        let phy_read = |reg: u32| -> Option<u16> {
            mmio_w32(base, 0x0020, (reg << 16) | (1 << 21) | 0x0800_0000);
            for _ in 0..1920 {
                amigaos4::timer::micro_delay(50);
                let v = mmio_r32(base, 0x0020);
                if v & 0x1000_0000 != 0 {
                    return if v & 0x4000_0000 != 0 { None } else { Some(v as u16) };
                }
            }
            None
        };
        let mut line = String::from("PHY:");
        for (name, reg) in [("BMCR", 0), ("BMSR", 1), ("ID1", 2), ("ID2", 3), ("ANAR", 4), ("ANLPAR", 5),
            ("1000CR", 9), ("1000SR", 10), ("SPEC", 0x11)] {
            match phy_read(reg) {
                Some(v) => { let _ = write!(line, " {} {:04x}", name, v); }
                None => { let _ = write!(line, " {} ----", name); }
            }
        }
        say!("{}", line);
        let e = mmio_r32(base, 0x0F00);
        mmio_w32(base, 0x0F00, e & !0x20);
    } else {
        say!("PHY: MDIO ownership not granted");
    }
    let mut line = String::from("NVM:");
    for w in 0..0x40u32 {
        if w % 16 == 0 {
            if w != 0 {
                say!("{}", line);
                line.clear();
            }
            let _ = write!(line, "  {:02x}:", w);
        }
        mmio_w32(base, 0x0014, (w << 2) | 1);
        let mut got = None;
        for _ in 0..10_000 {
            let v = mmio_r32(base, 0x0014);
            if v & 2 != 0 {
                got = Some((v >> 16) as u16);
                break;
            }
            amigaos4::timer::micro_delay(5);
        }
        match got {
            Some(v) => { let _ = write!(line, " {:04x}", v); }
            None => { let _ = write!(line, " ----"); }
        }
    }
    say!("{}", line);
}
