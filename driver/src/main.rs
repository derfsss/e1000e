//! e1000e.device, a SANA-II network driver for the Intel 82574L gigabit
//! controller: real PCIe cards (8086:10d3, 8086:10f6) and QEMU's
//! `-device e1000e`. Loaded from DEVS:Networks/.
//!
//! - Init finds and locks every card, one unit each, up to MAX_UNITS.
//! - A unit is set up at its first open and stopped at its last close: the
//!   card is reset and initialised, the receive and transmit descriptor
//!   rings are set up, receive buffers are kept posted, and an interrupt
//!   server and a unit task run.
//! - Each OpenDevice is an opener with its own buffer management (the
//!   stack's copy hook, S2_SANA2HOOK, or its OpenDevice copy functions),
//!   returned as ios2_BufferManagement.
//! - Received frames go to one pending CMD_READ of their type per opener;
//!   with none, to one S2_READORPHAN per opener. Frames to other stations
//!   and multicast groups nobody added are dropped unless an opener is
//!   promiscuous (the card's multicast hash table is only a first filter).
//! - CMD_WRITE, S2_BROADCAST and S2_MULTICAST copy the frame into one of
//!   the transmit slots in BeginIO itself, in the caller's task, and reply
//!   once it is posted; with every slot in flight they wait (pending) and
//!   the unit task sends them as slots come back.
//! - Statistics (global, extended, special, per tracked type), events,
//!   throughput sampling, multicast with reference counts, promiscuous and
//!   exclusive opens.
//! - The unofficial SANA-II extensions (sana2ext, the amigaos4 crate's
//!   docs/devices/sana2ext.md): S2X_QUERY with its capability list, read
//!   and write batches (S2X_READBATCH, S2X_WRITEBATCH), receive checksum
//!   results.
//!
//! DMA works on machines whose caches do not snoop PCI: each pool is
//! mapped once (StartDMA) for its physical address, and every buffer and
//! descriptor is written back from or invalidated in the data cache around
//! each transfer. Receive descriptors are handed back in aligned groups of
//! eight (128 bytes), so the CPU never writes a cache line that also holds
//! a descriptor the card owns; transmit descriptors ask for no write-back,
//! and the transmit head register says which slots are done.
//!
//! No Forbid or Disable: each unit's shared state and its transmit ring are
//! under their own exec mutexes (taken in that order: transmit, then
//! shared). The transmit tail register is written by whoever holds the
//! transmit lock; everything else on the card is touched only by the unit
//! task (and its interrupt server, which only reads the interrupt causes).
//! Counters are atomics.

#![no_std]
#![no_main]

extern crate alloc;

mod hw;
#[cfg(feature = "diag")]
mod ramlog;

/// An extras line (diag build): serial and RAM:e1000e_diag.log.
#[cfg(feature = "diag")]
macro_rules! xlog {
    ($($arg:tt)*) => {{
        let line = alloc::format!($($arg)*);
        amigaos4::serial_println!("{}: {}", NAME, line);
        crate::ramlog::line(line);
    }};
}

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::panic::PanicInfo;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use amigaos4::allocator::SharedExecAllocator;
use amigaos4::arch;
use amigaos4::device::cmd::{CMD_FLUSH, CMD_READ, CMD_WRITE, NSCMD_DEVICEQUERY};
use amigaos4::device::ioerr::{IOERR_ABORTED, IOERR_BADLENGTH, IOERR_NOCMD, IOERR_OPENFAIL, IOERR_UNITBUSY};
use amigaos4::device::{answer_device_query, Abort, IoReq, RequestQueue, Worker};
use amigaos4::interrupt::{IntServer, ResetCallback};
use amigaos4::mem::{DmaBuffer, DmaDirection, DmaMapping};
use amigaos4::sana2::*;
use amigaos4::sana2ext as x;
use amigaos4::serial_println;
use amigaos4::sys::{self, IORequest, PCIDevice, TagItem, Task};
use amigaos4::timer::{AmigaTimer, TimerUnit, TimerVal};
use hw::{bit, reg, Regs};

#[global_allocator]
// MEMF_SHARED: BeginIO, Open and Close run in the callers' tasks.
static ALLOCATOR: SharedExecAllocator = SharedExecAllocator;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    amigaos4::panic::default_panic_handler(info)
}

#[cfg(not(feature = "diag"))]
const NAME: &str = "e1000e.device";
#[cfg(not(feature = "diag"))]
const DEVNAME_C: &[u8] = amigaos4::amstr!("e1000e.device");
/// The diag build (feature "diag"): its own device name, so both can sit
/// in DEVS:Networks/ (only one can hold a card at a time).
#[cfg(feature = "diag")]
const NAME: &str = "e1000e_diag.device";
#[cfg(feature = "diag")]
const DEVNAME_C: &[u8] = amigaos4::amstr!("e1000e_diag.device");
const TASK_PRI: i32 = 5;
const TASK_STACK: u32 = 65536;
const MAX_UNITS: usize = 4;

/// One receive or transmit buffer for normal frames (RCTL.BSIZE 2048).
const BUF_SIZE: usize = 2048;
/// One buffer when jumbo frames are on (an MTU above 1500: S2XSET_MTU, or
/// the diag build's ENV:e1000e_diag/MTU): 16384 bytes, one frame per
/// descriptor (as FreeBSD's em driver does).
const JUMBO_BUF_SIZE: usize = 16384;
/// The MTUs the driver allows (the 82574 takes frames up to 9018 bytes;
/// the smallest is IPv4's minimum).
const JUMBO_MTU_MAX: u32 = 9000;
const MTU_MIN: u32 = 68;
/// Receive descriptors and buffers for normal frames (a multiple of
/// RX_GROUP; 1 MB of buffers). 128 overran with igb.device on real
/// hardware at ~37,000 frames a second (two downloads at once): the card's
/// receive FIFO overflowed while the ring waited to be emptied. The
/// descriptor ring is
/// always this long (also with jumbo frames, which use its first
/// RX_BUFFERS_JUMBO), so it never moves while the unit runs: the write
/// path's peek reads it without a lock.
const RX_BUFFERS: usize = 512;
/// With jumbo frames (16 KB buffers): 2 MB.
const RX_BUFFERS_JUMBO: usize = 128;

/// TCP segmentation (S2XCAP_SEGMENT): a segmented send is copied into one
/// of TSO_BUFS buffers of TSO_BUF bytes (a pool of its own, mapped once,
/// like the others), not into the ring's slots, and goes to the card with
/// one data descriptor (DTALEN is 16 bits). TSO_MAX_DATA is the most
/// ios2_DataLength (s2xcs_MaxSize), so the whole frame with its Ethernet
/// header fits a buffer and a descriptor.
const TSO_BUFS: usize = 8;
const TSO_BUF: usize = 65536;
const TSO_MAX_DATA: u32 = 65000;
/// MSS limits: at most 9216 bytes (each segment must fit the MTU anyway); the smallest
/// taken here, and the most segments one send may make.
const TSO_MSS_MAX: u32 = 9216;
const TSO_MSS_MIN: u16 = 64;
const TSO_MAX_SEGMENTS: u16 = 1024;

/// Flow control until S2XSET_FLOWCONTROL says otherwise: both ways, as
/// auto-negotiation resolves it with the link partner.
const FC_DEFAULT: u32 = x::S2XFC_AUTO | x::S2XFC_RX | x::S2XFC_TX;
/// The longest interrupt interval S2XSET_MODERATION takes (EITR's 13-bit
/// interval in 0.25 us units, a whole microsecond each step here).
/// ITR is 16 bits of 256 ns: at most 16,776 us.
const MODERATION_MAX_US: u32 = 0xFFFF * 32 / 125;

/// The pause bits the PHY advertises for a flow-control setting (IEEE
/// 802.3 annex 28B): both ways = PAUSE + ASM_DIR, obey only = PAUSE +
/// ASM_DIR (the partner then resolves to us obeying), send only =
/// ASM_DIR, none = neither.
fn fc_adv(modes: u32) -> u16 {
    match (modes & x::S2XFC_RX != 0, modes & x::S2XFC_TX != 0) {
        (true, _) => hw::phy::AR_PAUSE | hw::phy::AR_ASM_DIR,
        (false, true) => hw::phy::AR_ASM_DIR,
        (false, false) => 0,
    }
}

/// Buffer size, RCTL long-packet bit and receive buffers for an MTU.
/// The packet buffer split for a maximum frame (FreeBSD em: 22 KB for
/// receive above 8 KB frames, else 32 KB).
fn pba_for(max_frame: u32) -> u32 {
    if max_frame > 8192 { bit::PBA_22K } else { bit::PBA_32K }
}

fn sizes_for(mtu: u32) -> (usize, u32, usize) {
    if mtu as usize > eth::MTU {
        (JUMBO_BUF_SIZE, bit::RCTL_SZ_16384 | bit::RCTL_BSEX | bit::RCTL_LPE, RX_BUFFERS_JUMBO)
    } else {
        (BUF_SIZE, 0, RX_BUFFERS)
    }
}
/// Receive descriptors go back to the card eight at a time: 128 bytes, a
/// whole number of cache lines on every PowerPC the driver runs on.
const RX_GROUP: usize = 8;
/// Transmit descriptors (a multiple of 8: the ring length must be a
/// multiple of 128 bytes). One always stays empty.
const TX_SLOTS: usize = 64;
/// Multicast groups the unit keeps track of (the card's hash table has no
/// limit; this bounds the driver's list).
const MCAST_TABLE_MAX: usize = 64;
/// Ranges (S2_ADDMULTICASTADDRESSES) larger than this receive all multicast.
const MCAST_RANGE_MAX: u64 = 64;
const STORM_LIMIT: u32 = 1000;
const MAX_RECOVERIES: u32 = 8;

/// SANA-II extensions (docs/devices/sana2ext.md in the amigaos4 crate):
/// the most frames one S2X_READBATCH / S2X_WRITEBATCH may carry
/// (S2XCapBatch's s2xcb_MaxFrames), and the driver's own revision of
/// them (s2xq_Revision).
const BATCH_MAX: u32 = 64;
const S2X_REVISION: u16 = 1;
/// Transmit queues: one (S2XCapRSS s2xcr_MaxTxQueues 1); receive queues
/// follow RSS (Unit::rx_queues).
const TX_QUEUES: u32 = 1;

/// Test only: reset the card behind the driver's back (recovery test).
#[cfg(feature = "diag")]
const E1000E_CMD_TESTFAULT: u16 = 0x8001;
/// Diagnostics: a text report (registers, PHY, NVM, PCIe link, driver
/// state, counters) into ios2_Data, ios2_DataLength bytes at most; the
/// length written comes back in ios2_DataLength. Read by e1000ediag.
#[cfg(feature = "diag")]
const E1000E_CMD_DIAG: u16 = 0x8002;
/// Test only: stop telling the card about new transmit descriptors, so
/// frames wait with transmit still enabled (the transmit watchdog test).
#[cfg(feature = "diag")]
const E1000E_CMD_TXMUTE: u16 = 0x8003;
/// Transmit descriptors waiting with the head not moving this long: the
/// card is reset (82574 transmit hangs).
const TX_STALL_MS: u64 = 2000;
/// Speed switches, set at run time (E1000E_CMD_TUNE, e1000ediag TUNE=n;
/// ios2_WireError carries the value) so variants compare in one boot.
#[cfg(feature = "diag")]
const E1000E_CMD_TUNE: u16 = 0x8004;
/// The interrupt throttling interval in 256 ns units (ITR register; 0 =
/// none). QEMU raises anything below 500.
#[cfg(feature = "diag")]
const E1000E_CMD_ITR: u16 = 0x8005;
/// The write path reads the card's transmit head only when few slots
/// are left (a register read is a bus round trip on a real card, and
/// waits for QEMU's network thread under QEMU).
const TUNE_LAZY_RECLAIM: u32 = 1 << 0;
/// Receive descriptors are invalidated in the cache a group at a time.
const TUNE_RX_GROUP_INV: u32 = 1 << 1;
/// The 100 ms health check reads one register (RCTL) instead of three.
const TUNE_LEAN_HEALTH: u32 = 1 << 2;
/// After a write that finds a received frame waiting, the writer (the
/// stack's own task) takes the received frames itself when the unit task
/// is not busy, instead of waking it: no task switch per frame.
const TUNE_RX_IN_WRITER: u32 = 1 << 3;
/// While the unit mostly sends (bytes out far above bytes in), the unit
/// task drops to the stack's priority, so a writer is not preempted by
/// every received ACK and the ACKs are taken in batches; otherwise it
/// runs at TASK_PRI.
const TUNE_ADAPTIVE_PRI: u32 = 1 << 4;
/// The unit task's priority while the unit mostly sends.
const TASK_PRI_SENDING: i32 = 0;
/// "Mostly sends": over a 100 ms check, at least this many bytes out and
/// SEND_RATIO times the bytes in.
const SEND_MIN_BYTES: u64 = 64 * 1024;
const SEND_RATIO: u64 = 8;
/// Transmit batching (diag build): the tail register is written only when
/// the card has gone idle (TXQE) or TX_BATCH_MAX frames wait; otherwise
/// frames are added behind the ones the card is still sending.
const TUNE_TX_BATCH: u32 = 1 << 5;
const TX_BATCH_MAX: u32 = 16;
#[cfg(not(feature = "diag"))]
const TUNE_DEFAULT: u32 = TUNE_LAZY_RECLAIM | TUNE_RX_GROUP_INV | TUNE_LEAN_HEALTH | TUNE_RX_IN_WRITER | TUNE_ADAPTIVE_PRI;
#[cfg(feature = "diag")]
const TUNE_DEFAULT: u32 = TUNE_LAZY_RECLAIM | TUNE_RX_GROUP_INV | TUNE_LEAN_HEALTH | TUNE_RX_IN_WRITER | TUNE_ADAPTIVE_PRI | TUNE_TX_BATCH;
/// Test only: the unit task's priority (ios2_WireError, signed).
#[cfg(feature = "diag")]
const E1000E_CMD_PRI: u16 = 0x8006;
/// Free transmit slots below which the write path reads the head.
const TX_RECLAIM_BELOW: usize = 16;
/// How often an idle unit task wakes to look at the card.
const HEARTBEAT_MS: u32 = 1000;
/// A received frame that finds no read waits in the ring at most this long
/// for the stack's next read, and the unit task looks again this often.
const HOLD_MS: u64 = 100;
const HOLD_POLL_MS: u32 = 10;

const PCI_INTERRUPT_LINE: u32 = 0x3C;
const PCI_COMMAND: u32 = 0x04;
const PCI_COMMAND_MEMORY: u16 = 1 << 1;
const PCI_COMMAND_MASTER: u16 = 1 << 2;

/// Ethernet framing.
mod eth {
    pub const HLEN: usize = 14;
    pub const MTU: usize = 1500;
    pub const BROADCAST: [u8; 6] = [0xff; 6];
    pub const fn is_multicast(a: &[u8; 6]) -> bool {
        a[0] & 1 != 0
    }
}

macro_rules! pci {
    ($dev:expr, $method:ident $(, $arg:expr)*) => {
        ((*$dev).$method)($dev $(, $arg)*)
    };
}

// ---------------------------------------------------------------------------
// Shared unit state, under the unit's mutex
// ---------------------------------------------------------------------------

/// A value under an exec mutex, const-constructible for a static.
struct Locked<T> {
    value: UnsafeCell<T>,
    mutex: AtomicPtr<c_void>,
}
// SAFETY: `value` is only touched with `mutex` held (or by one user before
// init / after destroy).
unsafe impl<T> Sync for Locked<T> {}

impl<T> Locked<T> {
    const fn new(v: T) -> Self {
        Self { value: UnsafeCell::new(v), mutex: AtomicPtr::new(null_mut()) }
    }
    unsafe fn init(&self) -> bool {
        let m = sys::exec_alloc_sys_object(sys::ASOT_MUTEX, [TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 }].as_ptr());
        self.mutex.store(m as *mut c_void, Ordering::Release);
        !m.is_null()
    }
    unsafe fn destroy(&self) {
        let m = self.mutex.swap(null_mut(), Ordering::AcqRel);
        if !m.is_null() {
            sys::exec_free_sys_object(sys::ASOT_MUTEX, m as sys::APTR);
        }
    }
    fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let m = self.mutex.load(Ordering::Acquire);
        // SAFETY: the mutex guards the value.
        unsafe {
            if !m.is_null() {
                sys::exec_mutex_obtain(m as sys::APTR);
            }
            let r = f(&mut *self.value.get());
            if !m.is_null() {
                sys::exec_mutex_release(m as sys::APTR);
            }
            r
        }
    }
}

/// One OpenDevice: the stack's buffer management and open flags. Its
/// address is the ios2_BufferManagement cookie the stack's requests carry.
struct Opener {
    bm: BufferManagement,
    flags: u32,
}

#[derive(Clone, Copy, Default)]
struct Stats {
    rx_packets: u64,
    tx_packets: u64,
    rx_bytes: u64,
    tx_bytes: u64,
    bad_data: u64,
    overruns: u64,
    unknown_types: u64,
    reconfigurations: u64,
    rx_no_read: u64,
    rx_filtered: u64,
    tx_waits: u64,
    mcast_rejects: u64,
    recoveries: u64,
}

/// A posted receive request: a CMD_READ, or an S2X_READBATCH with how far
/// it is filled. CMD_READs and read batches share one list in posting
/// order: a frame goes to the oldest one of its type (per opener).
#[derive(Clone, Copy)]
struct Rd {
    r: S2Req,
    /// Posting order (Shared::seq when it was posted).
    seq: u32,
    /// The request's packet type and opener, kept here when it is posted:
    /// choosing a read for a frame then reads only this list, not every
    /// posted request.
    ptype: u32,
    op: *mut Opener,
    batch: Option<RxBatch>,
}

#[derive(Clone, Copy)]
struct RxBatch {
    /// The next frame to fill: a node of s2xb_Frames.
    next: *mut MinNode,
    filled: u32,
    /// Frames it can take (s2xb_Count, at most the list's length).
    max: u32,
    queue: u32,
}

impl Rd {
    /// Reply it: a batch with s2xb_Done set to the frames filled.
    ///
    /// # Safety
    /// A request the driver holds (taken out of the list).
    unsafe fn end(self, error: i8, wire: u32) {
        // SAFETY: the caller's contract.
        unsafe {
            if let Some(b) = self.batch {
                set_done(self.r, b.filled);
            }
            self.r.reply(error, wire);
        }
    }
}

/// Where a received frame goes: a posted CMD_READ or read batch (taken out
/// of the list), or an S2_READORPHAN.
#[derive(Clone, Copy)]
enum Pick {
    Read(Rd),
    Orphan(S2Req),
}

/// Openers one frame is handed to without a heap allocation (more spill
/// into a Vec).
const TAKEN_INLINE: usize = 4;

/// The reads a received frame goes to, one per opener. Inline for the
/// usual few openers: a heap allocation and free per frame (AllocVecTags,
/// FreeVec) cost more than the rest of choosing the reads.
struct Taken {
    n: usize,
    inline: [Option<(Pick, *mut Opener)>; TAKEN_INLINE],
    more: Vec<(Pick, *mut Opener)>,
}

impl Taken {
    const fn new() -> Self {
        Taken { n: 0, inline: [None; TAKEN_INLINE], more: Vec::new() }
    }

    fn len(&self) -> usize {
        self.n
    }

    fn is_empty(&self) -> bool {
        self.n == 0
    }

    fn push(&mut self, v: (Pick, *mut Opener)) {
        if self.n < TAKEN_INLINE {
            self.inline[self.n] = Some(v);
        } else {
            self.more.push(v);
        }
        self.n += 1;
    }

    fn has(&self, op: *mut Opener) -> bool {
        self.iter().any(|(_, o)| o == op)
    }

    fn iter(&self) -> impl Iterator<Item = (Pick, *mut Opener)> + '_ {
        self.inline.iter().flatten().copied().chain(self.more.iter().copied())
    }
}

/// The card's receive checksum result for a frame as S2XCS_... flags:
/// IP_GOOD/BAD when it checked the IPv4 header (IPCS), L4_GOOD/BAD when
/// it checked TCP or UDP (L4I). Never an L4 result for an IP fragment
/// (IPv4 MF or offset, an IPv6 fragment header first): the card does not
/// check those, and a result there would mean only part of a datagram.
/// 0: nothing checked (ARP, ICMP, other types).
fn rx_checksum(frame: &[u8], status: u8, errors: u8) -> u32 {
    let mut cs = 0;
    if status & bit::RXD_STAT_IPCS != 0 {
        cs |= if errors & bit::RXD_ERR_IPE != 0 { x::S2XCS_IP_BAD } else { x::S2XCS_IP_GOOD };
    }
    let v4 = frame.len() > 13 && frame[12] == 0x08 && frame[13] == 0x00;
    if v4 && status & bit::RXD_STAT_L4I != 0 && !ip_fragment(frame) {
        cs |= if errors & bit::RXD_ERR_L4E != 0 { x::S2XCS_L4_BAD } else { x::S2XCS_L4_GOOD };
    }
    cs
}

/// Whether an Ethernet frame holds an IP fragment.
fn ip_fragment(f: &[u8]) -> bool {
    if f.len() < eth::HLEN + 20 {
        return false;
    }
    match (f[12], f[13]) {
        // IPv4: more fragments, or a fragment offset.
        (0x08, 0x00) => (u16::from_be_bytes([f[20], f[21]]) & 0x3FFF) != 0,
        // IPv6: a fragment header straight after the fixed header.
        (0x86, 0xDD) => f.len() >= eth::HLEN + 40 && f[20] == 44,
        _ => false,
    }
}

/// A write frame's checksum request (its S2XFRAME_CHECKSUM item): the
/// FILL_ flags, and for FILL_L4 where the sum starts (s2xfc_Start) and
/// where the result goes (s2xfc_Offset, from Start), counted from the
/// first byte of the data the buffer management copies (the IP header,
/// or the Ethernet header with SANA2IOF_RAW).
#[derive(Clone, Copy)]
struct TxCsum {
    flags: u32,
    start: u16,
    offset: u16,
}

/// The checksum request of write batch frame `f`, if it asks for one.
///
/// # Safety
/// `f` is a validated frame (`check_batch`).
unsafe fn tx_csum_item(f: *mut MinNode) -> Option<TxCsum> {
    // SAFETY: the caller's contract; the items are the stack's, read only.
    unsafe {
        let items = core::ptr::addr_of!((*(f as *mut x::S2XFrame)).s2xf_Items);
        let c = x::find::<x::S2XFrameChecksum>(items, x::S2XFRAME_CHECKSUM)?;
        let flags = core::ptr::addr_of!((*c).s2xfc_Flags).read_unaligned();
        if flags & (x::S2XCS_FILL_IP | x::S2XCS_FILL_L4) == 0 {
            return None;
        }
        Some(TxCsum {
            flags,
            start: core::ptr::addr_of!((*c).s2xfc_Start).read_unaligned(),
            offset: core::ptr::addr_of!((*c).s2xfc_Offset).read_unaligned(),
        })
    }
}

/// A write frame's segmentation request (its S2XFRAME_SEGMENT item).
#[derive(Clone, Copy)]
struct TxSeg {
    proto: u32,
    seg: u16,
    hdrlen: u16,
}

/// The segmentation request of write batch frame `f`, if any.
///
/// # Safety
/// `f` is a validated frame (`check_batch`).
unsafe fn tx_seg_item(f: *mut MinNode) -> Option<TxSeg> {
    // SAFETY: the caller's contract; the items are the stack's, read only.
    unsafe {
        let items = core::ptr::addr_of!((*(f as *mut x::S2XFrame)).s2xf_Items);
        let s = x::find::<x::S2XFrameSegment>(items, x::S2XFRAME_SEGMENT)?;
        Some(TxSeg {
            proto: core::ptr::addr_of!((*s).s2xfs_Protocol).read_unaligned(),
            seg: core::ptr::addr_of!((*s).s2xfs_SegSize).read_unaligned(),
            hdrlen: core::ptr::addr_of!((*s).s2xfs_HdrLen).read_unaligned(),
        })
    }
}

/// Plan TCP segmentation of `fr` (the whole frame, Ethernet header first)
/// for the card: the context's IPLEN, TCP header length (L4LEN) and MSS,
/// the data descriptor's TSE with TXSM (and IXSM for IPv4: the card
/// rewrites every segment's IP header) and PAYLEN, the payload after all
/// the headers. The stack's pseudo-header sum (length 0) is in the TCP
/// checksum field; the card adds each segment's length and fixes lengths,
/// sequence numbers, IP ids and checksums (the 82574's TCP segmentation).
/// None: the request does not describe this frame (protocol, header
/// length, checksum offsets) or is outside the limits (MSS 64..9216, each
/// segment within the MTU, headers within 512 bytes, at most
/// TSO_MAX_SEGMENTS segments, some payload).
fn tso_plan(fr: &[u8], raw: bool, s: TxSeg, c: TxCsum, mtu: u32) -> Option<Offload> {
    let len = fr.len();
    let maclen = eth::HLEN;
    let ver = *fr.get(maclen)? >> 4;
    let v4 = match (s.proto, ver) {
        (x::S2XCSUM_TCPV4, 4) => true,
        _ => return None,
    };
    let base = if raw { 0 } else { maclen };
    let tcp = base + c.start as usize;
    if c.offset != 16 || tcp < maclen + 20 || tcp + 20 > len {
        return None;
    }
    let iplen = tcp - maclen;
    if v4 && ((fr[maclen] & 15) as usize) * 4 != iplen {
        return None;
    }
    let l4len = ((fr[tcp + 12] >> 4) as usize) * 4;
    let hdrs = tcp + l4len;
    if l4len < 20 || hdrs > len || hdrs > 512 || base + s.hdrlen as usize != hdrs {
        return None;
    }
    let mss = s.seg as u32;
    let paylen = (len - hdrs) as u32;
    if mss < TSO_MSS_MIN as u32 || mss > TSO_MSS_MAX || (iplen + l4len) as u32 + mss > mtu || paylen == 0 || paylen.div_ceil(mss) > TSO_MAX_SEGMENTS as u32 {
        return None;
    }
    let mut p = Offload {
        iplen: iplen as u32,
        tucmd: hw::bit::TXCTX_TUCMD_TCP,
        popts: hw::bit::TXD_POPTS_TXSM,
        mss,
        l4len: l4len as u32,
        paylen,
    };
    if v4 {
        p.tucmd |= hw::bit::TXCTX_TUCMD_IP;
        p.popts |= hw::bit::TXD_POPTS_IXSM;
    }
    Some(p)
}

/// What the card is to do for a frame's checksums: the context
/// descriptor's IP header length and TUCMD, the data descriptor's POPTS.
#[derive(Clone, Copy)]
struct Offload {
    iplen: u32,
    tucmd: u32,
    popts: u32,
    /// Segmentation (0: none): the MSS, the TCP header length and the
    /// payload length (PAYLEN).
    mss: u32,
    l4len: u32,
    paylen: u32,
}

/// Plan the checksums `c` asks for on `fr` (the whole frame, Ethernet
/// header first; `raw`: the stack's offsets count from it, else from the
/// IP header after it). The card inserts an IPv4 header checksum
/// (IXSM: from MACLEN, IPLEN long, at MACLEN + 10) and a TCP or UDP
/// checksum (TXSM: from MACLEN + IPLEN to the end of the data, at +16 or
/// +6), the stack having put the pseudo-header sum there (the 82574's
/// TCP/IP context descriptor; the same model as the request's). Anything the card
/// cannot express this way (another offset, a sum that does not start
/// right after a 20..511-byte IP header) is computed here, in place, so
/// the request's meaning holds whatever it says. None: the request does
/// not fit the frame (refused).
fn tx_offload(fr: &mut [u8], raw: bool, c: TxCsum) -> Option<Offload> {
    let len = fr.len();
    let maclen = eth::HLEN;
    let v4 = fr.get(maclen).map_or(false, |b| b >> 4 == 4);
    let mut p = Offload { iplen: 0, tucmd: if v4 { hw::bit::TXCTX_TUCMD_IP } else { 0 }, popts: 0, mss: 0, l4len: 0, paylen: 0 };
    if c.flags & x::S2XCS_FILL_IP != 0 {
        if !v4 {
            return None;
        }
        let ihl = ((fr[maclen] & 15) as usize) * 4;
        if ihl < 20 || maclen + ihl > len {
            return None;
        }
        p.iplen = ihl as u32;
        p.popts |= hw::bit::TXD_POPTS_IXSM;
    }
    if c.flags & x::S2XCS_FILL_L4 != 0 {
        let base = if raw { 0 } else { maclen };
        let start = base + c.start as usize;
        let at = start + c.offset as usize;
        if start > len || at + 2 > len {
            return None;
        }
        let iplen = start.wrapping_sub(maclen);
        let card = start >= maclen + 20
            && iplen <= 511
            && (c.offset == 6 || c.offset == 16)
            && (p.iplen == 0 || p.iplen as usize == iplen);
        if card {
            p.iplen = iplen as u32;
            p.popts |= hw::bit::TXD_POPTS_TXSM;
            if c.offset == 16 {
                p.tucmd |= hw::bit::TXCTX_TUCMD_TCP;
            }
        } else {
            // The one's complement sum from Start to the end of the frame
            // (the pseudo-header sum is already in the field), complemented.
            let mut s = 0u32;
            for ch in fr[start..].chunks(2) {
                s += (ch[0] as u32) << 8 | *ch.get(1).unwrap_or(&0) as u32;
            }
            while s >> 16 != 0 {
                s = (s & 0xFFFF) + (s >> 16);
            }
            fr[at..at + 2].copy_from_slice(&(!(s as u16)).to_be_bytes());
        }
    }
    Some(p)
}

/// Put `rd` back in posting order (after a frame went to it, or the stack's
/// filter refused one).
fn reinsert(s: &mut Shared, rd: Rd) {
    let pos = s.reads.iter().position(|x| (x.seq.wrapping_sub(rd.seq) as i32) > 0).unwrap_or(s.reads.len());
    s.reads.insert(pos, rd);
}

/// s2xb_Done of a batch request.
///
/// # Safety
/// `r` is a validated batch request (ios2_StatData an S2XBatch).
unsafe fn set_done(r: S2Req, n: u32) {
    let b = r.stat_data() as *mut x::S2XBatch;
    // SAFETY: the caller's contract.
    unsafe { core::ptr::addr_of_mut!((*b).s2xb_Done).write_unaligned(n) };
}

/// The standard request of a batch frame.
///
/// # Safety
/// `f` is a validated frame (`check_batch`).
unsafe fn frame_req(f: *mut MinNode) -> S2Req {
    // SAFETY: the caller's contract (s2xf_Req checked non-null).
    unsafe { S2Req::from_raw(core::ptr::addr_of!((*(f as *mut x::S2XFrame)).s2xf_Req).read_unaligned() as *mut IORequest).unwrap() }
}

/// The frame after `f` in its batch.
///
/// # Safety
/// `f` is a node of a well-formed list.
unsafe fn next_frame(f: *mut MinNode) -> *mut MinNode {
    // SAFETY: the caller's contract.
    unsafe { core::ptr::addr_of!((*f).mln_Succ).read_unaligned() }
}

/// Check an S2X_READBATCH / S2X_WRITEBATCH before it is taken: its
/// structure, the frame count (1..=BATCH_MAX), every frame (sized, with a
/// request) and the queue. Returns the first frame, the frames it carries
/// (s2xb_Count, or fewer if the list is shorter) and the queue, with
/// s2xb_Done set to 0.
///
/// # Safety
/// `r` is an opened request.
unsafe fn check_batch(r: S2Req, queues: u32) -> Result<(*mut MinNode, u32, u32), (i8, u32)> {
    let b = r.stat_data() as *mut x::S2XBatch;
    if b.is_null() {
        return Err((S2ERR_BAD_ARGUMENT, S2WERR_NULL_POINTER));
    }
    // SAFETY: the stack's batch; s2xb_Size is read first and the rest only
    // when it covers the whole structure.
    unsafe {
        let size = core::ptr::addr_of!((*b).s2xb_Size).read_unaligned() as usize;
        if size < core::mem::size_of::<x::S2XBatch>() {
            return Err((IOERR_BADLENGTH, S2WERR_BAD_STATDATA));
        }
        let count = core::ptr::addr_of!((*b).s2xb_Count).read_unaligned();
        let queue = core::ptr::addr_of!((*b).s2xb_Queue).read_unaligned();
        if count == 0 || count > BATCH_MAX || queue >= queues {
            return Err((S2ERR_BAD_ARGUMENT, S2WERR_GENERIC_ERROR));
        }
        let list = core::ptr::addr_of_mut!((*b).s2xb_Frames);
        let mut first = null_mut();
        let mut n = 0u32;
        for f in x::nodes(list) {
            if n == count {
                break;
            }
            let fr = f as *mut x::S2XFrame;
            let fsize = core::ptr::addr_of!((*fr).s2xf_Size).read_unaligned() as usize;
            if fsize < core::mem::size_of::<x::S2XFrame>() {
                return Err((S2ERR_BAD_ARGUMENT, S2WERR_BAD_STATDATA));
            }
            if core::ptr::addr_of!((*fr).s2xf_Req).read_unaligned().is_null() {
                return Err((S2ERR_BAD_ARGUMENT, S2WERR_NULL_POINTER));
            }
            if n == 0 {
                first = f;
            }
            n += 1;
        }
        if n == 0 {
            return Err((S2ERR_BAD_ARGUMENT, S2WERR_GENERIC_ERROR));
        }
        core::ptr::addr_of_mut!((*b).s2xb_Done).write_unaligned(0);
        Ok((first, n, queue))
    }
}

/// The capability items S2X_QUERY hands out: built when the unit starts,
/// freed when it stops (after the last CloseDevice), never changed in
/// between, so any opener's task may read them.
#[repr(C)]
struct Caps {
    list: x::MinList,
    batch: x::S2XCapBatch,
    rx_csum: x::S2XCapChecksum,
    tx_csum: x::S2XCapChecksum,
    mtu: x::S2XCapMTU,
    moderation: x::S2XCapModeration,
    flow: x::S2XCapFlowControl,
    segment: x::S2XCapSegment,
    /// s2xq_Features: one bit per item in the list.
    features: u32,
}

impl Caps {
    /// `tso`: the unit has its segmentation buffers (S2XCAP_SEGMENT).
    fn build(tso: bool) -> Box<Caps> {
        let mut c = Box::new(Caps {
            list: x::MinList { mlh_Head: null_mut(), mlh_Tail: null_mut(), mlh_TailPred: null_mut() },
            batch: x::S2XCapBatch {
                s2xcb_Header: x::S2XItem::new(x::S2XCAP_BATCH, core::mem::size_of::<x::S2XCapBatch>()),
                s2xcb_MaxFrames: BATCH_MAX,
                s2xcb_Flags: 0,
            },
            // What the 82574 checks on receive and the driver reports
            // (legacy descriptors: IPv4 only).
            rx_csum: x::S2XCapChecksum {
                s2xcc_Header: x::S2XItem::new(x::S2XCAP_RXCHECKSUM, core::mem::size_of::<x::S2XCapChecksum>()),
                s2xcc_Protocols: x::S2XCSUM_IPV4 | x::S2XCSUM_TCPV4 | x::S2XCSUM_UDPV4,
                s2xcc_Reserved: 0,
            },
            // What the card computes on send (context descriptors), with
            // the driver computing what the card cannot express.
            tx_csum: x::S2XCapChecksum {
                s2xcc_Header: x::S2XItem::new(x::S2XCAP_TXCHECKSUM, core::mem::size_of::<x::S2XCapChecksum>()),
                s2xcc_Protocols: x::S2XCSUM_IPV4 | x::S2XCSUM_TCPV4 | x::S2XCSUM_UDPV4 | x::S2XCSUM_TCPV6 | x::S2XCSUM_UDPV6,
                s2xcc_Reserved: 0,
            },
            mtu: x::S2XCapMTU {
                s2xcm_Header: x::S2XItem::new(x::S2XCAP_MTU, core::mem::size_of::<x::S2XCapMTU>()),
                s2xcm_MinMTU: MTU_MIN,
                s2xcm_MaxMTU: JUMBO_MTU_MAX,
            },
            // One interval for receive and transmit (one interrupt): no
            // S2XMOD_TX.
            moderation: x::S2XCapModeration {
                s2xco_Header: x::S2XItem::new(x::S2XCAP_MODERATION, core::mem::size_of::<x::S2XCapModeration>()),
                s2xco_MaxMicros: MODERATION_MAX_US,
                s2xco_Flags: x::S2XMOD_RX,
            },
            flow: x::S2XCapFlowControl {
                s2xcf_Header: x::S2XItem::new(x::S2XCAP_FLOWCONTROL, core::mem::size_of::<x::S2XCapFlowControl>()),
                s2xcf_Modes: x::S2XFC_RX | x::S2XFC_TX | x::S2XFC_AUTO,
                s2xcf_Reserved: 0,
            },
            segment: x::S2XCapSegment {
                s2xcs_Header: x::S2XItem::new(x::S2XCAP_SEGMENT, core::mem::size_of::<x::S2XCapSegment>()),
                s2xcs_Protocols: x::S2XCSUM_TCPV4,
                s2xcs_MaxSize: TSO_MAX_DATA,
                s2xcs_MaxSegments: TSO_MAX_SEGMENTS,
                s2xcs_MinSegSize: TSO_MSS_MIN,
                s2xcs_Reserved: 0,
            },
            features: 0,
        });
        let l = core::ptr::addr_of_mut!((*c).list);
        // SAFETY: the list and its items live in one box that does not
        // move while linked.
        unsafe {
            x::MinList::init(l);
            x::MinList::add_tail(l, core::ptr::addr_of_mut!((*c).batch) as *mut MinNode);
            x::MinList::add_tail(l, core::ptr::addr_of_mut!((*c).rx_csum) as *mut MinNode);
            x::MinList::add_tail(l, core::ptr::addr_of_mut!((*c).tx_csum) as *mut MinNode);
            x::MinList::add_tail(l, core::ptr::addr_of_mut!((*c).mtu) as *mut MinNode);
            x::MinList::add_tail(l, core::ptr::addr_of_mut!((*c).moderation) as *mut MinNode);
            x::MinList::add_tail(l, core::ptr::addr_of_mut!((*c).flow) as *mut MinNode);
            if tso {
                x::MinList::add_tail(l, core::ptr::addr_of_mut!((*c).segment) as *mut MinNode);
            }
        }
        c.features = x::S2XF_BATCH | x::S2XF_RXCHECKSUM | x::S2XF_TXCHECKSUM | x::S2XF_MTU | x::S2XF_MODERATION | x::S2XF_FLOWCONTROL;
        if tso {
            c.features |= x::S2XF_SEGMENT;
        }
        c
    }
}

struct Shared {
    openers: Vec<*mut Opener>,
    /// Oldest first; a frame usually takes the front one.
    reads: VecDeque<Rd>,
    /// The next Rd::seq.
    seq: u32,
    orphans: Vec<S2Req>,
    events: Vec<S2Req>,
    samples: Vec<S2Req>,
    /// S2_TRACKTYPE: (type, statistics, opener), one entry per opener.
    tracked: Vec<(u32, Sana2PacketTypeStats, *mut Opener)>,
    stats: Stats,
    online: bool,
    configured: bool,
    link_up: bool,
    station: [u8; 6],
    factory: [u8; 6],
    mtu: u32,
    bps: u32,
    last_start: TimeVal,
    mcast: Vec<([u8; 6], u32)>,
    /// Large S2_ADDMULTICASTADDRESSES ranges in force (all multicast).
    mcast_wide: Vec<(u64, u64, u32)>,
    promisc_openers: u32,
    exclusive: bool,
    /// Packet types each opener has queued CMD_READs for: a frame of one of
    /// these that finds no read is held in the ring, not dropped.
    read_types: Vec<(u32, *mut Opener)>,
    /// A frame is held for a read: BeginIO wakes the unit task when one comes.
    rx_held: bool,
    /// The card's statistics registers, added up (hw::COUNTERS order).
    hw: [u64; hw::COUNTERS.len()],
    /// Flow control as last resolved at link up.
    fc_mode: &'static str,
}

impl Shared {
    const fn new() -> Self {
        Shared {
            openers: Vec::new(),
            reads: VecDeque::new(),
            seq: 0,
            orphans: Vec::new(),
            events: Vec::new(),
            samples: Vec::new(),
            tracked: Vec::new(),
            stats: Stats {
                rx_packets: 0,
                tx_packets: 0,
                rx_bytes: 0,
                tx_bytes: 0,
                bad_data: 0,
                overruns: 0,
                unknown_types: 0,
                reconfigurations: 0,
                rx_no_read: 0,
                rx_filtered: 0,
                tx_waits: 0,
                mcast_rejects: 0,
                recoveries: 0,
            },
            online: false,
            configured: false,
            link_up: true,
            station: [0; 6],
            factory: [0; 6],
            mtu: eth::MTU as u32,
            bps: 1_000_000_000,
            last_start: TimeVal { Seconds: 0, Microseconds: 0 },
            mcast: Vec::new(),
            mcast_wide: Vec::new(),
            promisc_openers: 0,
            exclusive: false,
            read_types: Vec::new(),
            rx_held: false,
            hw: [0; hw::COUNTERS.len()],
            fc_mode: "not resolved yet",
        }
    }
    fn opener_ok(&self, bm: *mut c_void) -> bool {
        !bm.is_null() && self.openers.iter().any(|&o| o as *mut c_void == bm)
    }
    fn mcast_wanted(&self, mac: &[u8; 6]) -> bool {
        if self.mcast.iter().any(|(m, _)| m == mac) {
            return true;
        }
        let v = mac_u64(mac);
        self.mcast_wide.iter().any(|&(lo, hi, _)| v >= lo && v <= hi)
    }
}

fn mac_u64(m: &[u8; 6]) -> u64 {
    m.iter().fold(0u64, |a, &b| a << 8 | b as u64)
}
fn u64_mac(v: u64) -> [u8; 6] {
    let b = v.to_be_bytes();
    [b[2], b[3], b[4], b[5], b[6], b[7]]
}

// ---------------------------------------------------------------------------
// Units
// ---------------------------------------------------------------------------

struct Unit {
    dev: AtomicPtr<PCIDevice>,
    opens: AtomicU32,
    running: AtomicU32,
    stuck: AtomicU32,
    stop_ack_task: AtomicPtr<Task>,
    stop_ack_mask: AtomicU32,
    /// The device, owned by the unit task while it runs.
    net: UnsafeCell<Option<Net>>,
    regs: UnsafeCell<Option<Regs>>,
    regs_ready: AtomicU32,
    quiesced: AtomicU32,
    irq_task: AtomicPtr<Task>,
    irq_mask: AtomicU32,
    irq_raised: AtomicU32,
    irq_claimed: AtomicU32,
    irq_passed_on: AtomicU32,
    irq_storm: AtomicU32,
    irq_drained: AtomicU32,
    irq_config: AtomicU32,
    /// Receive overruns (RXO) seen by the interrupt server, for the unit task.
    irq_overrun: AtomicU32,
    /// The receive descriptor ring and the next descriptor the unit task
    /// will look at, for the write path's peek (`rx_peek`); null while the
    /// unit is not running.
    rx_ring: AtomicPtr<u8>,
    rx_next: AtomicU32,
    /// A received frame waits in the ring for a read: no peek wakes.
    rx_holding: AtomicU32,
    /// Speed switches (TUNE_*).
    tune: AtomicU32,
    /// Held by whoever runs the receive side (Net::rx and everything
    /// else on the card): the unit task for each pass of its loop, or a
    /// writer that got it with MutexAttempt (TUNE_RX_IN_WRITER).
    rxl: Locked<()>,
    /// S2X_QUERY's capability items while the unit runs (null otherwise).
    caps: AtomicPtr<Caps>,
    /// Receive queues in force (always 1: no RSS on this driver): read
    /// batches name one.
    rx_queues: AtomicU32,
    /// The card went idle (TXQE seen) since the last tail write.
    tx_idle: AtomicU32,
    /// Frames longer than a normal Ethernet frame (jumbo), each way.
    jumbo_rx: AtomicU32,
    jumbo_tx: AtomicU32,
    /// The transmit ring, used by BeginIO in the stack's task and by the
    /// unit task, under its own mutex.
    tx: Locked<Option<TxRing>>,
    /// The time the unit task last read (for statistics stamped in BeginIO).
    now_s: AtomicU32,
    now_us: AtomicU32,
    /// Requests that need the device, for the unit task.
    cmdq: RequestQueue,
    worker: Worker,
    shared: Locked<Shared>,
    /// The receive mode or filter table must be sent again.
    rx_mode_dirty: AtomicU32,
    /// Out of service after repeated failed recoveries: writes fail at once.
    out_of_service: AtomicU32,
    /// Cost counters (S2_GETSPECIALSTATS), indexed by `perf::*`.
    perf: [AtomicU32; perf::N],
}

/// What each frame costs: counters for S2_GETSPECIALSTATS, so a profiler
/// can show interrupts, wakes, notifications and ACKs per frame for any
/// stack on the card. Atomics: they are counted in BeginIO, in the unit
/// task and in the interrupt server without the lock.
mod perf {
    pub const WAKES: usize = 0;
    pub const WAKES_RX: usize = 1;
    pub const TX_PUBLISH: usize = 2;
    pub const TX_NOTIFY: usize = 3;
    pub const RX_NOTIFY: usize = 4;
    pub const TX_ACKS: usize = 5;
    pub const RX_ACKS: usize = 6;
    /// Time inside the transmit notification (a register write that QEMU
    /// serves at once), in tenths of a microsecond.
    pub const TX_NOTIFY_T: usize = 7;
    /// Time the interrupt handler spends reading the ISR register.
    pub const ISR_T: usize = 8;
    /// Time in the driver's write path in the caller's task (all of it,
    /// the notification included).
    pub const TX_CALL_T: usize = 9;
    /// Unit task wakes from the write path's receive peek.
    pub const PEEK_WAKES: usize = 10;
    /// Time in the unit task's receive pass (0.1 us), and the part of it
    /// spent handing frames to the stack (its copy hook).
    pub const RX_T: usize = 11;
    pub const RX_DELIVER_T: usize = 12;
    /// Reads of the transmit head register (TDH).
    pub const TDH_READS: usize = 13;
    /// Receive passes run by a writer (TUNE_RX_IN_WRITER).
    pub const RX_BY_WRITER: usize = 14;
    /// Unit task priority changes (TUNE_ADAPTIVE_PRI).
    pub const PRI_CHANGES: usize = 15;
    /// Tail writes put off by transmit batching.
    pub const TX_DEFERRED: usize = 16;
    /// Frames sent with a checksum offload (sana2ext S2XFRAME_CHECKSUM).
    pub const TX_OFFLOADS: usize = 17;
    pub const N: usize = 18;
}

/// Timebase ticks per microsecond, set when the first unit starts.
static TB_PER_US: AtomicU32 = AtomicU32::new(0);

/// Tenths of a microsecond since `t0` (a timebase reading); 0 before the
/// tick rate is known.
fn tenths_since(t0: u64) -> u32 {
    let per = TB_PER_US.load(Ordering::Relaxed) as u64;
    if per == 0 {
        return 0;
    }
    (arch::timebase().wrapping_sub(t0) * 10 / per) as u32
}

/// Whether an Ethernet frame is an IPv4 TCP segment with no data and only
/// the ACK flag (a pure acknowledgement).
fn pure_ack(f: &[u8]) -> bool {
    if f.len() < 14 + 20 + 20 || f[12] != 0x08 || f[13] != 0x00 || f[14] >> 4 != 4 || f[23] != 6 {
        return false;
    }
    let ihl = ((f[14] & 15) as usize) * 4;
    let total = u16::from_be_bytes([f[16], f[17]]) as usize;
    let t = 14 + ihl;
    if f.len() < t + 20 {
        return false;
    }
    let doff = ((f[t + 12] >> 4) as usize) * 4;
    total == ihl + doff && f[t + 13] == 0x10
}

impl Unit {
    fn count(&self, i: usize) {
        self.perf[i].fetch_add(1, Ordering::Relaxed);
    }

    fn add_time(&self, i: usize, t0: u64) {
        self.perf[i].fetch_add(tenths_since(t0), Ordering::Relaxed);
    }

    const NEW: Unit = Unit {
        dev: AtomicPtr::new(null_mut()),
        opens: AtomicU32::new(0),
        running: AtomicU32::new(0),
        stuck: AtomicU32::new(0),
        stop_ack_task: AtomicPtr::new(null_mut()),
        stop_ack_mask: AtomicU32::new(0),
        net: UnsafeCell::new(None),
        regs: UnsafeCell::new(None),
        regs_ready: AtomicU32::new(0),
        quiesced: AtomicU32::new(0),
        irq_task: AtomicPtr::new(null_mut()),
        irq_mask: AtomicU32::new(0),
        irq_raised: AtomicU32::new(0),
        irq_claimed: AtomicU32::new(0),
        irq_passed_on: AtomicU32::new(0),
        irq_storm: AtomicU32::new(0),
        irq_drained: AtomicU32::new(0),
        irq_config: AtomicU32::new(0),
        irq_overrun: AtomicU32::new(0),
        rx_ring: AtomicPtr::new(null_mut()),
        rx_next: AtomicU32::new(0),
        rx_holding: AtomicU32::new(0),
        tune: AtomicU32::new(TUNE_DEFAULT),
        rxl: Locked::new(()),
        caps: AtomicPtr::new(null_mut()),
        rx_queues: AtomicU32::new(1),
        tx_idle: AtomicU32::new(1),
        jumbo_rx: AtomicU32::new(0),
        jumbo_tx: AtomicU32::new(0),
        tx: Locked::new(None),
        now_s: AtomicU32::new(0),
        now_us: AtomicU32::new(0),
        cmdq: RequestQueue::new(),
        worker: Worker::new(),
        shared: Locked::new(Shared::new()),
        rx_mode_dirty: AtomicU32::new(0),
        out_of_service: AtomicU32::new(0),
        perf: [const { AtomicU32::new(0) }; perf::N],
    };

    unsafe fn regs(&self) -> Option<Regs> {
        if self.regs_ready.load(Ordering::Acquire) != 0 { *self.regs.get() } else { None }
    }

    fn now(&self) -> TimeVal {
        TimeVal { Seconds: self.now_s.load(Ordering::Relaxed), Microseconds: self.now_us.load(Ordering::Relaxed) }
    }

    fn index(&'static self) -> usize {
        // SAFETY: every Unit lives in UNITS.
        unsafe { (self as *const Unit).offset_from(UNITS.0.get() as *const Unit) as usize }
    }
}

struct Units(UnsafeCell<[Unit; MAX_UNITS]>);
// SAFETY: each field is an atomic or a cell with one user at a time.
unsafe impl Sync for Units {}
static UNITS: Units = Units(UnsafeCell::new([Unit::NEW; MAX_UNITS]));
static NUNITS: AtomicU32 = AtomicU32::new(0);
static OPEN_LOCK: AtomicPtr<c_void> = AtomicPtr::new(null_mut());

fn unit(n: usize) -> &'static Unit {
    // SAFETY: shared references; every field has interior mutability.
    unsafe { &(*UNITS.0.get())[n] }
}
fn units() -> impl Iterator<Item = &'static Unit> {
    (0..NUNITS.load(Ordering::Acquire) as usize).map(unit)
}

#[cfg(not(feature = "diag"))]
const TASK_NAMES: [&[u8]; MAX_UNITS] = [
    amigaos4::amstr!("e1000e.device unit 0"),
    amigaos4::amstr!("e1000e.device unit 1"),
    amigaos4::amstr!("e1000e.device unit 2"),
    amigaos4::amstr!("e1000e.device unit 3"),
];
#[cfg(feature = "diag")]
const TASK_NAMES: [&[u8]; MAX_UNITS] = [
    amigaos4::amstr!("e1000e_diag.device unit 0"),
    amigaos4::amstr!("e1000e_diag.device unit 1"),
    amigaos4::amstr!("e1000e_diag.device unit 2"),
    amigaos4::amstr!("e1000e_diag.device unit 3"),
];
// ---------------------------------------------------------------------------
// The card (owned by the unit task)
// ---------------------------------------------------------------------------

/// One receive queue: its descriptor ring (always RX_BUFFERS long), its
/// buffers (rx_n of buf_size), where the driver is in it.
struct RxQ {
    // Mappings before their buffers: they drop (EndDMA) first. Each is
    // made once, for the physical address; the cache is managed per
    // transfer (see the module notes).
    _ring_map: Option<DmaMapping<'static>>,
    _pool_map: Option<DmaMapping<'static>>,
    ring: DmaBuffer,
    bufs: DmaBuffer,
    ring_phys: u32,
    pool_phys: u32,
    /// The next descriptor to look at.
    next: usize,
    /// RDT: the first descriptor of the group the driver holds back.
    tail: usize,
    /// The frame at `next` waits for a read, since then (timebase). While
    /// it waits nothing behind it on this queue is taken: the ring fills
    /// and the card holds back, instead of frames being dropped.
    held: Option<u64>,
}

impl RxQ {
    /// A queue's ring and `rx_n` buffers of `buf_size`, mapped for DMA
    /// (before the CPU writes them: StartDMA may invalidate the cache), the
    /// buffers out of the cache.
    unsafe fn alloc(rx_n: usize, buf_size: usize) -> Result<RxQ, String> {
        let e = |x: amigaos4::AmigaError| alloc::format!("{}", x);
        let mut ring = DmaBuffer::alloc_contiguous(RX_BUFFERS * hw::DESC_SIZE, 4096).map_err(e)?;
        let mut bufs = DmaBuffer::alloc_contiguous(rx_n * buf_size, 4096).map_err(e)?;
        // SAFETY: fresh pools of these sizes.
        let (ring_map, ring_phys) = unsafe { map_pool(&mut ring, RX_BUFFERS * hw::DESC_SIZE, DmaDirection::FromDevice)? };
        let (pool_map, pool_phys) = unsafe { map_pool(&mut bufs, rx_n * buf_size, DmaDirection::FromDevice)? };
        arch::cache_invalidate(bufs.as_mut_ptr(), rx_n * buf_size);
        Ok(RxQ { _ring_map: Some(ring_map), _pool_map: Some(pool_map), ring, bufs, ring_phys, pool_phys, next: 0, tail: 0, held: None })
    }
}

struct Net {
    dev: *mut PCIDevice,
    r: Regs,
    u: &'static Unit,
    /// The receive queues: queue 0 always, more with RSS.
    q: Vec<RxQ>,
    rx_n: usize,
    /// HOLD_MS in timebase ticks.
    hold_limit: u64,
    /// What the card was last told.
    sent_mode: Option<u32>,
    sent_mta: Option<Box<[u32; hw::MTA_WORDS]>>,
    sent_mac: Option<[u8; 6]>,
    /// Bytes per receive buffer, and the RCTL bits for it (size, BSEX, LPE).
    buf_size: usize,
    rctl_size: u32,
    /// The packet buffer split written before each reset (receive KB).
    pba: u32,
    /// The largest frame (MTU + header + CRC), for the flow-control marks.
    max_frame: u32,
    /// Read batches that took a frame in this receive pass: replied at
    /// its end (a batch takes every frame waiting, at least one).
    touched: Vec<S2Req>,
    /// Flow control as set (S2XFC_...): with AUTO resolved at each link
    /// up, else forced.
    fc_modes: u32,
    /// The interrupt interval (EITR 0 bits 14:2, 0.25 us units) as set:
    /// kept over resets.
    itr: u32,
}

/// The namespace of the card claims (exec named memory).
const CLAIM_SPACE: &[u8] = b"e1000e\0";

/// A card claimed by this driver: a named memory block "card at 0x..."
/// (the card's register address) in the "e1000e" namespace. PCIDevice
/// Lock(EXCLUSIVE) alone did not keep the other build out (both are
/// initialised by ramlib's task, and the second took a card the first was
/// running). AllocNamedMemory fails when the name exists, so claiming is
/// one step. (An exec resource made of a bare Node did not work: the
/// second build's OpenResource on it never returned.)
struct CardClaim {
    name: [u8; 32],
}

static CLAIMS: [AtomicPtr<CardClaim>; MAX_UNITS] = [const { AtomicPtr::new(null_mut()) }; MAX_UNITS];

/// The claim's name for a card (NUL-terminated), from its register address.
unsafe fn claim_name(dev: *mut PCIDevice) -> Option<[u8; 32]> {
    let (base, _) = bar0(dev)?;
    let mut name = [0u8; 32];
    let text = alloc::format!("card at 0x{:08x}", base);
    name[..text.len()].copy_from_slice(text.as_bytes());
    Some(name)
}

/// Whether another e1000e build has claimed this card.
unsafe fn card_claimed(dev: *mut PCIDevice) -> bool {
    match claim_name(dev) {
        Some(name) => !sys::exec_find_named_memory(CLAIM_SPACE.as_ptr() as _, name.as_ptr() as _).is_null(),
        None => false,
    }
}

/// Claim a card for this driver. None: it has no register range, or
/// another e1000e build claimed it first.
unsafe fn claim_card(dev: *mut PCIDevice) -> Option<*mut CardClaim> {
    let name = claim_name(dev)?;
    let mut tags = [TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 }];
    let m = sys::exec_alloc_named_memory(4, CLAIM_SPACE.as_ptr() as _, name.as_ptr() as _, tags.as_mut_ptr());
    if m.is_null() {
        return None;
    }
    Some(Box::into_raw(Box::new(CardClaim { name })))
}

unsafe fn release_claim(c: *mut CardClaim) {
    if !c.is_null() {
        sys::exec_free_named_memory(CLAIM_SPACE.as_ptr() as _, (*c).name.as_ptr() as _);
        drop(Box::from_raw(c));
    }
}

/// Find every 82574 card, locked and claimed for this driver, in unit order.
unsafe fn find_cards() -> Vec<(*mut PCIDevice, *mut CardClaim)> {
    let mut found = Vec::new();
    for id in hw::DEVICE_IDS {
        for index in 0.. {
            if found.len() == MAX_UNITS {
                return found;
            }
            let tags = [
                TagItem { ti_Tag: sys::FDT_VENDOR_ID, ti_Data: hw::VENDOR_INTEL },
                TagItem { ti_Tag: sys::FDT_DEVICE_ID, ti_Data: id },
                TagItem { ti_Tag: sys::FDT_INDEX, ti_Data: index },
                TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 },
            ];
            let dev = sys::wrappers::pci::pci_find_device(tags.as_ptr());
            if dev.is_null() {
                break;
            }
            // Another e1000e build has this card: leave it completely alone.
            // No Lock, no Unlock and no FreeDevice: the handle FindDevice
            // gives out is the other driver's own (freeing it on QEMU sent
            // the running driver's config writes to the virtio-scsi
            // controller).
            if card_claimed(dev) {
                serial_println!("{}: 8086:{:04x} #{} is run by another e1000e build; skipped", NAME, id, index);
                continue;
            }
            if pci!(dev, Lock, sys::PCI_LOCK_EXCLUSIVE) == 0 {
                serial_println!("{}: 8086:{:04x} #{} is locked by another driver; skipped", NAME, id, index);
                continue;
            }
            let Some(claim) = claim_card(dev) else {
                serial_println!("{}: 8086:{:04x} #{} has no register range, or another e1000e build claimed it; skipped", NAME, id, index);
                pci!(dev, Unlock);
                continue;
            };
            found.push((dev, claim));
        }
    }
    found
}

fn hex_mac(m: &[u8; 6]) -> String {
    alloc::format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
}

/// BAR 0's CPU address (the registers), if it is a memory range.
unsafe fn bar0(dev: *mut PCIDevice) -> Option<(u32, u32)> {
    let r = pci!(dev, GetResourceRange, 0);
    if r.is_null() {
        return None;
    }
    let flags = core::ptr::addr_of!((*r).Flags).read_unaligned();
    let base = core::ptr::addr_of!((*r).BaseAddress).read_unaligned();
    let size = core::ptr::addr_of!((*r).Size).read_unaligned();
    pci!(dev, FreeResourceRange, r);
    if flags & sys::PCI_RANGE_IO != 0 || base == 0 || size < 0x6000 {
        return None;
    }
    Some((base, size))
}

/// Millisecond sleeps for the reset: timer.device, or a busy wait without it.
struct Sleeper(Option<AmigaTimer>);

impl Sleeper {
    fn new() -> Self {
        Sleeper(AmigaTimer::open(TimerUnit::MicroHz).ok())
    }
    fn ms(&mut self, ms: u32) {
        match self.0.as_mut() {
            Some(t) => {
                let _ = t.delay(TimerVal::from_millis(ms));
            }
            None => amigaos4::timer::micro_delay(ms * 1000),
        }
    }
}

/// One whole pool, mapped for DMA: the mapping and its physical address.
unsafe fn map_pool(buf: &mut DmaBuffer, len: usize, dir: DmaDirection) -> Result<(DmaMapping<'static>, u32), String> {
    let m = DmaMapping::new(buf.as_mut_ptr(), len, dir).map_err(|x| alloc::format!("pool map: {}", x))?;
    let ent = m.entries();
    if ent.len() != 1 {
        return Err(alloc::format!("a DMA pool is {} blocks, needs 1", ent.len()));
    }
    let phys = ent[0].PhysicalAddress as usize as u32;
    Ok((m, phys))
}

impl Net {
    /// Reset and set the card up: registers, rings, buffers. Interrupts
    /// stay masked until the unit task's server is in place.
    unsafe fn setup(u: &'static Unit, dev: *mut PCIDevice) -> Result<Net, String> {
        let n = u.index();
        let e = |x: amigaos4::AmigaError| alloc::format!("{}", x);
        let device = pci!(dev, ReadConfigWord, 2);
        let irq = pci!(dev, ReadConfigByte, PCI_INTERRUPT_LINE);
        let cmd = pci!(dev, ReadConfigWord, PCI_COMMAND);
        let want = cmd | PCI_COMMAND_MEMORY | PCI_COMMAND_MASTER;
        if want != cmd {
            pci!(dev, WriteConfigWord, PCI_COMMAND, want);
        }
        let (base, size) = bar0(dev).ok_or("BAR 0 is not a memory range")?;
        let r = Regs::new(base);
        if r.r(reg::STATUS) == 0xFFFF_FFFF {
            return Err(alloc::format!("the card does not answer at 0x{:08x}", base));
        }
        // The MTU: 1500, or (diag build) ENV:e1000e_diag/MTU for jumbo frames.
        #[cfg(feature = "diag")]
        let mtu = env_mtu(n);
        #[cfg(not(feature = "diag"))]
        let mtu = eth::MTU as u32;
        let max_frame = mtu + eth::HLEN as u32 + 4;
        let (buf_size, rctl_size, rx_n) = sizes_for(mtu);
        let pba = pba_for(max_frame);
        r.w(reg::PBA, pba);
        let mut sleep = Sleeper::new();
        r.reset(&mut |ms| sleep.ms(ms))?;
        let factory = match r.factory_mac() {
            Some(m) if m != [0; 6] && m != [0xff; 6] && !eth::is_multicast(&m) => m,
            _ => {
                // No usable address: a locally administered one from the timebase.
                let tb = arch::timebase().to_be_bytes();
                [0x02, 0x00, tb[4], tb[5], tb[6], tb[7] ^ n as u8]
            }
        };
        r.init_hw();
        r.set_station(&factory);
        r.fc_init(max_frame);
        // Saved settings (diag build, the e1000e_diag tool): speed switches
        // and the interrupt throttling interval.
        #[cfg(feature = "diag")]
        {
            if let Some(v) = env_u32("e1000e_diag/TUNE") {
                u.tune.store(v, Ordering::Release);
                xlog!("unit {}: speed switches 0x{:x} from ENV:e1000e_diag/TUNE", n, v);
            }
            if let Some(v) = env_u32("e1000e_diag/ITR") {
                r.w(reg::ITR, v & 0xFFFF);
                xlog!("unit {}: ITR {} from ENV:e1000e_diag/ITR", n, v & 0xFFFF);
            }
        }
        let ps = r.phy_setup(fc_adv(FC_DEFAULT));
        let aspm = aspm_off(dev);
        match ps.error {
            Some(why) => serial_println!("{} unit {}: PHY not set up: {}", NAME, n, why),
            None => serial_println!(
                "{} unit {}: PHY id 0x{:08x}{}{}; ASPM {}",
                NAME, n, ps.id,
                if ps.powered_up { ", powered up" } else { "" },
                if ps.restarted { ", auto-negotiation restarted" } else { ", already set up" },
                aspm
            ),
        }

        // Receive: queue 0's ring and buffers.
        let q0 = RxQ::alloc(rx_n, buf_size)?;
        // Transmit.
        let mut tring = DmaBuffer::alloc_contiguous(TX_SLOTS * hw::DESC_SIZE, 4096).map_err(e)?;
        let mut tbufs = DmaBuffer::alloc_contiguous(TX_SLOTS * buf_size, 4096).map_err(e)?;
        let (tring_map, tring_phys) = map_pool(&mut tring, TX_SLOTS * hw::DESC_SIZE, DmaDirection::ToDevice)?;
        let (tpool_map, tpool_phys) = map_pool(&mut tbufs, TX_SLOTS * buf_size, DmaDirection::ToDevice)?;
        // Segmentation's pool (optional: without it S2XCAP_SEGMENT is not
        // offered).
        let tso = DmaBuffer::alloc_contiguous(TSO_BUFS * TSO_BUF, 4096).ok().and_then(|mut b| {
            let (m, p) = map_pool(&mut b, TSO_BUFS * TSO_BUF, DmaDirection::ToDevice).ok()?;
            Some((m, b, p))
        });
        if tso.is_none() {
            serial_println!("{} unit {}: no memory for the segmentation buffers: TCP segmentation not offered", NAME, n);
        }
        let (tso_map, tso_buf, tso_phys) = match tso {
            Some((m, b, p)) => (Some(m), Some(b), p),
            None => (None, None, 0),
        };

        let mut net = Net {
            dev,
            r,
            u,
            q: alloc::vec![q0],
            rx_n,
            hold_limit: timebase_hz() * HOLD_MS / 1000,
            sent_mode: None,
            sent_mta: None,
            sent_mac: None,
            buf_size,
            rctl_size,
            pba,
            max_frame,
            touched: Vec::new(),
            fc_modes: FC_DEFAULT,
            itr: r.itr(),
        };
        net.rx_start();
        u.rx_ring.store(net.q[0].ring.as_mut_ptr(), Ordering::Release);

        let mut tx = TxRing {
            _ring_map: Some(tring_map),
            _pool_map: Some(tpool_map),
            ring: tring,
            bufs: tbufs,
            ring_phys: tring_phys,
            phys: tpool_phys,
            n: TX_SLOTS,
            tail: 0,
            clean: 0,
            unkicked: false,
            pending: VecDeque::new(),
            r,
            stall_head: usize::MAX,
            stall_since: 0,
            mute: false,
            head_reads: 0,
            buf_size,
            unkicked_n: 0,
            last_big: false,
            _tso_map: tso_map,
            tso: tso_buf,
            tso_phys,
            tso_slot: [None; TSO_BUFS],
        };
        tx.start();
        u.tx_idle.store(1, Ordering::Release);
        u.tx.with(|t| *t = Some(tx));

        let (link_up, bps) = r.link();
        let fc_mode = if link_up { r.fc_resolve() } else { "not resolved yet (link down)" };
        serial_println!(
            "{} unit {}: 8086:{:04x}, registers at 0x{:08x} ({} KB), IRQ line {}",
            NAME, n, device, base, size / 1024, irq
        );
        serial_println!(
            "{} unit {}: MAC {}, rx {} descriptors, tx {} slots, link {}{}",
            NAME, n, hex_mac(&factory), rx_n, TX_SLOTS,
            if link_up { "up, " } else { "down (autonegotiation may still be running)" },
            if link_up { alloc::format!("{} Mbit/s {} duplex", bps / 1_000_000, if r.r(reg::STATUS) & bit::STATUS_FD != 0 { "full" } else { "half" }) } else { String::new() }
        );
        u.shared.with(|s| {
            s.factory = factory;
            s.station = factory;
            s.mtu = mtu;
            s.bps = if link_up { bps } else { 1_000_000_000 };
            s.link_up = link_up;
            s.fc_mode = fc_mode;
        });
        if link_up {
            serial_println!("{} unit {}: {}", NAME, n, fc_mode);
        }
        Ok(net)
    }

    unsafe fn rx_desc(&mut self, q: usize, i: usize) -> *mut u8 {
        self.q[q].ring.as_mut_ptr().add(i * hw::DESC_SIZE)
    }

    unsafe fn rx_ptr(&mut self, q: usize, i: usize) -> *mut u8 {
        self.q[q].bufs.as_mut_ptr().add(i * self.buf_size)
    }

    /// Write queue `q`'s descriptors `first..first + count` for their
    /// buffers and push them out of the cache.
    unsafe fn rx_fill(&mut self, q: usize, first: usize, count: usize) {
        for i in first..first + count {
            let d = self.rx_desc(q, i);
            hw::rx_desc_set(d, self.q[q].pool_phys + (i * self.buf_size) as u32);
        }
        let d = self.rx_desc(q, first);
        arch::cache_flush(d, count * hw::DESC_SIZE);
    }

    /// Fill every receive ring and start receiving: the card gets every
    /// descriptor but the last group of each, RSS spreads the frames over
    /// the queues as set (or none: everything on queue 0).
    unsafe fn rx_start(&mut self) {
        self.r.rx_stop();
        for q in 0..self.q.len() {
            self.rx_fill(q, 0, self.rx_n);
            let rq = &mut self.q[q];
            rq.next = 0;
            rq.tail = self.rx_n - RX_GROUP;
            rq.held = None;
            if !self.r.rx_queue_init(q, rq.ring_phys, self.rx_n, self.buf_size) {
                serial_println!("{} unit {}: receive queue {} did not report enabled", NAME, self.u.index(), q);
            }
        }
        self.u.rx_next.store(0, Ordering::Release);
        self.u.rx_holding.store(0, Ordering::Release);
        self.u.shared.with(|s| s.rx_held = false);
        self.u.rx_queues.store(self.q.len() as u32, Ordering::Release);
        self.r.rx_enable(self.sent_mode.unwrap_or(0) | self.rctl_size);
        arch::sync_barrier();
        for q in 0..self.q.len() {
            self.r.w(Regs::rdt(q), self.q[q].tail as u32);
        }
    }

    /// Whether a frame waits in any queue for a read.
    fn any_held(&self) -> bool {
        self.q.iter().any(|q| q.held.is_some())
    }

    /// Bring the card's receive mode, multicast table and station address
    /// in line with the openers and multicast list.
    unsafe fn apply_rx_mode(&mut self) {
        let (promisc, table, wide, station) = self.u.shared.with(|s| {
            (s.promisc_openers > 0, s.mcast.iter().map(|(m, _)| *m).collect::<Vec<_>>(), !s.mcast_wide.is_empty(), s.station)
        });
        let mode = if promisc { bit::RCTL_UPE | bit::RCTL_MPE } else if wide { bit::RCTL_MPE } else { 0 };
        if self.sent_mac != Some(station) {
            self.r.set_station(&station);
            self.sent_mac = Some(station);
        }
        let mta = Box::new(hw::mta_for(&table));
        if self.sent_mta.as_deref() != Some(&*mta) {
            for (i, w) in mta.iter().enumerate() {
                self.r.w(reg::MTA + 4 * i as u32, *w);
            }
            self.sent_mta = Some(mta);
        }
        if self.sent_mode != Some(mode) {
            let v = (self.r.r(reg::RCTL) & !(bit::RCTL_UPE | bit::RCTL_MPE)) | self.rctl_size;
            self.r.w(reg::RCTL, v | mode);
            self.sent_mode = Some(mode);
        }
    }

    /// After the card was reset: it owns no buffer any more.
    fn end_all_mappings(&mut self) {
        for q in self.q.iter_mut() {
            q.held = None;
        }
        self.u.shared.with(|s| s.rx_held = false);
    }

    /// Reset the card and set it up again in place (recovery): registers,
    /// both rings, receive mode. Reads stay pending.
    unsafe fn recover(&mut self) -> Result<(), String> {
        // The transmit side is held for the whole recovery, so no caller
        // posts into the ring while it is replaced.
        let u = self.u;
        u.tx.with(|tx| -> Result<(), String> {
            self.r.w(reg::PBA, self.pba);
            let mut sleep = Sleeper::new();
            self.r.reset(&mut |ms| sleep.ms(ms))?;
            self.reinit(tx);
            Ok(())
        })
    }

    /// After a reset (the transmit lock held, `tx` its ring): the chip
    /// settings, the PHY, flow control, the interrupt interval, both rings
    /// and the receive mode, as they were set. Posted reads stay queued.
    unsafe fn reinit(&mut self, tx: &mut Option<TxRing>) {
        let u = self.u;
        self.end_all_mappings();
        self.r.init_hw();
        self.r.w(reg::ITR, self.itr);
        self.r.fc_init(self.max_frame);
        let _ = self.r.phy_setup(fc_adv(self.fc_modes));
        if self.fc_modes & x::S2XFC_AUTO == 0 {
            let _ = self.r.fc_force(self.fc_modes & x::S2XFC_RX != 0, self.fc_modes & x::S2XFC_TX != 0);
        }
        self.sent_mode = None;
        self.sent_mta = None;
        self.sent_mac = None;
        let station = u.shared.with(|s| s.station);
        self.r.set_station(&station);
        self.rx_start();
        if let Some(tx) = tx.as_mut() {
            tx.start();
        }
        u.tx_idle.store(1, Ordering::Release);
        self.apply_rx_mode();
        let _ = self.r.read_icr();
        {
            self.r.enable_irqs();
        }
        u.irq_drained.store(u.irq_claimed.load(Ordering::Relaxed), Ordering::Relaxed);
        u.irq_storm.store(0, Ordering::Release);
    }

    /// Pause frames as the flow-control setting says: forced (no AUTO), or
    /// as auto-negotiation resolved them at link up (`up`) and limited to
    /// the directions the setting allows. Returns the mode, for the log.
    unsafe fn fc_apply(&self, up: bool) -> &'static str {
        let (rx, tx) = (self.fc_modes & x::S2XFC_RX != 0, self.fc_modes & x::S2XFC_TX != 0);
        if self.fc_modes & x::S2XFC_AUTO == 0 {
            return self.r.fc_force(rx, tx);
        }
        if !up {
            return "not resolved yet (link down)";
        }
        let mode = self.r.fc_resolve();
        if !(rx && tx) {
            self.r.fc_mask(rx, tx);
        }
        mode
    }

    /// S2XSET_MTU and S2XSET_RSS: frames up to `mtu` bytes of packet data
    /// each way, and RSS as `rss` says (None or 1 queue: off). The card is
    /// reset and set up again with buffers for the size (16 KB above 1500,
    /// as at start) and one receive queue per RSS queue: every pool that is
    /// new is allocated first, so a failure changes nothing. Queue 0's
    /// descriptor ring never moves (the write path's peek reads it without
    /// a lock); only its buffers change with the size. What the card still
    /// has to send is given up to 50 ms first; writes waiting for a slot
    /// stay queued and are checked against the new MTU; posted reads stay
    /// queued. The link may go down and up again (the PHY is reset with the
    /// MAC on some chips).
    unsafe fn rebuild(&mut self, mtu: u32) -> Result<(), String> {
        let (buf_size, rctl_size, rx_n) = sizes_for(mtu);
        let nq = 1;
        let u = self.u;
        let e = |x: amigaos4::AmigaError| alloc::format!("{}", x);
        let resize = buf_size != self.buf_size;
        let pools = if resize {
            let mut rb = DmaBuffer::alloc_contiguous(rx_n * buf_size, 4096).map_err(e)?;
            let mut tb = DmaBuffer::alloc_contiguous(TX_SLOTS * buf_size, 4096).map_err(e)?;
            let (rmap, rphys) = map_pool(&mut rb, rx_n * buf_size, DmaDirection::FromDevice)?;
            let (tmap, tphys) = map_pool(&mut tb, TX_SLOTS * buf_size, DmaDirection::ToDevice)?;
            Some((rb, rmap, rphys, tb, tmap, tphys))
        } else {
            None
        };
        // Queues 1.. that are new, or whose buffers change size.
        let mut fresh: Vec<Option<RxQ>> = Vec::new();
        for k in 1..nq {
            fresh.push(if resize || k >= self.q.len() { Some(RxQ::alloc(rx_n, buf_size)?) } else { None });
        }
        u.tx.with(|tx| -> Result<(), String> {
            if let Some(t) = tx.as_mut() {
                t.push_out(u, true);
                for _ in 0..50 {
                    if self.r.r(reg::TDH) as usize == t.tail {
                        break;
                    }
                    amigaos4::timer::micro_delay(1000);
                }
            }
            // The packet buffer split for the new frame size takes effect
            // at the reset.
            self.pba = pba_for(mtu + eth::HLEN as u32 + 4);
            self.r.w(reg::PBA, self.pba);
            let mut sleep = Sleeper::new();
            self.r.reset(&mut |ms| sleep.ms(ms))?;
            // The card no longer reaches any buffer: the pools may change.
            if let Some((rb, rmap, rphys, tb, tmap, tphys)) = pools {
                let q0 = &mut self.q[0];
                q0._pool_map = None;
                q0.bufs = rb;
                q0._pool_map = Some(rmap);
                q0.pool_phys = rphys;
                arch::cache_invalidate(q0.bufs.as_mut_ptr(), rx_n * buf_size);
                if let Some(t) = tx.as_mut() {
                    t._pool_map = None;
                    t.bufs = tb;
                    t._pool_map = Some(tmap);
                    t.phys = tphys;
                    t.buf_size = buf_size;
                }
            }
            let mut old: Vec<RxQ> = self.q.drain(1..).collect();
            for (i, f) in fresh.into_iter().enumerate() {
                match f {
                    Some(q) => self.q.push(q),
                    None => self.q.push(old.remove(0)),
                }
                let _ = i;
            }
            drop(old);
            self.buf_size = buf_size;
            self.rctl_size = rctl_size;
            self.rx_n = rx_n;
            self.max_frame = mtu + eth::HLEN as u32 + 4;
            u.shared.with(|s| s.mtu = mtu);
            self.reinit(tx);
            Ok(())
        })
    }

    fn device_broken(&self) -> Option<&'static str> {
        if self.u.irq_storm.load(Ordering::Acquire) != 0 {
            return Some("the storm breaker stopped the card");
        }
        // SAFETY: register reads of the unit's card.
        unsafe {
            if self.u.tune.load(Ordering::Relaxed) & TUNE_LEAN_HEALTH != 0 {
                // One read: a card that is gone reads all ones, a reset
                // clears receive enable (and transmit enable with it).
                let rctl = self.r.r(reg::RCTL);
                return if rctl == 0xFFFF_FFFF {
                    Some("the card does not answer (all ones)")
                } else if rctl & bit::RCTL_EN == 0 {
                    Some("receive is off: the card was reset")
                } else {
                    None
                };
            }
            if self.r.r(reg::STATUS) == 0xFFFF_FFFF {
                Some("the card does not answer (all ones)")
            } else if self.r.r(reg::RCTL) & bit::RCTL_EN == 0 {
                Some("receive is off: the card was reset")
            } else if self.r.r(reg::TCTL) & bit::TCTL_EN == 0 {
                Some("transmit is off: the card was reset")
            } else {
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Statistics, events, throughput samples (task and BeginIO, under the lock)
// ---------------------------------------------------------------------------

/// Reply every S2_ONEVENT waiting for any of `events`.
fn signal_events(u: &Unit, events: u32) {
    let hit: Vec<(S2Req, u32)> = u.shared.with(|s| {
        let mut hit = Vec::new();
        s.events.retain(|&r| {
            let want = r.wire_error();
            if want & events != 0 {
                hit.push((r, want & events));
                false
            } else {
                true
            }
        });
        hit
    });
    for (r, got) in hit {
        // SAFETY: a queued request, ours to reply.
        unsafe { r.reply(0, got) };
    }
}

/// Count a frame in the throughput samples (bytes sent or received).
fn sample_update(s: &mut Shared, now: TimeVal, sent: u32, received: u32) {
    for &r in s.samples.iter() {
        let st = r.stat_data() as *mut Sana2ThroughputStats;
        if st.is_null() {
            continue;
        }
        // SAFETY: the caller's statistics block, at least s2ts_Actual bytes.
        unsafe {
            let actual = core::ptr::addr_of!((*st).s2ts_Actual).read_unaligned() as usize;
            let fits = |off: usize, len: usize| actual >= off + len;
            if fits(32, 8) {
                let p = core::ptr::addr_of_mut!((*st).s2ts_BytesSent);
                let v = p.read_unaligned();
                let n = ((v.s2q_High as u64) << 32 | v.s2q_Low as u64) + sent as u64;
                p.write_unaligned(S2Quad::from(n));
            }
            if fits(40, 8) {
                let p = core::ptr::addr_of_mut!((*st).s2ts_BytesReceived);
                let v = p.read_unaligned();
                let n = ((v.s2q_High as u64) << 32 | v.s2q_Low as u64) + received as u64;
                p.write_unaligned(S2Quad::from(n));
            }
            if fits(48, 8) {
                let p = core::ptr::addr_of_mut!((*st).s2ts_Updates);
                let v = p.read_unaligned();
                let n = ((v.s2q_High as u64) << 32 | v.s2q_Low as u64) + 1;
                p.write_unaligned(S2Quad::from(n));
            }
            if fits(24, 8) {
                core::ptr::addr_of_mut!((*st).s2ts_EndTime).write_unaligned(now);
            }
            if fits(16, 4) {
                let task = core::ptr::addr_of!((*st).s2ts_NotifyTask).read_unaligned();
                let mask = core::ptr::addr_of!((*st).s2ts_NotifyMask).read_unaligned();
                if !task.is_null() && mask != 0 {
                    sys::exec_signal(task, mask);
                }
            }
        }
    }
}

/// Update every opener's statistics for `ptype` (each opener that sent
/// S2_TRACKTYPE for it has its own).
fn type_stats(s: &mut Shared, ptype: u32, mut f: impl FnMut(&mut Sana2PacketTypeStats)) {
    for (t, st, _) in s.tracked.iter_mut() {
        if *t == ptype {
            f(st);
        }
    }
}

// ---------------------------------------------------------------------------
// Receive and transmit (the unit task)
// ---------------------------------------------------------------------------

impl Net {
    /// Take every received frame from every queue, deliver it, and give
    /// the descriptors back in whole groups.
    unsafe fn rx(&mut self, now: TimeVal) -> u32 {
        let t0 = arch::timebase();
        let mut n = 0;
        for q in 0..self.q.len() {
            n += self.rx_queue(q, now);
        }
        self.end_touched();
        if n > 0 {
            self.u.add_time(perf::RX_T, t0);
        }
        n
    }

    /// One queue's share of a receive pass.
    unsafe fn rx_queue(&mut self, q: usize, now: TimeVal) -> u32 {
        let group_inv = self.u.tune.load(Ordering::Relaxed) & TUNE_RX_GROUP_INV != 0;
        // The group whose descriptors were last invalidated together.
        let mut inv_group = usize::MAX;
        let mut n = 0;
        let mut refilled = false;
        loop {
            let i = self.q[q].next;
            let d = self.rx_desc(q, i);
            let mut fresh = true;
            if group_inv {
                let g = i / RX_GROUP;
                if g != inv_group {
                    arch::cache_invalidate(self.rx_desc(q, g * RX_GROUP), RX_GROUP * hw::DESC_SIZE);
                    inv_group = g;
                } else {
                    fresh = false;
                }
            } else {
                arch::cache_invalidate(d, hw::DESC_SIZE);
            }
            let (mut len, mut status, mut errors) = hw::rx_desc_get(d);
            if status & bit::RXD_STAT_DD == 0 && !fresh {
                // Read from the group's earlier invalidation: the card may
                // have written it since. Look once more, fresh.
                arch::cache_invalidate(d, hw::DESC_SIZE);
                (len, status, errors) = hw::rx_desc_get(d);
            }
            if status & bit::RXD_STAT_DD == 0 {
                break;
            }
            let len = len as usize;
            if status & bit::RXD_STAT_EOP == 0 || errors & bit::RXD_ERR_FRAME != 0 || len < eth::HLEN || len > self.buf_size {
                // A bad frame, or part of one too long for a buffer (long
                // frames are off, so the card should not make those).
                self.u.shared.with(|s| s.stats.bad_data += 1);
            } else {
                let p = self.rx_ptr(q, i);
                if self.q[q].held.is_none() {
                    arch::cache_invalidate(p, len);
                }
                let frame = core::slice::from_raw_parts(p, len);
                if len > eth::HLEN + eth::MTU {
                    self.u.jumbo_rx.fetch_add(1, Ordering::Relaxed);
                }
                let may_hold = match self.q[q].held {
                    None => true,
                    Some(since) => arch::timebase().wrapping_sub(since) < self.hold_limit,
                };
                // No RSS on this driver: no hash.
                let rss = (0, 0);
                let td = arch::timebase();
                let delivered = self.deliver(frame, now, may_hold, q as u32, rx_checksum(frame, status, errors), rss);
                self.u.add_time(perf::RX_DELIVER_T, td);
                if !delivered {
                    // No read for it yet: keep it (and everything behind
                    // it on this queue) in the ring until the stack queues one.
                    if self.q[q].held.is_none() {
                        self.q[q].held = Some(arch::timebase());
                    }
                    break;
                }
                if pure_ack(frame) {
                    self.u.count(perf::RX_ACKS);
                }
            }
            let rx_n = self.rx_n;
            let rq = &mut self.q[q];
            rq.held = None;
            n += 1;
            rq.next = (i + 1) % rx_n;
            if rq.next % RX_GROUP == 0 {
                // A whole group is done: write it afresh (the card no
                // longer owns any of its cache lines), and hand the card the
                // group held back so far. This group is the new one held back.
                let first = (rq.next + rx_n - RX_GROUP) % rx_n;
                rq.tail = first;
                self.rx_fill(q, first, RX_GROUP);
                refilled = true;
            }
        }
        if q == 0 {
            self.u.rx_next.store(self.q[0].next as u32, Ordering::Release);
            self.u.rx_holding.store(self.q[0].held.is_some() as u32, Ordering::Release);
        }
        if refilled {
            arch::sync_barrier();
            self.r.w(Regs::rdt(q), self.q[q].tail as u32);
            self.u.count(perf::RX_NOTIFY);
        }
        n
    }

    /// Hand one frame to the openers' reads. False: the frame is of a type
    /// an opener reads, but none of its reads is queued right now and
    /// `may_hold` allows waiting: nothing was done, try again later.
    ///
    /// Each opener gets the frame once, in its oldest posted request for
    /// the type: a CMD_READ (any queue) or an S2X_READBATCH for `queue`.
    ///
    /// `csum` is the card's checksum result (S2XCS_... flags) for read
    /// batch frames that carry an S2XFRAME_CHECKSUM item.
    unsafe fn deliver(&mut self, frame: &[u8], now: TimeVal, may_hold: bool, queue: u32, csum: u32, rss: (u32, u32)) -> bool {
        let dst = [frame[0], frame[1], frame[2], frame[3], frame[4], frame[5]];
        let src = [frame[6], frame[7], frame[8], frame[9], frame[10], frame[11]];
        let ptype = u16::from_be_bytes([frame[12], frame[13]]) as u32;
        let bcast = dst == eth::BROADCAST;
        let mcast = !bcast && eth::is_multicast(&dst);
        let len = frame.len() as u32;
        let fits = |rd: &Rd| rd.ptype == ptype && rd.batch.map_or(true, |b| b.queue == queue);
        // Choose the reads under the lock; copy and reply outside it. A
        // batch is taken out of the list while its frame is copied, and put
        // back (in its place) if it has room for more.
        let taken: Option<Taken> = self.u.shared.with(|s| {
            // A frame that waits is counted when it is finally taken.
            let wants = s.read_types.iter().any(|&(t, _)| t == ptype);
            let readable = s.reads.iter().any(fits) || !s.orphans.is_empty();
            let filtered_out = s.promisc_openers == 0
                && ((mcast && !s.mcast_wanted(&dst)) || (!bcast && !mcast && dst != s.station));
            if s.online && wants && !readable && !filtered_out {
                if may_hold {
                    s.rx_held = true;
                    return None;
                }
                // It waited HOLD_MS and no read came: whoever reads this
                // type has stopped (or died without closing). Forget them
                // until they queue a read again, so the next frames of the
                // type do not each hold the ring for HOLD_MS.
                s.read_types.retain(|&(t, _)| t != ptype);
            }
            s.rx_held = false;
            if !s.online {
                return Some(Taken::new());
            }
            let promisc = s.promisc_openers > 0;
            if !promisc {
                if mcast && !s.mcast_wanted(&dst) {
                    s.stats.mcast_rejects += 1;
                    return Some(Taken::new());
                }
                if !bcast && !mcast && dst != s.station {
                    s.stats.rx_filtered += 1;
                    return Some(Taken::new());
                }
            }
            // Counted once past the address filter, as a card that filters
            // in hardware would (the filter counters keep the others).
            s.stats.rx_packets += 1;
            s.stats.rx_bytes += len as u64;
            sample_update(s, now, 0, len);
            type_stats(s, ptype, |st| {
                st.PacketsReceived += 1;
                st.BytesReceived += len;
            });
            let mut taken = Taken::new();
            // One read per opener: once every opener has one, stop looking.
            let openers = s.openers.len();
            let mut i = 0;
            while i < s.reads.len() && taken.len() < openers {
                let rd = s.reads[i];
                if fits(&rd) && !taken.has(rd.op) {
                    taken.push((Pick::Read(rd), rd.op));
                    s.reads.remove(i);
                } else {
                    i += 1;
                }
            }
            if taken.is_empty() {
                let mut i = 0;
                while i < s.orphans.len() {
                    let r = s.orphans[i];
                    let op = r.buffer_management() as *mut Opener;
                    if !taken.has(op) {
                        taken.push((Pick::Orphan(r), op));
                        s.orphans.remove(i);
                    } else {
                        i += 1;
                    }
                }
                s.stats.unknown_types += 1;
                if taken.is_empty() {
                    s.stats.rx_no_read += 1;
                    type_stats(s, ptype, |st| st.PacketsDropped += 1);
                }
            }
            Some(taken)
        });
        let Some(taken) = taken else { return false };
        for (pick, op) in taken.iter() {
            let bm = &(*op).bm;
            // The request the frame's fields go into, and the one whose
            // io_Flags (SANA2IOF_RAW) apply.
            let (r, base) = match pick {
                Pick::Read(rd) => match rd.batch {
                    Some(b) => (frame_req(b.next), rd.r),
                    None => (rd.r, rd.r),
                },
                Pick::Orphan(r) => (r, r),
            };
            let raw = base.io().flags() & SANA2IOF_RAW != 0;
            let (data, dlen) = if raw { (frame.as_ptr(), len) } else { (frame.as_ptr().add(eth::HLEN), len - eth::HLEN as u32) };
            r.set_src_mac(&src);
            r.set_dst_mac(&dst);
            r.set_packet_type(ptype);
            r.set_data_length(dlen);
            let io = r.io();
            let mut flags = if r == base { io.flags() & !(SANA2IOF_BCAST | SANA2IOF_MCAST) } else { base.io().flags() & SANA2IOF_RAW };
            if bcast {
                flags |= SANA2IOF_BCAST;
            } else if mcast {
                flags |= SANA2IOF_MCAST;
            }
            io.set_flags(flags);
            // S2_PacketFilter is for CMD_READ (and the CMD_READ-like frames
            // of a read batch); an orphan read gets everything.
            if let Pick::Read(rd) = pick {
                if !bm.filter(r, data) {
                    // The stack does not want this one: the read stays pending.
                    self.u.shared.with(|s| reinsert(s, rd));
                    continue;
                }
            }
            let copied = bm.copy_to_buff(r, r.data(), data, dlen);
            if !copied {
                signal_events(self.u, S2EVENT_ERROR | S2EVENT_RX | S2EVENT_BUFF);
            }
            match pick {
                Pick::Read(mut rd) if rd.batch.is_some() => {
                    let mut b = rd.batch.unwrap();
                    if copied {
                        r.set_wire_error(0);
                        io.set_error(0);
                    } else {
                        r.set_wire_error(S2WERR_BUFF_ERROR);
                        io.set_error(S2ERR_NO_RESOURCES);
                    }
                    // The frame's items: the checksum result, if asked for
                    // (types the driver does not know are stepped over).
                    let items = core::ptr::addr_of!((*(b.next as *mut x::S2XFrame)).s2xf_Items);
                    if let Some(c) = x::find::<x::S2XFrameChecksum>(items, x::S2XFRAME_CHECKSUM) {
                        core::ptr::addr_of_mut!((*c).s2xfc_Flags).write_unaligned(csum);
                    }
                    // And the RSS hash (type 0: not hashed).
                    if let Some(h) = x::find::<x::S2XFrameRSS>(items, x::S2XFRAME_RSS) {
                        core::ptr::addr_of_mut!((*h).s2xfr_Hash).write_unaligned(if rss.1 != 0 { rss.0 } else { 0 });
                        core::ptr::addr_of_mut!((*h).s2xfr_HashType).write_unaligned(rss.1);
                    }
                    b.filled += 1;
                    b.next = next_frame(b.next);
                    rd.batch = Some(b);
                    if b.filled == b.max {
                        rd.end(0, 0);
                    } else {
                        self.u.shared.with(|s| reinsert(s, rd));
                        self.touched.push(rd.r);
                    }
                }
                _ => {
                    if copied {
                        r.reply(0, 0);
                    } else {
                        r.reply(S2ERR_NO_RESOURCES, S2WERR_BUFF_ERROR);
                    }
                }
            }
        }
        true
    }

    /// The end of a receive pass: every read batch that took frames in it
    /// and is still posted is replied (it takes what is waiting, at least
    /// one frame). One that was replied meanwhile (full, aborted) is gone
    /// from the list, or back with nothing filled if the stack posted it
    /// again: both are left alone.
    unsafe fn end_touched(&mut self) {
        if self.touched.is_empty() {
            return;
        }
        let touched = core::mem::take(&mut self.touched);
        let done: Vec<Rd> = self.u.shared.with(|s| {
            let mut out = Vec::new();
            for r in touched.iter() {
                if let Some(pos) = s.reads.iter().position(|x| x.r == *r && x.batch.map_or(false, |b| b.filled > 0)) {
                    out.extend(s.reads.remove(pos));
                }
            }
            out
        });
        for rd in done {
            rd.end(0, 0);
        }
        self.touched = touched;
        self.touched.clear();
    }

    /// A link status change (LSC).
    unsafe fn config_changed(&mut self) {
        let (up, bps) = self.r.link();
        let was = self.u.shared.with(|s| {
            if up {
                s.bps = bps;
            }
            core::mem::replace(&mut s.link_up, up)
        });
        if was != up {
            if up {
                let fd = self.r.r(reg::STATUS) & bit::STATUS_FD != 0;
                serial_println!("{} unit {}: link up, {} Mbit/s {} duplex", NAME, self.u.index(), bps / 1_000_000, if fd { "full" } else { "half" });
            } else {
                serial_println!("{} unit {}: link down", NAME, self.u.index());
            }
            if up {
                let mode = self.fc_apply(true);
                self.u.shared.with(|s| s.fc_mode = mode);
                serial_println!("{} unit {}: {}", NAME, self.u.index(), mode);
            }
            signal_events(self.u, if up { S2EVENT_CONNECT } else { S2EVENT_DISCONNECT | S2EVENT_ERROR });
        }
    }
}

// ---------------------------------------------------------------------------
// Transmit, in the caller's task (BeginIO) or the unit task, under the unit's
// transmit lock
// ---------------------------------------------------------------------------
//
// A write is copied into a free slot and posted in BeginIO itself, in the
// stack's task: no hand-off to the unit task, so no two task switches per
// frame. Only when every slot is in flight does it wait (marked pending) in
// the ring's own list; the unit task sends those as slots come back, on the
// transmit interrupt. Lock order: the transmit lock, then the shared state;
// never the other way round.
//
// An S2X_WRITEBATCH is sent the same way, frame by frame in list order with
// one tail write for all; when slots run out part-way it waits in the same
// list (after the frames already placed are kicked), and the unit task goes
// on with it from the frame it stopped at.

/// A write waiting for a slot: a CMD_WRITE / S2_BROADCAST / S2_MULTICAST,
/// or a write batch with how far it got.
#[derive(Clone, Copy)]
enum Tx {
    One(S2Req),
    Batch(TxBatch),
}

#[derive(Clone, Copy)]
struct TxBatch {
    r: S2Req,
    /// The next frame to send.
    next: *mut MinNode,
    /// Frames still to send, and frames taken (s2xb_Done).
    left: u32,
    done: u32,
}

impl Tx {
    fn req(&self) -> S2Req {
        match self {
            Tx::One(r) => *r,
            Tx::Batch(b) => b.r,
        }
    }

    /// Reply it with an error (abort, offline, close): a batch with the
    /// frames already taken counted.
    ///
    /// # Safety
    /// A request the driver holds (taken out of the list).
    unsafe fn end(self, error: i8, wire: u32) {
        // SAFETY: the caller's contract.
        unsafe {
            if let Tx::Batch(b) = self {
                set_done(b.r, b.done);
            }
            self.req().reply(error, wire);
        }
    }
}

/// What TxRing::put did with a frame.
enum Put {
    Sent,
    /// Every slot is in flight: nothing done.
    Full,
    /// Refused (io_Error, ios2_WireError): nothing sent.
    Refused(i8, u32),
}

struct TxRing {
    // Mappings before their buffers: they drop (EndDMA) first.
    _ring_map: Option<DmaMapping<'static>>,
    _pool_map: Option<DmaMapping<'static>>,
    ring: DmaBuffer,
    bufs: DmaBuffer,
    ring_phys: u32,
    phys: u32,
    n: usize,
    /// TDT: the next descriptor to fill.
    tail: usize,
    /// The oldest descriptor the card may still own.
    clean: usize,
    /// Descriptors filled and the card not yet told.
    unkicked: bool,
    /// Writes waiting for a slot (marked pending: replied when sent).
    pending: VecDeque<Tx>,
    r: Regs,
    /// The transmit head when last sampled with descriptors waiting, and
    /// since when (timebase) it has not moved.
    stall_head: usize,
    stall_since: u64,
    /// Test only (E1000E_CMD_TXMUTE): the tail register is not written.
    mute: bool,
    /// TDH reads, added to perf::TDH_READS by the unit task.
    head_reads: u32,
    /// Bytes per transmit buffer.
    buf_size: usize,
    /// Frames filled since the last tail write (batching).
    unkicked_n: u32,
    /// The last frame filled was a jumbo frame (batching never holds one).
    last_big: bool,
    /// Segmentation's buffers (TSO_BUFS of TSO_BUF bytes; mapping before
    /// buffer: it drops first), their physical address, and for each the
    /// data descriptor that last used it (free once that one is done).
    _tso_map: Option<DmaMapping<'static>>,
    tso: Option<DmaBuffer>,
    tso_phys: u32,
    tso_slot: [Option<usize>; TSO_BUFS],
}

impl TxRing {
    /// Whether descriptor `i` may still be the card's (between the oldest
    /// not taken back and the tail).
    fn in_flight(&self, i: usize) -> bool {
        (i + self.n - self.clean) % self.n < (self.tail + self.n - self.clean) % self.n
    }

    /// A segmentation buffer no descriptor in flight uses.
    fn free_tso(&self) -> Option<usize> {
        self.tso.as_ref()?;
        (0..TSO_BUFS).find(|&k| self.tso_slot[k].map_or(true, |d| !self.in_flight(d)))
    }

    /// Point the card at the (empty) ring and enable transmit.
    unsafe fn start(&mut self) {
        self.tso_slot = [None; TSO_BUFS];
        self.tail = 0;
        self.clean = 0;
        self.unkicked = false;
        self.unkicked_n = 0;
        self.stall_head = usize::MAX;
        self.stall_since = 0;
        self.mute = false;
        self.r.tx_init(self.ring_phys, self.n);
    }

    /// The transmit watchdog (the unit task's health check): true when
    /// descriptors have waited TX_STALL_MS with the card's head not moving.
    unsafe fn stalled(&mut self, tb: u64, limit: u64) -> bool {
        if self.tail == self.clean {
            self.stall_head = usize::MAX;
            return false;
        }
        self.reclaim();
        if self.tail == self.clean {
            self.stall_head = usize::MAX;
            return false;
        }
        if self.stall_head != self.clean {
            self.stall_head = self.clean;
            self.stall_since = tb;
            return false;
        }
        tb.wrapping_sub(self.stall_since) > limit
    }

    /// Take back every slot the card is done with: those before its head.
    /// Slots free for new frames (one always stays empty).
    fn free_slots(&self) -> usize {
        self.n - 1 - (self.tail + self.n - self.clean) % self.n
    }

    unsafe fn reclaim(&mut self) -> u32 {
        self.head_reads += 1;
        let head = self.r.r(reg::TDH) as usize;
        if head >= self.n {
            return 0;
        }
        let done = (head + self.n - self.clean) % self.n;
        self.clean = head;
        done as u32
    }

    /// Send one CMD_WRITE / S2_BROADCAST / S2_MULTICAST, ending it with
    /// `finish` (quick-aware; a pending request is not quick). Returns false
    /// when every slot is in flight (the request is not ended).
    unsafe fn send(&mut self, u: &Unit, r: S2Req, now: TimeVal) -> bool {
        match self.put(u, r, r.io().command(), r.buffer_management() as *mut Opener, now, None, None) {
            Put::Sent => r.finish(0, 0),
            Put::Full => return false,
            Put::Refused(e, w) => r.finish(e, w),
        }
        true
    }

    /// Go on with a write batch: send its frames in list order until one
    /// is refused, the slots run out, or all are sent. True: the batch is
    /// ended (replied, quick-aware, with s2xb_Done); false: it waits for
    /// slots. A refused frame's request gets the error and so does the
    /// batch; the frames after it are left untouched.
    unsafe fn send_batch(&mut self, u: &Unit, b: &mut TxBatch, now: TimeVal) -> bool {
        let op = b.r.buffer_management() as *mut Opener;
        while b.left > 0 {
            let fr = frame_req(b.next);
            // A frame is a CMD_WRITE unless its request says broadcast or
            // multicast (its io_Command is not otherwise used).
            let cmd = match fr.io().command() {
                S2_BROADCAST => S2_BROADCAST,
                S2_MULTICAST => S2_MULTICAST,
                _ => CMD_WRITE,
            };
            match self.put(u, fr, cmd, op, now, tx_csum_item(b.next), tx_seg_item(b.next)) {
                Put::Sent => {
                    fr.io().set_error(0);
                    fr.set_wire_error(0);
                    b.done += 1;
                    b.left -= 1;
                    b.next = next_frame(b.next);
                }
                Put::Full => return false,
                Put::Refused(e, w) => {
                    fr.io().set_error(e);
                    fr.set_wire_error(w);
                    set_done(b.r, b.done);
                    b.r.finish(e, w);
                    return true;
                }
            }
        }
        set_done(b.r, b.done);
        b.r.finish(0, 0);
        true
    }

    /// Place one frame (request `r`, command `cmd`, copied through opener
    /// `op`'s buffer management) in the ring, without replying anything.
    ///
    /// `csum` and `seg` are a write batch frame's checksum and segmentation
    /// requests (none for CMD_WRITE).
    unsafe fn put(&mut self, u: &Unit, r: S2Req, cmd: u16, op: *mut Opener, now: TimeVal, csum: Option<TxCsum>, seg: Option<TxSeg>) -> Put {
        let io = r.io();
        let (online, configured, station, mtu) = u.shared.with(|s| (s.online, s.configured, s.station, s.mtu));
        if !configured {
            return Put::Refused(S2ERR_BAD_STATE, S2WERR_NOT_CONFIGURED);
        }
        if !online {
            return Put::Refused(S2ERR_OUTOFSERVICE, S2WERR_UNIT_OFFLINE);
        }
        if u.out_of_service.load(Ordering::Acquire) != 0 {
            // Resting between recovery tries: the ring is not being served.
            return Put::Refused(S2ERR_OUTOFSERVICE, S2WERR_RCVREL_HDW_ERR);
        }
        let raw = io.flags() & SANA2IOF_RAW != 0;
        let dlen = r.data_length() as usize;
        let frame_len = if raw { dlen } else { eth::HLEN + dlen };
        if seg.is_some() {
            // A segmented send: larger than the MTU allowed, up to
            // TSO_MAX_DATA, with a TCP checksum request (the spec's rule).
            if self.tso.is_none() {
                return Put::Refused(S2ERR_NOT_SUPPORTED, S2WERR_GENERIC_ERROR);
            }
            if dlen > TSO_MAX_DATA as usize || (raw && dlen < eth::HLEN) {
                return Put::Refused(S2ERR_MTU_EXCEEDED, S2WERR_GENERIC_ERROR);
            }
            if !csum.map_or(false, |c| c.flags & x::S2XCS_FILL_L4 != 0) {
                return Put::Refused(S2ERR_BAD_ARGUMENT, S2WERR_GENERIC_ERROR);
            }
        } else if (raw && (dlen < eth::HLEN || dlen > eth::HLEN + mtu as usize)) || (!raw && dlen > mtu as usize) {
            return Put::Refused(S2ERR_MTU_EXCEEDED, S2WERR_GENERIC_ERROR);
        }
        let dst = match cmd {
            S2_BROADCAST => eth::BROADCAST,
            _ => r.dst_mac(),
        };
        if cmd == S2_MULTICAST && (!eth::is_multicast(&dst) || dst == eth::BROADCAST) {
            return Put::Refused(S2ERR_BAD_ADDRESS, S2WERR_BAD_MULTICAST);
        }
        // A frame with an offload takes two descriptors: a context
        // descriptor (slot i, its buffer unused) and the data descriptor
        // (slot i + 1, with that slot's buffer), so buffers stay one per
        // descriptor.
        // A segmented send goes into a segmentation buffer instead (with its
        // own context descriptor too).
        let offload = seg.is_some() || csum.map_or(false, |c| c.flags & (x::S2XCS_FILL_IP | x::S2XCS_FILL_L4) != 0);
        let i = self.tail;
        let need = if offload { 2 } else { 1 };
        if self.free_slots() < need {
            return Put::Full;
        }
        let j = if offload { (i + 1) % self.n } else { i };
        let (f, phys, tso_k) = if seg.is_some() {
            let k = match self.free_tso() {
                Some(k) => k,
                None => {
                    self.reclaim();
                    match self.free_tso() {
                        Some(k) => k,
                        None => return Put::Full,
                    }
                }
            };
            let Some(pool) = self.tso.as_mut() else { return Put::Full };
            (pool.as_mut_ptr().add(k * TSO_BUF), self.tso_phys + (k * TSO_BUF) as u32, Some(k))
        } else {
            if frame_len.max(60) > self.buf_size {
                return Put::Refused(S2ERR_MTU_EXCEEDED, S2WERR_GENERIC_ERROR);
            }
            (self.bufs.as_mut_ptr().add(j * self.buf_size), self.phys + (j * self.buf_size) as u32, None)
        };
        let payload = if raw {
            f
        } else {
            core::ptr::copy_nonoverlapping(dst.as_ptr(), f, 6);
            core::ptr::copy_nonoverlapping(station.as_ptr(), f.add(6), 6);
            let t = (r.packet_type() as u16).to_be_bytes();
            *f.add(12) = t[0];
            *f.add(13) = t[1];
            f.add(eth::HLEN)
        };
        if !(*op).bm.copy_from_buff(r, payload, r.data(), dlen as u32) {
            signal_events(u, S2EVENT_ERROR | S2EVENT_TX | S2EVENT_BUFF);
            return Put::Refused(S2ERR_NO_RESOURCES, S2WERR_BUFF_ERROR);
        }
        // The checksums asked for: planned for the card, or (what it
        // cannot express) computed here, before the buffer leaves the cache.
        let fr = core::slice::from_raw_parts_mut(f, frame_len);
        let plan = match (seg, csum) {
            (Some(s), Some(c)) => match tso_plan(fr, raw, s, c, mtu) {
                Some(p) => Some(p),
                None => return Put::Refused(S2ERR_BAD_ARGUMENT, S2WERR_GENERIC_ERROR),
            },
            (None, Some(c)) if offload => match tx_offload(fr, raw, c) {
                Some(p) => Some(p),
                None => return Put::Refused(S2ERR_BAD_ARGUMENT, S2WERR_GENERIC_ERROR),
            },
            _ => None,
        };
        // A plain frame is padded to the 60-byte Ethernet minimum here
        // (zeros keep old buffer data off the wire). A frame with an
        // offload (checksums, segmentation) goes to the card at exactly its
        // length and the card pads it on the wire (TCTL.PSP), as Intel's
        // drivers do: the card works from the data it fetched, and QEMU's
        // emulated cards, inserting the IPv4 header checksum, rewrite the
        // IP total length as everything fetched after the IP header and
        // sum the transport checksum over it. With the padding included, a 54-byte
        // TCP ACK left as a segment with 6 bytes of zeros (the peer took
        // them as data) or with a wrong checksum.
        let wire = if offload { frame_len } else { frame_len.max(60) };
        if wire > frame_len {
            core::ptr::write_bytes(f.add(frame_len), 0, wire - frame_len);
        }
        arch::cache_flush(f, wire);
        if let Some(k) = tso_k {
            self.tso_slot[k] = Some(j);
        }
        match plan {
            Some(p) => {
                let c = self.ring.as_mut_ptr().add(i * hw::DESC_SIZE);
                hw::tx_ctx_set(c, eth::HLEN as u32, p.iplen, p.tucmd, p.mss, p.l4len, p.paylen);
                let d = self.ring.as_mut_ptr().add(j * hw::DESC_SIZE);
                hw::tx_desc_set_offload(d, phys, wire as u32, p.popts, p.mss != 0);
                // Both descriptors (they may lie in two cache lines).
                arch::cache_flush(c, hw::DESC_SIZE);
                arch::cache_flush(d, hw::DESC_SIZE);
                u.count(perf::TX_OFFLOADS);
            }
            None => {
                let d = self.ring.as_mut_ptr().add(i * hw::DESC_SIZE);
                hw::tx_desc_set(d, phys, wire as u16);
                arch::cache_flush(d, hw::DESC_SIZE);
            }
        }
        self.tail = (j + 1) % self.n;
        self.unkicked = true;
        self.unkicked_n += 1;
        self.last_big = frame_len > eth::HLEN + eth::MTU;
        if self.last_big {
            u.jumbo_tx.fetch_add(1, Ordering::Relaxed);
        }
        u.count(perf::TX_PUBLISH);
        let ptype = if raw { u16::from_be_bytes([*f.add(12), *f.add(13)]) as u32 } else { r.packet_type() };
        if pure_ack(core::slice::from_raw_parts(f, frame_len)) {
            u.count(perf::TX_ACKS);
        }
        u.shared.with(|s| {
            s.stats.tx_packets += 1;
            s.stats.tx_bytes += frame_len as u64;
            sample_update(s, now, frame_len as u32, 0);
            type_stats(s, ptype, |st| {
                st.PacketsSent += 1;
                st.BytesSent += frame_len as u32;
            });
        });
        Put::Sent
    }

    /// Tell the card about the filled descriptors (the tail register).
    unsafe fn push_out(&mut self, u: &Unit, notify: bool) {
        if notify && self.unkicked && !self.mute {
            if u.tune.load(Ordering::Relaxed) & TUNE_TX_BATCH != 0
                && u.tx_idle.load(Ordering::Acquire) == 0
                && self.unkicked_n < TX_BATCH_MAX
                && !self.last_big
            {
                // The card is still sending: these frames go with the next
                // tail write (at TXQE, through the unit task, or once
                // TX_BATCH_MAX wait). Only normal-size frames wait: a few
                // jumbo frames fill a TCP window long before TX_BATCH_MAX,
                // and each burst would then wait for the TXQE interrupt
                // (MTU 9000 between two QEMU machines fell from ~15 to
                // ~2 MB/s). A jumbo frame sends at once, with what waits.
                u.count(perf::TX_DEFERRED);
                return;
            }
            u.tx_idle.store(0, Ordering::Release);
            self.unkicked_n = 0;
            self.unkicked = false;
            let t0 = arch::timebase();
            arch::sync_barrier();
            self.r.w(reg::TDT, self.tail as u32);
            u.add_time(perf::TX_NOTIFY_T, t0);
            u.count(perf::TX_NOTIFY);
        }
    }

    /// Send what waits for a slot, then push everything out.
    unsafe fn flush(&mut self, u: &Unit, now: TimeVal) {
        while let Some(t) = self.pending.pop_front() {
            match t {
                Tx::One(r) => {
                    if !self.send(u, r, now) {
                        self.pending.push_front(Tx::One(r));
                        break;
                    }
                }
                Tx::Batch(mut b) => {
                    if !self.send_batch(u, &mut b, now) {
                        self.pending.push_front(Tx::Batch(b));
                        break;
                    }
                }
            }
        }
        self.push_out(u, true);
    }
}

/// BeginIO's S2X_WRITEBATCH (checked): send it at once in the caller's
/// task, as far as slots allow; the rest waits for the unit task.
unsafe fn tx_batch_from_caller(u: &Unit, r: S2Req, first: *mut MinNode, count: u32) {
    let now = u.now();
    let mut b = TxBatch { r, next: first, left: count, done: 0 };
    let state = u.tx.with(|tx| {
        let Some(tx) = tx.as_mut() else { return None };
        if u.tune.load(Ordering::Relaxed) & TUNE_LAZY_RECLAIM == 0 || tx.free_slots() < (count as usize).max(TX_RECLAIM_BELOW) {
            tx.reclaim();
        }
        if tx.pending.is_empty() && tx.send_batch(u, &mut b, now) {
            tx.push_out(u, true);
            return Some(true);
        }
        // Frames already placed go to the card now, so their slots come
        // back while the rest waits.
        tx.push_out(u, true);
        r.io().mark_pending();
        tx.pending.push_back(Tx::Batch(b));
        Some(false)
    });
    match state {
        None => r.finish(IOERR_UNITBUSY, 0),
        Some(true) => {
            if rx_peek(u) {
                if u.tune.load(Ordering::Relaxed) & TUNE_RX_IN_WRITER != 0 && rx_in_writer(u) {
                    u.count(perf::RX_BY_WRITER);
                } else {
                    u.count(perf::PEEK_WAKES);
                    u.worker.wake();
                }
            }
        }
        Some(false) => {
            u.shared.with(|s| s.stats.tx_waits += 1);
            u.worker.wake();
        }
    }
}

/// BeginIO's CMD_WRITE / S2_BROADCAST / S2_MULTICAST: send at once in the
/// caller's task, or leave it for the unit task when every slot is busy.
unsafe fn tx_from_caller(u: &Unit, r: S2Req) {
    let t0 = arch::timebase();
    let now = u.now();
    let state = u.tx.with(|tx| {
        let Some(tx) = tx.as_mut() else { return None };
        if u.tune.load(Ordering::Relaxed) & TUNE_LAZY_RECLAIM == 0 || tx.free_slots() < TX_RECLAIM_BELOW {
            tx.reclaim();
        }
        if tx.pending.is_empty() && tx.send(u, r, now) {
            tx.push_out(u, true);
            return Some(true);
        }
        r.io().mark_pending();
        tx.pending.push_back(Tx::One(r));
        Some(false)
    });
    match state {
        None => r.finish(IOERR_UNITBUSY, 0),
        Some(true) => {
            if rx_peek(u) {
                if u.tune.load(Ordering::Relaxed) & TUNE_RX_IN_WRITER != 0 && rx_in_writer(u) {
                    u.count(perf::RX_BY_WRITER);
                } else {
                    u.count(perf::PEEK_WAKES);
                    u.worker.wake();
                }
            }
        }
        Some(false) => {
            u.shared.with(|s| s.stats.tx_waits += 1);
            u.worker.wake();
        }
    }
    u.add_time(perf::TX_CALL_T, t0);
}

/// Whether the next receive descriptor holds a frame the unit task has not
/// taken yet. Called after each write in the caller's task: under load the
/// answers to what was sent (TCP ACKs) are taken at once instead of at the
/// next interrupt, which the card may hold back (interrupt throttling;
/// QEMU's e1000e never throttles less than 128 us, and its timer runs late
/// on some hosts). Read only: one descriptor's status byte.
unsafe fn rx_peek(u: &Unit) -> bool {
    let base = u.rx_ring.load(Ordering::Acquire);
    if base.is_null() || u.rx_holding.load(Ordering::Acquire) != 0 {
        return false;
    }
    let i = u.rx_next.load(Ordering::Acquire) as usize % RX_BUFFERS;
    let d = base.add(i * hw::DESC_SIZE);
    arch::cache_invalidate(d, hw::DESC_SIZE);
    hw::rx_desc_get(d).1 & bit::RXD_STAT_DD != 0
}

/// The writer takes the received frames itself, if the unit task is not
/// busy (it holds the receive lock for each pass). False: it is busy, or
/// the unit is not running.
unsafe fn rx_in_writer(u: &Unit) -> bool {
    let m = u.rxl.mutex.load(Ordering::Acquire) as sys::APTR;
    if m.is_null() || sys::exec_mutex_attempt(m) == 0 {
        return false;
    }
    let mut done = false;
    if u.regs_ready.load(Ordering::Acquire) != 0 && u.running.load(Ordering::Acquire) != 0 {
        if let Some(net) = (*u.net.get()).as_mut() {
            net.rx(u.now());
            done = true;
            if net.any_held() {
                // A frame waits for a read: the unit task keeps the hold
                // timer, so it must look.
                u.worker.wake();
            }
        }
    }
    sys::exec_mutex_release(m);
    done
}

/// The unit task's share: take back slots, send what waited.
unsafe fn tx_service(u: &Unit, now: TimeVal) {
    u.tx.with(|tx| {
        if let Some(tx) = tx.as_mut() {
            // Lazy: the head is read only when writes wait for a slot or
            // slots run low; otherwise the slots come back at the next read.
            let lazy = u.tune.load(Ordering::Relaxed) & TUNE_LAZY_RECLAIM != 0;
            if !lazy || !tx.pending.is_empty() || tx.free_slots() < TX_RECLAIM_BELOW {
                tx.reclaim();
            }
            tx.flush(u, now);
            u.perf[perf::TDH_READS].fetch_add(core::mem::take(&mut tx.head_reads), Ordering::Relaxed);
        }
    });
}

// ---------------------------------------------------------------------------
// Commands that need the device (the unit task)
// ---------------------------------------------------------------------------

/// Abort every pending read and orphan read, and what waits to be sent.
unsafe fn abort_traffic(u: &Unit, net: &mut Net, error: i8, wire: u32) {
    // Read batches touched in a receive pass are replied at its end; the
    // unit task (the only caller) is between passes.
    net.touched.clear();
    let (reads, orphans) = u.shared.with(|s| (core::mem::take(&mut s.reads), core::mem::take(&mut s.orphans)));
    for rd in reads {
        rd.end(error, wire);
    }
    for r in orphans {
        r.reply(error, wire);
    }
    let pending = u.tx.with(|tx| tx.as_mut().map(|t| core::mem::take(&mut t.pending)).unwrap_or_default());
    for t in pending {
        t.end(error, wire);
    }
}

unsafe fn serve(u: &'static Unit, net: &mut Net, r: S2Req, timer: &mut AmigaTimer) {
    let now = timer.get_sys_time();
    let now = TimeVal { Seconds: now.secs, Microseconds: now.micros };
    match r.io().command() {
        S2_CONFIGINTERFACE => {
            let addr = r.src_mac();
            let res = u.shared.with(|s| {
                if s.configured {
                    return Err((S2ERR_BAD_STATE, S2WERR_IS_CONFIGURED));
                }
                if eth::is_multicast(&addr) || addr == [0; 6] {
                    return Err((S2ERR_BAD_ADDRESS, S2WERR_SRC_ADDRESS));
                }
                s.station = addr;
                s.configured = true;
                s.online = true;
                s.last_start = now;
                Ok(())
            });
            match res {
                Ok(()) => {
                    net.apply_rx_mode();
                    serial_println!("{} unit {}: configured, station {}, online", NAME, u.index(), hex_mac(&addr));
                    r.set_src_mac(&addr);
                    r.reply(0, 0);
                    signal_events(u, S2EVENT_ONLINE | S2EVENT_CONFIGCHANGED);
                }
                Err((e, w)) => r.reply(e, w),
            }
        }
        S2_ONLINE => {
            let res = u.shared.with(|s| {
                if !s.configured {
                    return Err((S2ERR_BAD_STATE, S2WERR_NOT_CONFIGURED));
                }
                let was = s.online;
                s.online = true;
                if !was {
                    let keep = (s.stats.recoveries, s.stats.reconfigurations + 1);
                    s.stats = Stats::default();
                    s.stats.recoveries = keep.0;
                    s.stats.reconfigurations = keep.1;
                    s.last_start = now;
                }
                Ok(was)
            });
            match res {
                Ok(was) => {
                    r.reply(0, 0);
                    if !was {
                        serial_println!("{} unit {}: online", NAME, u.index());
                        signal_events(u, S2EVENT_ONLINE);
                    }
                }
                Err((e, w)) => r.reply(e, w),
            }
        }
        S2_OFFLINE => {
            let was = u.shared.with(|s| core::mem::replace(&mut s.online, false));
            if was {
                abort_traffic(u, net, S2ERR_OUTOFSERVICE, S2WERR_UNIT_OFFLINE);
                serial_println!("{} unit {}: offline", NAME, u.index());
            }
            r.reply(0, 0);
            if was {
                signal_events(u, S2EVENT_OFFLINE);
            }
        }
        S2_ADDMULTICASTADDRESS | S2_DELMULTICASTADDRESS | S2_ADDMULTICASTADDRESSES | S2_DELMULTICASTADDRESSES => {
            let cmd = r.io().command();
            let lo = r.src_mac();
            let hi = if cmd == S2_ADDMULTICASTADDRESSES || cmd == S2_DELMULTICASTADDRESSES { r.dst_mac() } else { lo };
            let (l, h) = (mac_u64(&lo), mac_u64(&hi));
            if !eth::is_multicast(&lo) || !eth::is_multicast(&hi) || h < l {
                r.reply(S2ERR_BAD_ADDRESS, S2WERR_BAD_MULTICAST);
                return;
            }
            let add = cmd == S2_ADDMULTICASTADDRESS || cmd == S2_ADDMULTICASTADDRESSES;
            let res = u.shared.with(|s| {
                if h - l + 1 > MCAST_RANGE_MAX {
                    if add {
                        match s.mcast_wide.iter_mut().find(|e| e.0 == l && e.1 == h) {
                            Some(e) => e.2 += 1,
                            None => s.mcast_wide.push((l, h, 1)),
                        }
                    } else if let Some(pos) = s.mcast_wide.iter().position(|e| e.0 == l && e.1 == h) {
                        s.mcast_wide[pos].2 -= 1;
                        if s.mcast_wide[pos].2 == 0 {
                            s.mcast_wide.remove(pos);
                        }
                    }
                    return Ok(());
                }
                for v in l..=h {
                    let mac = u64_mac(v);
                    if add {
                        match s.mcast.iter_mut().find(|e| e.0 == mac) {
                            Some(e) => e.1 += 1,
                            None => {
                                if s.mcast.len() >= 4 * MCAST_TABLE_MAX {
                                    return Err((S2ERR_NO_RESOURCES, S2WERR_MULTICAST_FULL));
                                }
                                s.mcast.push((mac, 1));
                            }
                        }
                    } else if let Some(pos) = s.mcast.iter().position(|e| e.0 == mac) {
                        s.mcast[pos].1 -= 1;
                        if s.mcast[pos].1 == 0 {
                            s.mcast.remove(pos);
                        }
                    }
                }
                Ok(())
            });
            match res {
                Ok(()) => {
                    net.apply_rx_mode();
                    r.reply(0, 0);
                }
                Err((e, w)) => r.reply(e, w),
            }
        }
        S2_SAMPLE_THROUGHPUT => {
            let st = r.stat_data() as *mut Sana2ThroughputStats;
            let len = core::ptr::addr_of!((*st).s2ts_Length).read_unaligned() as usize;
            let actual = len.min(core::mem::size_of::<Sana2ThroughputStats>());
            let mut fresh: Sana2ThroughputStats = core::mem::zeroed();
            fresh.s2ts_Length = len as u32;
            fresh.s2ts_Actual = actual as u32;
            // Keep the caller's notify task and mask.
            let keep = core::cmp::min(actual, 16);
            core::ptr::copy_nonoverlapping((st as *const u8).add(8), (&mut fresh as *mut _ as *mut u8).add(8), keep.saturating_sub(8));
            fresh.s2ts_StartTime = now;
            fresh.s2ts_EndTime = now;
            core::ptr::copy_nonoverlapping(&fresh as *const _ as *const u8, st as *mut u8, actual);
            u.shared.with(|s| s.samples.push(r));
        }
        CMD_FLUSH => {
            // Everything queued: reads, orphans, writes, and (unlike
            // S2_OFFLINE, which keeps them for the ONLINE event) events and
            // throughput samples.
            abort_traffic(u, net, IOERR_ABORTED, 0);
            let (ev, sa) = u.shared.with(|s| (core::mem::take(&mut s.events), core::mem::take(&mut s.samples)));
            for x in ev.into_iter().chain(sa) {
                x.reply(IOERR_ABORTED, 0);
            }
            r.reply(0, 0);
        }
        x::S2X_CONFIGURE => configure(u, net, r),
        _cmd => {
            #[cfg(feature = "diag")]
            if serve_diag(u, net, r) {
                return;
            }
            r.reply(IOERR_NOCMD, 0)
        }
    }
}

/// S2X_CONFIGURE (the unit task): read the settings (S2XCF_READ), or
/// check every item and then apply them in list order, writing back the
/// values applied. A refusal changes nothing and points s2xc_Failed at the
/// item: an unknown type (S2ERR_NOT_SUPPORTED), an item too short or a
/// value outside the capability (S2ERR_BAD_ARGUMENT), the MTU while a
/// receive request is posted (S2ERR_BAD_STATE). A change sends
/// S2EVENT_CONFIGCHANGED.
unsafe fn configure(u: &'static Unit, net: &mut Net, r: S2Req) {
    use core::ptr::{addr_of, addr_of_mut};
    let c = r.stat_data() as *mut x::S2XConfigure;
    if c.is_null() {
        r.reply(S2ERR_BAD_ARGUMENT, S2WERR_NULL_POINTER);
        return;
    }
    if (addr_of!((*c).s2xc_Size).read_unaligned() as usize) < core::mem::size_of::<x::S2XConfigure>() {
        r.reply(IOERR_BADLENGTH, S2WERR_BAD_STATDATA);
        return;
    }
    addr_of_mut!((*c).s2xc_Failed).write_unaligned(null_mut());
    let read = addr_of!((*c).s2xc_Flags).read_unaligned() & x::S2XCF_READ != 0;
    let list = addr_of!((*c).s2xc_Items);
    let refuse = |it: *mut x::S2XItem, e: i8, w: u32| {
        addr_of_mut!((*c).s2xc_Failed).write_unaligned(it);
        r.reply(e, w);
    };
    // Check everything first.
    let posted = u.shared.with(|s| !s.reads.is_empty() || !s.orphans.is_empty());
    for it in x::items(list) {
        let ty = addr_of!((*it).s2xi_Type).read_unaligned();
        let size = addr_of!((*it).s2xi_Size).read_unaligned() as usize;
        let need = match ty {
            x::S2XSET_MTU => core::mem::size_of::<x::S2XSetMTU>(),
            x::S2XSET_MODERATION => core::mem::size_of::<x::S2XSetModeration>(),
            x::S2XSET_FLOWCONTROL => core::mem::size_of::<x::S2XSetFlowControl>(),
            _ => {
                refuse(it, S2ERR_NOT_SUPPORTED, S2WERR_GENERIC_ERROR);
                return;
            }
        };
        if size < need {
            refuse(it, S2ERR_BAD_ARGUMENT, S2WERR_BAD_STATDATA);
            return;
        }
        if read {
            continue;
        }
        let bad = match ty {
            x::S2XSET_MTU => {
                let v = addr_of!((*(it as *mut x::S2XSetMTU)).s2xsm_MTU).read_unaligned();
                if posted {
                    // Buffers posted for the old MTU: the stack aborts its
                    // reads first.
                    refuse(it, S2ERR_BAD_STATE, S2WERR_GENERIC_ERROR);
                    return;
                }
                !(MTU_MIN..=JUMBO_MTU_MAX).contains(&v)
            }
            x::S2XSET_MODERATION => addr_of!((*(it as *mut x::S2XSetModeration)).s2xso_RxMicros).read_unaligned() > MODERATION_MAX_US,
            _ => addr_of!((*(it as *mut x::S2XSetFlowControl)).s2xsf_Modes).read_unaligned() & !(x::S2XFC_RX | x::S2XFC_TX | x::S2XFC_AUTO) != 0,
        };
        if bad {
            refuse(it, S2ERR_BAD_ARGUMENT, S2WERR_GENERIC_ERROR);
            return;
        }
    }
    // Read, or apply in list order; either way each item ends up holding
    // the value in force.
    let mut changed = false;
    for it in x::items(list) {
        match addr_of!((*it).s2xi_Type).read_unaligned() {
            x::S2XSET_MTU => {
                let p = addr_of_mut!((*(it as *mut x::S2XSetMTU)).s2xsm_MTU);
                let cur = u.shared.with(|s| s.mtu);
                if !read && p.read_unaligned() != cur {
                    let v = p.read_unaligned();
                    if let Err(e) = net.rebuild(v) {
                        serial_println!("{} unit {}: MTU {} not set: {}", NAME, u.index(), v, e);
                        refuse(it, S2ERR_NO_RESOURCES, S2WERR_GENERIC_ERROR);
                        if changed {
                            signal_events(u, S2EVENT_CONFIGCHANGED);
                        }
                        return;
                    }
                    serial_println!("{} unit {}: MTU {}", NAME, u.index(), v);
                    changed = true;
                }
                p.write_unaligned(u.shared.with(|s| s.mtu));
            }
            x::S2XSET_MODERATION => {
                let m = it as *mut x::S2XSetModeration;
                if !read {
                    // One interval for both directions (no S2XMOD_TX), in
                    // the card's 256 ns steps (rounded).
                    let us = addr_of!((*m).s2xso_RxMicros).read_unaligned();
                    let v = ((us * 125 + 16) / 32).min(0xFFFF);
                    if v != net.itr {
                        net.r.set_itr(v);
                        net.itr = net.r.itr();
                        changed = true;
                    }
                }
                let us = (net.itr * 32 + 62) / 125;
                addr_of_mut!((*m).s2xso_RxMicros).write_unaligned(us);
                addr_of_mut!((*m).s2xso_TxMicros).write_unaligned(us);
            }
            _ => {
                let f = it as *mut x::S2XSetFlowControl;
                if !read {
                    let want = addr_of!((*f).s2xsf_Modes).read_unaligned();
                    if want != net.fc_modes {
                        net.fc_modes = want;
                        // The advertisement (auto-negotiation restarts if it
                        // changes: the link goes down and up), then the
                        // mode: forced now, or resolved at link up.
                        let _ = net.r.phy_setup(fc_adv(want));
                        let (up, _) = net.r.link();
                        let mode = net.fc_apply(up);
                        u.shared.with(|s| s.fc_mode = mode);
                        serial_println!("{} unit {}: flow control 0x{:x}: {}", NAME, u.index(), want, mode);
                        changed = true;
                    }
                }
                addr_of_mut!((*f).s2xsf_Modes).write_unaligned(net.fc_modes);
            }
        }
    }
    r.reply(0, 0);
    if changed {
        signal_events(u, S2EVENT_CONFIGCHANGED);
    }
}

/// The diag build's private commands. False: not one of them.
#[cfg(feature = "diag")]
unsafe fn serve_diag(u: &'static Unit, net: &mut Net, r: S2Req) -> bool {
    match r.io().command() {
        E1000E_CMD_DIAG => {
            let text = diag_report(u, net);
            let buf = r.data() as *mut u8;
            let cap = r.data_length() as usize;
            if buf.is_null() || cap == 0 {
                r.reply(S2ERR_BAD_ARGUMENT, S2WERR_NULL_POINTER);
            } else {
                let n = text.len().min(cap - 1);
                core::ptr::copy_nonoverlapping(text.as_ptr(), buf, n);
                *buf.add(n) = 0;
                r.set_data_length(n as u32);
                r.reply(0, 0);
            }
        }
        E1000E_CMD_TUNE => {
            let v = r.wire_error();
            let old = u.tune.swap(v, Ordering::AcqRel);
            xlog!("unit {}: speed switches 0x{:x} (were 0x{:x})", u.index(), v, old);
            r.set_wire_error(old);
            r.reply(0, old);
        }
        E1000E_CMD_PRI => {
            let p = r.wire_error() as i32;
            let old = sys::exec_set_task_pri(sys::exec_find_task(null_mut()) as _, p.clamp(-20, 20) as i8);
            xlog!("unit {}: unit task priority {} (was {})", u.index(), p, old);
            r.reply(0, 0);
        }
        E1000E_CMD_ITR => {
            let v = r.wire_error() & 0xFFFF;
            net.r.w(reg::ITR, v);
            net.itr = v;
            xlog!("unit {}: ITR {} ({} interrupts/s at most)", u.index(), v, if v == 0 { 0 } else { 3_906_250 / v });
            r.reply(0, 0);
        }
        E1000E_CMD_TXMUTE => {
            u.tx.with(|tx| if let Some(t) = tx.as_mut() { t.mute = true });
            serial_println!("{} unit {}: test: transmit tail no longer written", NAME, u.index());
            r.reply(0, 0);
        }
        E1000E_CMD_TESTFAULT => {
            let c = net.r.r(reg::CTRL);
            net.r.w(reg::CTRL, c | bit::CTRL_RST);
            serial_println!("{} unit {}: test fault: card reset behind the driver's back", NAME, u.index());
            r.reply(0, 0);
        }
        _ => return false,
    }
    true
}

/// The timebase frequency (exec.doc GetCPUInfo, GCIT_TimeBaseSpeed).
fn timebase_hz() -> u64 {
    let mut hz: u64 = 0;
    // SAFETY: GCIT_TimeBaseSpeed writes one uint64.
    unsafe {
        let tags = [
            TagItem { ti_Tag: sys::GCIT_TIMEBASE_SPEED, ti_Data: &mut hz as *mut u64 as usize as u32 },
            TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 },
        ];
        sys::exec_get_cpuinfo(tags.as_ptr());
    }
    hz.max(1)
}

// ---------------------------------------------------------------------------
// The interrupt server and reset callback
// ---------------------------------------------------------------------------

/// The PCI Express capability's offset in config space, if the card has one.
unsafe fn pcie_cap(dev: *mut PCIDevice) -> Option<u32> {
    // Status bit 4: a capability list.
    if pci!(dev, ReadConfigWord, 0x06) & 0x10 == 0 {
        return None;
    }
    let mut p = (pci!(dev, ReadConfigByte, 0x34) & 0xFC) as u32;
    let mut hops = 0;
    while p >= 0x40 && hops < 48 {
        if pci!(dev, ReadConfigByte, p) == 0x10 {
            return Some(p);
        }
        p = (pci!(dev, ReadConfigByte, p + 1) & 0xFC) as u32;
        hops += 1;
    }
    None
}

/// Turn PCIe link power saving (ASPM L0s and L1) off, as FreeBSD's em
/// driver does for the 82574 (transmit hangs with it on). A card without
/// a PCIe capability (QEMU's PCI-only machines) is left alone.
unsafe fn aspm_off(dev: *mut PCIDevice) -> &'static str {
    let Some(cap) = pcie_cap(dev) else { return "n/a (no PCIe capability)" };
    // Link Capabilities bits 11:10: ASPM support; Link Control bits 1:0.
    if pci!(dev, ReadConfigLong, cap + 0x0C) & 0x0C00 == 0 {
        return "not supported";
    }
    let ctl = pci!(dev, ReadConfigWord, cap + 0x10);
    if ctl & 0x3 == 0 {
        return "already off";
    }
    pci!(dev, WriteConfigWord, cap + 0x10, ctl & !0x3);
    "turned off"
}

#[cfg(feature = "diag")]
/// The text report for E1000E_CMD_DIAG (the unit task: it alone touches
/// the PHY and the NVM). Never reads ICR or the statistics registers
/// directly (they clear on read): ICS mirrors ICR, and the statistics are
/// the driver's sums.
unsafe fn diag_report(u: &'static Unit, net: &mut Net) -> String {
    use core::fmt::Write;
    let r = net.r;
    let dev = net.dev;
    let mut t = String::new();
    let _ = writeln!(t, "{} unit {}", NAME, u.index());
    let vid = pci!(dev, ReadConfigWord, 0);
    let did = pci!(dev, ReadConfigWord, 2);
    let _ = writeln!(
        t,
        "PCI {:04x}:{:04x} rev {:02x}, command {:04x}, status {:04x}, IRQ line {}",
        vid, did, pci!(dev, ReadConfigByte, 8), pci!(dev, ReadConfigWord, 4), pci!(dev, ReadConfigWord, 6),
        pci!(dev, ReadConfigByte, PCI_INTERRUPT_LINE)
    );
    match pcie_cap(dev) {
        Some(c) => {
            let ls = pci!(dev, ReadConfigWord, c + 0x12);
            let lc = pci!(dev, ReadConfigWord, c + 0x10);
            let _ = writeln!(t, "PCIe link: gen {} x{}, ASPM control {}", ls & 0xF, (ls >> 4) & 0x3F, lc & 3);
        }
        None => {
            let _ = writeln!(t, "PCIe: no capability (a PCI bus)");
        }
    }
    let regs: [(&str, u32); 30] = [
        ("CTRL", reg::CTRL), ("STATUS", reg::STATUS), ("EECD", reg::EECD), ("CTRL_EXT", reg::CTRL_EXT),
        ("ICS", reg::ICS), ("IMS", reg::IMS), ("ITR", reg::ITR), ("RCTL", reg::RCTL), ("TCTL", reg::TCTL),
        ("TIPG", reg::TIPG), ("EXTCNF", reg::EXTCNF_CTRL), ("PBA", reg::PBA), ("FCRTL", reg::FCRTL),
        ("FCRTH", reg::FCRTH), ("FCTTV", reg::FCTTV), ("RDBAL", reg::RDBAL), ("RDLEN", reg::RDLEN),
        ("RDH", reg::RDH), ("RDT", reg::RDT), ("RDTR", reg::RDTR), ("TDBAL", reg::TDBAL), ("TDLEN", reg::TDLEN),
        ("TDH", reg::TDH), ("TDT", reg::TDT), ("TXDCTL", reg::TXDCTL0), ("TARC0", reg::TARC0),
        ("GCR", reg::GCR), ("GCR2", reg::GCR2), ("RAL0", reg::RAL0), ("RAH0", reg::RAH0),
    ];
    for (i, (name, off)) in regs.iter().enumerate() {
        let _ = write!(t, "{:>8} {:08x}{}", name, r.r(*off), if i % 4 == 3 { "\n" } else { "  " });
    }
    let _ = writeln!(t);
    let (up, bps) = r.link();
    let _ = writeln!(t, "link {}, {} Mbit/s, {} duplex", if up { "up" } else { "down" }, bps / 1_000_000,
        if r.r(reg::STATUS) & bit::STATUS_FD != 0 { "full" } else { "half" });
    let phyregs = r.with_mdio(|r| {
        let mut v = [None; 7];
        for (i, reg) in [hw::phy::CONTROL, hw::phy::STATUS, hw::phy::ID1, hw::phy::ID2, hw::phy::AUTONEG_ADV,
            hw::phy::LP_ABILITY, hw::phy::GBIT_STATUS].iter().enumerate() {
            v[i] = r.phy_read(*reg);
        }
        (v, r.phy_read(hw::phy::GBIT_CTRL), r.phy_read(hw::phy::SPEC_STATUS))
    });
    match phyregs {
        Some((v, gc, ss)) => {
            let f = |x: Option<u16>| x.map(|x| alloc::format!("{:04x}", x)).unwrap_or_else(|| "----".into());
            let _ = writeln!(t, "PHY: BMCR {} BMSR {} ID {}{} ANAR {} ANLPAR {} 1000CR {} 1000SR {} SPEC {}",
                f(v[0]), f(v[1]), f(v[2]), f(v[3]), f(v[4]), f(v[5]), f(gc), f(v[6]), f(ss));
        }
        None => {
            let _ = writeln!(t, "PHY: MDIO ownership not granted");
        }
    }
    let _ = write!(t, "NVM:");
    for w in 0..0x40u32 {
        if w % 16 == 0 {
            let _ = write!(t, "\n  {:02x}:", w);
        }
        match r.nvm_word(w) {
            Some(v) => { let _ = write!(t, " {:04x}", v); }
            None => { let _ = write!(t, " ----"); }
        }
    }
    let _ = writeln!(t);
    let (tx_tail, tx_clean, pend, mute) =
        u.tx.with(|tx| tx.as_ref().map(|x| (x.tail, x.clean, x.pending.len(), x.mute)).unwrap_or((0, 0, 0, false)));
    let (st, fc, hwc, station) = u.shared.with(|s| (s.stats, s.fc_mode, s.hw, s.station));
    let _ = writeln!(t, "station {}, {}", hex_mac(&station), fc);
    let _ = writeln!(t, "rx: next {} tail {} held {}; tx: tail {} clean {} waiting writes {}{}",
        net.q[0].next, net.q[0].tail, net.any_held(), tx_tail, tx_clean, pend, if mute { " (MUTED)" } else { "" });
    let _ = writeln!(t, "irq: raised {} claimed {} passed on {}; sent {} received {} recoveries {}",
        u.irq_raised.load(Ordering::Relaxed), u.irq_claimed.load(Ordering::Relaxed),
        u.irq_passed_on.load(Ordering::Relaxed), st.tx_packets, st.rx_packets, st.recoveries);
    for (i, (_, name)) in hw::COUNTERS.iter().enumerate() {
        if hwc[i] != 0 {
            let nm = core::str::from_utf8(&name[..name.len() - 1]).unwrap_or("?");
            let _ = writeln!(t, "  {}: {}", nm, hwc[i]);
        }
    }
    t
}

/// A number from ENV: (diag build; decimal or 0x hex). None if unset, not a
/// number, or the caller is not a process (DOS needs one).
#[cfg(feature = "diag")]
unsafe fn env_u32(name: &str) -> Option<u32> {
    let me = sys::exec_find_task(null_mut());
    if me.is_null() || core::ptr::read_unaligned((me as *const u8).add(8)) != sys::NT_PROCESS {
        return None;
    }
    let mut cname = [0u8; 40];
    let nb = name.as_bytes();
    if nb.len() >= cname.len() {
        return None;
    }
    cname[..nb.len()].copy_from_slice(nb);
    let mut buf = [0u8; 16];
    let got = sys::dos_get_var(cname.as_ptr() as _, buf.as_mut_ptr() as _, buf.len() as i32, sys::GVF_GLOBAL_ONLY);
    if got <= 0 {
        return None;
    }
    let t = core::str::from_utf8(&buf[..got as usize]).ok()?.trim();
    match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(h) => u32::from_str_radix(h, 16).ok(),
        None => t.parse().ok(),
    }
}

/// The MTU from ENV:e1000e_diag/MTU (diag build), or 1500. Read in the
/// opener's task, which must be a process for DOS (Roadshow's and s2test's
/// are).
#[cfg(feature = "diag")]
unsafe fn env_mtu(n: usize) -> u32 {
    let me = sys::exec_find_task(null_mut());
    // tc_Node.ln_Type, at offset 8 of the Node.
    let is_process = !me.is_null() && core::ptr::read_unaligned((me as *const u8).add(8)) == sys::NT_PROCESS;
    if !is_process {
        xlog!("unit {}: MTU 1500: the opener is not a process, ENV:e1000e_diag/MTU not read", n);
        return eth::MTU as u32;
    }
    let mut buf = [0u8; 16];
    let got = sys::dos_get_var(amigaos4::amstr!("e1000e_diag/MTU").as_ptr() as _, buf.as_mut_ptr() as _, buf.len() as i32, sys::GVF_GLOBAL_ONLY);
    if got <= 0 {
        xlog!("unit {}: MTU 1500 (no ENV:e1000e_diag/MTU)", n);
        return eth::MTU as u32;
    }
    let text = core::str::from_utf8(&buf[..got as usize]).unwrap_or("").trim();
    match text.parse::<u32>() {
        Ok(v) if (eth::MTU as u32..=JUMBO_MTU_MAX).contains(&v) => {
            xlog!("unit {}: MTU {} from ENV:e1000e_diag/MTU{}", n, v,
                if v > eth::MTU as u32 { " (jumbo frames: 16 KB buffers, LPE)" } else { "" });
            v
        }
        _ => {
            xlog!("unit {}: MTU 1500: ENV:e1000e_diag/MTU \"{}\" is not {}..{}", n, text, eth::MTU, JUMBO_MTU_MAX);
            eth::MTU as u32
        }
    }
}

/// Every 5 s while there is traffic (diag build): transmit batching and
/// jumbo frame figures to the RAM: log.
#[cfg(feature = "diag")]
unsafe fn xsummary(u: &'static Unit, last: &mut [u32; 5]) {
    let now = [
        u.perf[perf::TX_PUBLISH].load(Ordering::Relaxed),
        u.perf[perf::TX_NOTIFY].load(Ordering::Relaxed),
        u.perf[perf::TX_DEFERRED].load(Ordering::Relaxed),
        u.jumbo_rx.load(Ordering::Relaxed),
        u.jumbo_tx.load(Ordering::Relaxed),
    ];
    let d: Vec<u32> = now.iter().zip(last.iter()).map(|(a, b)| a.wrapping_sub(*b)).collect();
    if d.iter().all(|&x| x == 0) {
        return;
    }
    *last = now;
    let (mtu, rx, tx) = u.shared.with(|s| (s.mtu, s.stats.rx_packets, s.stats.tx_packets));
    let per_write = if d[1] > 0 { d[0] as f32 / d[1] as f32 } else { 0.0 };
    xlog!(
        "unit {}: 5 s: sent {} frames with {} tail writes ({:.2} per write), {} writes put off; jumbo (MTU {}): {} in, {} out; totals in {} out {}; switches 0x{:x}",
        u.index(), d[0], d[1], per_write, d[2], mtu, d[3], d[4], rx, tx, u.tune.load(Ordering::Relaxed)
    );
}

unsafe fn irq_body(u: &Unit) -> u32 {
    let r = match u.regs() {
        Some(r) if !u.irq_task.load(Ordering::Acquire).is_null() => r,
        _ => {
            u.irq_passed_on.fetch_add(1, Ordering::Relaxed);
            return 0;
        }
    };
    if u.irq_storm.load(Ordering::Relaxed) != 0 || u.quiesced.load(Ordering::Relaxed) != 0 {
        u.irq_passed_on.fetch_add(1, Ordering::Relaxed);
        return 0;
    }
    u.irq_raised.fetch_add(1, Ordering::Relaxed);
    let t0 = arch::timebase();
    // Reading ICR clears it and lowers the line. The line may be shared:
    // nothing set (or a card that is gone, all ones) is not ours.
    let icr = r.read_icr();
    u.add_time(perf::ISR_T, t0);
    if icr == 0 || icr == 0xFFFF_FFFF {
        u.irq_passed_on.fetch_add(1, Ordering::Relaxed);
        return 0;
    }
    let claimed = u.irq_claimed.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    if claimed.wrapping_sub(u.irq_drained.load(Ordering::Relaxed)) > STORM_LIMIT {
        r.quiesce();
        u.irq_storm.store(1, Ordering::Release);
    }
    if icr & bit::INT_LSC != 0 {
        u.irq_config.store(1, Ordering::Release);
    }
    if icr & bit::INT_TXQE != 0 {
        u.tx_idle.store(1, Ordering::Release);
    }
    if icr & bit::INT_RXO != 0 {
        u.irq_overrun.fetch_add(1, Ordering::Relaxed);
    }
    let task = u.irq_task.load(Ordering::Acquire);
    if !task.is_null() {
        sys::exec_signal(task, u.irq_mask.load(Ordering::Relaxed));
    }
    1
}

unsafe extern "C" fn net_irq(_ctx: *mut c_void, _sysbase: *mut c_void, data: *mut c_void) -> u32 {
    let r = irq_body(&*(data as *const Unit));
    arch::clear_reservation();
    r
}

static QUIESCED_ALL: AtomicU32 = AtomicU32::new(0);
struct ResetCell(UnsafeCell<Option<ResetCallback>>);
// SAFETY: changed only under OPEN_LOCK.
unsafe impl Sync for ResetCell {}
static RESET_CB: ResetCell = ResetCell(UnsafeCell::new(None));
static RUNNING_UNITS: AtomicU32 = AtomicU32::new(0);

/// On ColdReboot, stop every running card so none goes on writing into
/// memory the next boot hands out again.
unsafe extern "C" fn net_reset(_ctx: *mut c_void, _execbase: *mut c_void, _data: *mut c_void) -> u32 {
    if QUIESCED_ALL.swap(1, Ordering::AcqRel) != 0 {
        return 0;
    }
    for u in units() {
        u.quiesced.store(1, Ordering::Release);
        if let Some(r) = u.regs() {
            r.quiesce();
        }
    }
    0
}

// ---------------------------------------------------------------------------
// The unit task
// ---------------------------------------------------------------------------

extern "C" fn unit_task(n: u32) {
    // SAFETY: from here on this task is its unit's device's only user.
    unsafe {
        if n as usize >= MAX_UNITS {
            return;
        }
        let u = unit(n as usize);
        let net = match (*u.net.get()).as_mut() {
            Some(x) => x,
            None => return,
        };
        let irq_sig = sys::exec_alloc_signal(-1);
        let work_sig = sys::exec_alloc_signal(-1);
        // The heartbeat has its own request: `timer` serves get_sys_time.
        // So has the hold poll (a frame waiting in the ring for a read).
        let timers = AmigaTimer::open(TimerUnit::MicroHz)
            .and_then(|t| Ok((t, AmigaTimer::open(TimerUnit::MicroHz)?, AmigaTimer::open(TimerUnit::MicroHz)?)));
        let (mut timer, mut beat, mut holdt) = match timers {
            Ok(t) => t,
            Err(_) => {
                serial_println!("{} unit {}: no timer.device; the unit does not start", NAME, n);
                return;
            }
        };
        if irq_sig < 0 || work_sig < 0 {
            serial_println!("{} unit {}: no free signal; the unit does not start", NAME, n);
            return;
        }
        let me = sys::exec_find_task(null_mut());
        let irq_mask = 1u32 << irq_sig;
        let work_mask = 1u32 << work_sig;
        let chain = pci!(net.dev, MapInterrupt);
        u.irq_mask.store(irq_mask, Ordering::Relaxed);
        u.irq_task.store(me, Ordering::Release);
        let server = if chain != 0 {
            match IntServer::add(chain, 0, TASK_NAMES[n as usize], net_irq, u as *const Unit as *mut c_void) {
                Ok(s) => Some(s),
                Err(e) => {
                    serial_println!("{} unit {}: AddIntServer({}) failed: {}", NAME, n, chain, e);
                    None
                }
            }
        } else {
            None
        };
        // Without a server, the timer drives the unit (polling every 10 ms).
        let polled = server.is_none();
        net.apply_rx_mode();
        let _ = net.r.read_icr();
        if !polled {
            net.r.enable_irqs();
        }
        serial_println!(
            "{} unit {}: unit task ready: {} chain {}, pri {}",
            NAME, n, if polled { "POLLED, no interrupt server on" } else { "interrupt server on" }, chain, TASK_PRI
        );
        u.worker.publish(me, work_mask);
        let mut recoveries_in_a_row = 0u32;
        // After MAX_RECOVERIES in a row: out of service, the next try at
        // retry_at (timebase), rest_s seconds after the last.
        let mut resting = false;
        u.out_of_service.store(0, Ordering::Release);
        let mut retry_at = 0u64;
        let mut rest_s = 1u64;
        // The card's health is checked (register reads, which are slow on
        // the bus) at most this often, unless the storm breaker fired.
        let tb_hz = timebase_hz();
        TB_PER_US.store((tb_hz / 1_000_000).max(1) as u32, Ordering::Relaxed);
        let check_every = tb_hz / 10; // 100 ms
        let mut next_check = 0u64;
        // Health checks counted: the card counters are read every tenth (1 s).
        let mut checks = 0u32;
        // Adaptive priority: the byte counts at the last check, and the
        // priority now.
        let (mut last_out, mut last_in) = (0u64, 0u64);
        let mut cur_pri = TASK_PRI;
        // Extras summary (diag build): the counters at the last report.
        #[cfg(feature = "diag")]
        let mut xsum = [0u32; 5];

        let rxm = u.rxl.mutex.load(Ordering::Acquire) as sys::APTR;
        let mut rx_locked = false;
        loop {
            if rx_locked {
                sys::exec_mutex_release(rxm);
                rx_locked = false;
            }
            if polled {
                timer.start(TimerVal::from_millis(10));
                sys::exec_wait(work_mask | timer.signal_mask());
                if timer.is_done() {
                    timer.finish();
                } else {
                    timer.abort();
                }
            } else {
                // Writes are sent in the stack's task and do not wake this
                // one, and a device that was reset raises no interrupt: the
                // heartbeat makes sure its status is still read.
                if beat.is_done() {
                    beat.finish();
                    beat.start(TimerVal::from_millis(HEARTBEAT_MS));
                }
                // A held frame is looked at again soon even if no read
                // comes, so it is dropped after HOLD_MS at the latest.
                if net.any_held() && holdt.is_done() {
                    holdt.finish();
                    holdt.start(TimerVal::from_millis(HOLD_POLL_MS));
                }
                let all = work_mask | irq_mask | beat.signal_mask() | holdt.signal_mask();
                sys::exec_wait(all);
            }
            if !rxm.is_null() {
                sys::exec_mutex_obtain(rxm);
                rx_locked = true;
            }
            if u.worker.stop_requested() {
                break;
            }
            u.irq_drained.store(u.irq_claimed.load(Ordering::Relaxed), Ordering::Relaxed);
            let tb = arch::timebase();
            let due = tb >= next_check || u.irq_storm.load(Ordering::Acquire) != 0;
            if due {
                next_check = tb + check_every;
            }
            let broken = if due {
                net.device_broken().or_else(|| {
                    let limit = tb_hz * TX_STALL_MS / 1000;
                    let st = u.tx.with(|tx| tx.as_mut().map(|t| t.stalled(tb, limit)).unwrap_or(false));
                    if st { Some("transmit stalled: descriptors waiting, the head does not move") } else { None }
                })
            } else {
                None
            };
            if due && broken.is_none() {
                checks = checks.wrapping_add(1);
                #[cfg(feature = "diag")]
                if checks % 50 == 0 {
                    xsummary(u, &mut xsum);
                }
                if u.tune.load(Ordering::Relaxed) & TUNE_ADAPTIVE_PRI != 0 {
                    let (tb_out, tb_in) = u.shared.with(|s| (s.stats.tx_bytes, s.stats.rx_bytes));
                    let (dout, din) = (tb_out.wrapping_sub(last_out), tb_in.wrapping_sub(last_in));
                    last_out = tb_out;
                    last_in = tb_in;
                    let want = if dout >= SEND_MIN_BYTES && dout >= din * SEND_RATIO { TASK_PRI_SENDING } else { TASK_PRI };
                    if want != cur_pri {
                        sys::exec_set_task_pri(me as _, want as i8);
                        cur_pri = want;
                        u.count(perf::PRI_CHANGES);
                    }
                }
            }
            if due && broken.is_none() && checks % 10 == 0 {
                let c = net.r.read_counters();
                u.shared.with(|s| s.hw.iter_mut().zip(c.iter()).for_each(|(a, b)| *a += *b as u64));
                #[cfg(feature = "debug-log")]
                {
                    // Once a second: rings and interrupts.
                    {
                        let (tt, tc) = u.tx.with(|tx| tx.as_ref().map(|x| (x.tail, x.clean)).unwrap_or((0, 0)));
                        serial_println!(
                            "{} unit {}: [dbg] STATUS {:08x} ICS {:08x} RDH {} RDT {} next {} | TDH {} TDT {} tail {} clean {} | irq {}/{}/{} peek {}",
                            NAME, n, net.r.r(reg::STATUS), net.r.r(reg::ICS), net.r.r(reg::RDH), net.r.r(reg::RDT), net.q[0].next,
                            net.r.r(reg::TDH), net.r.r(reg::TDT), tt, tc,
                            u.irq_raised.load(Ordering::Relaxed), u.irq_claimed.load(Ordering::Relaxed),
                            u.irq_passed_on.load(Ordering::Relaxed), u.perf[perf::PEEK_WAKES].load(Ordering::Relaxed)
                        );
                    }
                }
            }
            if due && broken.is_none() && (recoveries_in_a_row != 0 || resting) {
                // Healthy at a status check after a recovery: it held.
                if resting || recoveries_in_a_row >= MAX_RECOVERIES {
                    serial_println!("{} unit {}: the device works again", NAME, n);
                }
                recoveries_in_a_row = 0;
                rest_s = 1;
                resting = false;
                u.out_of_service.store(0, Ordering::Release);
            }
            if let Some(why) = broken {
                if resting && tb < retry_at {
                    // Out of service until the next try: fail what needs the device.
                    while let Some(r) = u.cmdq.take_next() {
                        u.cmdq.done();
                        S2Req::from_raw(r.as_ptr()).unwrap().reply(S2ERR_OUTOFSERVICE, S2WERR_RCVREL_HDW_ERR);
                    }
                    continue;
                }
                recoveries_in_a_row += 1;
                if recoveries_in_a_row > MAX_RECOVERIES && !resting {
                    // A device that breaks again straight after every
                    // recovery: rest, and try again later, more slowly each
                    // time (1 s, 2 s, ... 32 s), rather than for good: a
                    // stack keeps the unit open, so a unit that gave up for
                    // good would stay dead until a reboot.
                    resting = true;
                    u.out_of_service.store(1, Ordering::Release);
                    retry_at = tb + rest_s * tb_hz;
                    serial_println!("{} unit {}: {}; {} recoveries in a row, out of service, next try in {} s", NAME, n, why, MAX_RECOVERIES, rest_s);
                    while let Some(r) = u.cmdq.take_next() {
                        u.cmdq.done();
                        S2Req::from_raw(r.as_ptr()).unwrap().reply(S2ERR_OUTOFSERVICE, S2WERR_RCVREL_HDW_ERR);
                    }
                    continue;
                }
                serial_println!("{} unit {}: recovering ({} in a row): {}", NAME, n, recoveries_in_a_row, why);
                let t_rec = arch::timebase();
                match net.recover() {
                    Ok(()) => {
                        u.shared.with(|s| s.stats.recoveries += 1);
                        serial_println!("{} unit {}: recovered in {} ms", NAME, n, arch::timebase().wrapping_sub(t_rec) * 1000 / tb_hz);
                        signal_events(u, S2EVENT_ERROR | S2EVENT_HARDWARE);
                        if resting {
                            // Back on probation: one more failure rests again, longer.
                            resting = false;
                            u.out_of_service.store(0, Ordering::Release);
                            recoveries_in_a_row = MAX_RECOVERIES;
                            rest_s = (rest_s * 2).min(32);
                        }
                    }
                    Err(e) => {
                        serial_println!("{} unit {}: recovery failed: {}", NAME, n, e);
                        if resting || recoveries_in_a_row >= MAX_RECOVERIES {
                            resting = true;
                            u.out_of_service.store(1, Ordering::Release);
                            retry_at = tb + rest_s * tb_hz;
                            rest_s = (rest_s * 2).min(32);
                        }
                        continue;
                    }
                }
                // Carry on: the request that woke the task is still queued.
            }
            let now = timer.get_sys_time();
            u.now_s.store(now.secs, Ordering::Relaxed);
            u.now_us.store(now.micros, Ordering::Relaxed);
            let now = TimeVal { Seconds: now.secs, Microseconds: now.micros };
            if u.irq_config.swap(0, Ordering::AcqRel) != 0 {
                net.config_changed();
            }
            let ov = u.irq_overrun.swap(0, Ordering::AcqRel);
            if ov != 0 {
                u.shared.with(|s| s.stats.overruns += ov as u64);
            }
            u.count(perf::WAKES);
            if net.rx(now) > 0 {
                recoveries_in_a_row = 0;
                u.count(perf::WAKES_RX);
            }
            tx_service(u, now);
            while let Some(ior) = u.cmdq.take_next() {
                u.cmdq.done();
                if let Some(r) = S2Req::from_raw(ior.as_ptr()) {
                    serve(u, net, r, &mut timer);
                }
            }
            if u.rx_mode_dirty.swap(0, Ordering::AcqRel) != 0 {
                net.apply_rx_mode();
            }
            tx_service(u, now);
        }

        // STOP (the unit's last close). Reset the card BEFORE the server
        // goes: a level-triggered line left asserted with no server to
        // claim it would storm the chain.
        u.worker.withdraw();
        let st = u.shared.with(|s| (s.stats.tx_packets, s.stats.rx_packets, s.stats.tx_waits, s.stats.recoveries));
        serial_println!(
            "{} unit {}: totals: sent {}, received {}, writes that waited for a slot {}, recoveries {}",
            NAME, n, st.0, st.1, st.2, st.3
        );
        abort_traffic(u, net, IOERR_ABORTED, 0);
        while let Some(r) = u.cmdq.take_next() {
            u.cmdq.done();
            r.reply(IOERR_ABORTED, 0);
        }
        // Masked, receive and transmit off, then a full reset: no DMA
        // goes on into the buffers about to be freed. The firmware gets
        // the card back.
        net.r.quiesce();
        let mut sleep = Sleeper::new();
        let _ = net.r.reset(&mut |ms| sleep.ms(ms));
        net.r.release();
        let _ = net.r.read_icr();
        u.quiesced.store(1, Ordering::Release);
        drop(server);
        drop(beat); // aborts a pending heartbeat
        drop(holdt);
        drop(timer);
        u.irq_task.store(null_mut(), Ordering::Release);
        sys::exec_free_signal(irq_sig);
        sys::exec_free_signal(work_sig);
        if rx_locked {
            sys::exec_mutex_release(rxm);
        }
        u.running.store(0, Ordering::Release);
        let t = u.stop_ack_task.load(Ordering::Acquire);
        let m = u.stop_ack_mask.load(Ordering::Acquire);
        if !t.is_null() && m != 0 {
            sys::exec_signal(t, m);
        }
    }
}

// ---------------------------------------------------------------------------
// Unit lifetime: start at first open, stop at last close
// ---------------------------------------------------------------------------

const STOP_BOUND_MS: u32 = 3000;
const GONE_POLL_MS: u32 = 5;

unsafe fn start_unit(u: &'static Unit) -> Result<(), String> {
    let n = u.index();
    let dev = u.dev.load(Ordering::Acquire);
    if dev.is_null() {
        return Err("no card".into());
    }
    if !u.cmdq_init() {
        return Err("mutex allocation failed".into());
    }
    let net = match Net::setup(u, dev) {
        Ok(x) => x,
        Err(e) => {
            drop(u.tx.with(|t| t.take()));
            u.cmdq_destroy();
            return Err(e);
        }
    };
    let _ = net.r.read_icr();
    *u.regs.get() = Some(net.r);
    u.regs_ready.store(1, Ordering::Release);
    u.quiesced.store(0, Ordering::Release);
    *u.net.get() = Some(net);
    let tags = [
        TagItem { ti_Tag: sys::AT_PARAM1, ti_Data: n as u32 },
        TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 },
    ];
    let task = sys::exec_create_task(TASK_NAMES[n].as_ptr() as _, TASK_PRI, unit_task as *const c_void, TASK_STACK, tags.as_ptr());
    if task.is_null() {
        u.regs_ready.store(0, Ordering::Release);
        if let Some(mut net) = core::ptr::replace(u.net.get(), None) {
            net.r.quiesce();
            net.end_all_mappings();
        }
        drop(u.tx.with(|t| t.take()));
        u.cmdq_destroy();
        return Err("CreateTask failed".into());
    }
    u.caps.store(Box::into_raw(Caps::build(u.tx.with(|t| t.as_ref().map_or(false, |t| t.tso.is_some())))), Ordering::Release);
    u.running.store(1, Ordering::Release);
    if RUNNING_UNITS.fetch_add(1, Ordering::AcqRel) == 0 {
        QUIESCED_ALL.store(0, Ordering::Release);
        match ResetCallback::add(127, DEVNAME_C, net_reset, null_mut()) {
            Ok(cb) => *RESET_CB.0.get() = Some(cb),
            Err(e) => serial_println!("{}: AddResetCallback failed: {}", NAME, e),
        }
    }
    Ok(())
}

impl Unit {
    unsafe fn cmdq_init(&self) -> bool {
        if self.cmdq.init().is_err() {
            return false;
        }
        if !self.shared.init() {
            self.cmdq.destroy();
            return false;
        }
        if !self.tx.init() {
            self.cmdq.destroy();
            self.shared.destroy();
            return false;
        }
        if !self.rxl.init() {
            self.cmdq.destroy();
            self.shared.destroy();
            self.tx.destroy();
            return false;
        }
        true
    }
    unsafe fn cmdq_destroy(&self) {
        self.cmdq.destroy();
        self.shared.destroy();
        self.tx.destroy();
        self.rxl.destroy();
    }
}

unsafe fn wait_ready(u: &Unit) -> bool {
    let Ok(mut timer) = AmigaTimer::open(TimerUnit::MicroHz) else { return false };
    let mut waited = 0;
    while !u.worker.is_ready() {
        if waited >= STOP_BOUND_MS || u.running.load(Ordering::Acquire) == 0 {
            return false;
        }
        let _ = timer.delay(TimerVal::from_millis(GONE_POLL_MS));
        waited += GONE_POLL_MS;
    }
    true
}

/// Stop unit `u`: ask its task to stop, wait for its answer and until exec
/// has removed it, then free the device's memory. The card stays locked.
unsafe fn stop_unit(u: &'static Unit) -> Result<(), String> {
    let n = u.index();
    if u.running.load(Ordering::Acquire) == 0 {
        return Ok(());
    }
    let mut timer = AmigaTimer::open(TimerUnit::MicroHz).map_err(|e| alloc::format!("timer.device: {}", e))?;
    let sig = sys::exec_alloc_signal(-1);
    if sig < 0 {
        return Err("no free signal".into());
    }
    let mask = 1u32 << sig;
    u.stop_ack_task.store(sys::exec_find_task(null_mut()), Ordering::Release);
    u.stop_ack_mask.store(mask, Ordering::Release);
    u.worker.request_stop();
    timer.start(TimerVal::from_millis(STOP_BOUND_MS));
    let answered = loop {
        let got = sys::exec_wait(mask | timer.signal_mask());
        if got & mask != 0 {
            break true;
        }
        if timer.is_done() {
            timer.finish();
            break false;
        }
    };
    timer.abort();
    sys::exec_free_signal(sig);
    u.stop_ack_mask.store(0, Ordering::Release);
    u.stop_ack_task.store(null_mut(), Ordering::Release);
    if !answered {
        u.stuck.store(1, Ordering::Release);
        return Err(alloc::format!("the unit task did not stop within {} ms", STOP_BOUND_MS));
    }
    let mut waited = 0;
    while !sys::exec_find_task(TASK_NAMES[n].as_ptr() as _).is_null() {
        if waited >= STOP_BOUND_MS {
            u.stuck.store(1, Ordering::Release);
            return Err("the unit task answered but did not go".into());
        }
        let _ = timer.delay(TimerVal::from_millis(GONE_POLL_MS));
        waited += GONE_POLL_MS;
    }
    u.regs_ready.store(0, Ordering::Release);
    drop(u.tx.with(|t| t.take())); // the card is reset: EndDMA and FreeVec
    drop(core::ptr::replace(u.net.get(), None));
    // Openers are gone (last close); drop what is left of the shared state.
    let leftover = u.shared.with(|s| core::mem::replace(s, Shared::new()));
    drop(leftover);
    u.cmdq_destroy();
    // The capability items: no opener is left to read them.
    let caps = u.caps.swap(null_mut(), Ordering::AcqRel);
    if !caps.is_null() {
        drop(Box::from_raw(caps));
    }
    let dev = u.dev.load(Ordering::Acquire);
    core::ptr::write(u as *const Unit as *mut Unit, Unit::NEW);
    u.dev.store(dev, Ordering::Release);
    if RUNNING_UNITS.fetch_sub(1, Ordering::AcqRel) == 1 {
        drop((*RESET_CB.0.get()).take());
    }
    serial_println!("{} unit {}: stopped: task gone, card reset, memory freed", NAME, n);
    Ok(())
}

unsafe fn with_open_lock<R>(f: impl FnOnce() -> R) -> R {
    let m = OPEN_LOCK.load(Ordering::Acquire);
    if !m.is_null() {
        sys::exec_mutex_obtain(m as sys::APTR);
    }
    let r = f();
    if !m.is_null() {
        sys::exec_mutex_release(m as sys::APTR);
    }
    r
}

// ---------------------------------------------------------------------------
// Device entry points
// ---------------------------------------------------------------------------

/// io_Message.mn_Length: the request's size.
fn req_len(ior: *mut IORequest) -> u16 {
    // SAFETY: every IORequest starts with a Message (mn_Length at 18).
    unsafe { core::ptr::read_unaligned((ior as *const u8).add(18) as *const u16) }
}
const S2REQ_SIZE: u16 = 88;

#[no_mangle]
pub extern "C" fn rust_net_init() -> i32 {
    // SAFETY: the init runs once per load, before any open.
    unsafe {
        let cards = find_cards();
        if cards.is_empty() {
            serial_println!("{}: no Intel 82574 card (8086:10d3 or 8086:10f6); device not added", NAME);
            return 1;
        }
        let m = sys::exec_alloc_sys_object(sys::ASOT_MUTEX, [TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 }].as_ptr());
        if m.is_null() {
            for (dev, claim) in cards {
                release_claim(claim);
                pci!(dev, Unlock);
                sys::wrappers::pci::pci_free_device(dev);
            }
            return 1;
        }
        OPEN_LOCK.store(m as *mut c_void, Ordering::Release);
        let count = cards.len();
        for (i, (dev, claim)) in cards.into_iter().enumerate() {
            unit(i).dev.store(dev, Ordering::Release);
            CLAIMS[i].store(claim, Ordering::Release);
        }
        NUNITS.store(count as u32, Ordering::Release);
        #[cfg(feature = "diag")]
        ramlog::start();
        serial_println!("{}: {} card(s), units 0 to {}; each is set up at its first open", NAME, count, count - 1);
    }
    0
}

#[no_mangle]
pub extern "C" fn rust_net_expunge() -> i32 {
    // SAFETY: exec calls Expunge with no other user of the device.
    unsafe {
        for u in units() {
            if u.opens.load(Ordering::Acquire) != 0 || u.running.load(Ordering::Acquire) != 0 || u.stuck.load(Ordering::Acquire) != 0 {
                return 0;
            }
        }
        for u in units() {
            let dev = u.dev.swap(null_mut(), Ordering::AcqRel);
            if !dev.is_null() {
                pci!(dev, Unlock);
                sys::wrappers::pci::pci_free_device(dev);
            }
        }
        for c in &CLAIMS {
            release_claim(c.swap(null_mut(), Ordering::AcqRel));
        }
        NUNITS.store(0, Ordering::Release);
        let m = OPEN_LOCK.swap(null_mut(), Ordering::AcqRel);
        if !m.is_null() {
            sys::exec_free_sys_object(sys::ASOT_MUTEX, m as sys::APTR);
        }
        #[cfg(feature = "diag")]
        ramlog::stop();
        serial_println!("{}: expunged", NAME);
    }
    1
}

#[no_mangle]
pub extern "C" fn rust_dev_open(ior: *mut IORequest, unit_number: u32, flags: u32) -> i8 {
    if ior.is_null() || unit_number >= NUNITS.load(Ordering::Acquire) {
        return IOERR_OPENFAIL;
    }
    let u = unit(unit_number as usize);
    // SAFETY: the caller's request; start/stop under OPEN_LOCK.
    unsafe {
        with_open_lock(|| {
            let full = req_len(ior) >= S2REQ_SIZE;
            if u.opens.load(Ordering::Acquire) == 0 && u.running.load(Ordering::Acquire) == 0 {
                if u.stuck.load(Ordering::Acquire) != 0 {
                    return IOERR_OPENFAIL;
                }
                if let Err(e) = start_unit(u) {
                    serial_println!("{} unit {}: open failed: {}", NAME, unit_number, e);
                    return IOERR_OPENFAIL;
                }
                if !wait_ready(u) {
                    serial_println!("{} unit {}: open failed: the unit task did not get ready", NAME, unit_number);
                    let _ = stop_unit(u);
                    return IOERR_OPENFAIL;
                }
            }
            // A SANA-II opener: its buffer management from the tags in
            // ios2_BufferManagement, replaced by its cookie.
            if full {
                let r = S2Req::from_raw(ior).unwrap();
                let bm = BufferManagement::from_tags(r.buffer_management() as *const TagItem);
                let refused = u.shared.with(|s| {
                    if s.exclusive || (flags & SANA2OPF_MINE != 0 && !s.openers.is_empty()) {
                        return true;
                    }
                    false
                });
                if refused {
                    if u.opens.load(Ordering::Acquire) == 0 {
                        let _ = stop_unit(u);
                    }
                    return IOERR_UNITBUSY;
                }
                let op = Box::into_raw(Box::new(Opener { bm, flags }));
                u.shared.with(|s| {
                    s.openers.push(op);
                    if flags & SANA2OPF_MINE != 0 {
                        s.exclusive = true;
                    }
                    if flags & SANA2OPF_PROM != 0 {
                        s.promisc_openers += 1;
                    }
                });
                if flags & SANA2OPF_PROM != 0 {
                    u.rx_mode_dirty.store(1, Ordering::Release);
                    u.worker.wake();
                }
                r.set_buffer_management(op as *mut c_void);
            }
            if let Some(r) = IoReq::from_raw(ior) {
                r.set_unit(u as *const Unit as *mut c_void);
            }
            u.opens.fetch_add(1, Ordering::AcqRel);
            0
        })
    }
}

fn unit_of(ior: *mut IORequest) -> Option<&'static Unit> {
    // SAFETY: an opened request's io_Unit.
    let p = unsafe { IoReq::from_raw(ior) }?.unit() as *const Unit;
    units().find(|u| core::ptr::eq(*u, p))
}

#[no_mangle]
pub extern "C" fn rust_dev_close(ior: *mut IORequest) {
    let Some(u) = unit_of(ior) else { return };
    // SAFETY: the caller's request, opened on this device.
    unsafe {
        with_open_lock(|| {
            if req_len(ior) >= S2REQ_SIZE {
                let r = S2Req::from_raw(ior).unwrap();
                let op = r.buffer_management() as *mut Opener;
                let (found, left_reads, leftover) = u.shared.with(|s| {
                    let Some(pos) = s.openers.iter().position(|&o| o == op) else { return (false, Vec::new(), Vec::new()) };
                    s.openers.remove(pos);
                    s.read_types.retain(|&(_, o)| o != op);
                    s.tracked.retain(|&(_, _, o)| o != op);
                    if (*op).flags & SANA2OPF_MINE != 0 {
                        s.exclusive = false;
                    }
                    if (*op).flags & SANA2OPF_PROM != 0 {
                        s.promisc_openers = s.promisc_openers.saturating_sub(1);
                    }
                    // Requests the stack left behind (it should not).
                    let mine = |r: &S2Req| r.buffer_management() as *mut Opener == op;
                    let mut left = Vec::new();
                    let mut left_reads = Vec::new();
                    let mut i = 0;
                    while i < s.reads.len() {
                        if mine(&s.reads[i].r) {
                            left_reads.extend(s.reads.remove(i));
                        } else {
                            i += 1;
                        }
                    }
                    for list in [&mut s.orphans, &mut s.events, &mut s.samples] {
                        let mut i = 0;
                        while i < list.len() {
                            if mine(&list[i]) {
                                left.push(list.remove(i));
                            } else {
                                i += 1;
                            }
                        }
                    }
                    (true, left_reads, left)
                });
                for rd in left_reads {
                    rd.end(IOERR_ABORTED, 0);
                }
                for r in leftover {
                    r.reply(IOERR_ABORTED, 0);
                }
                // Writes still waiting for a transmit slot would be copied
                // through this opener's buffer management after it is freed.
                if found {
                    let mine: Vec<Tx> = u.tx.with(|tx| {
                        let Some(t) = tx.as_mut() else { return Vec::new() };
                        let mut out = Vec::new();
                        let mut i = 0;
                        while i < t.pending.len() {
                            if t.pending[i].req().buffer_management() as *mut Opener == op {
                                out.push(t.pending.remove(i).unwrap());
                            } else {
                                i += 1;
                            }
                        }
                        out
                    });
                    for t in mine {
                        t.end(IOERR_ABORTED, 0);
                    }
                }
                if found {
                    drop(Box::from_raw(op));
                    u.rx_mode_dirty.store(1, Ordering::Release);
                    u.worker.wake();
                }
            }
            if u.opens.fetch_sub(1, Ordering::AcqRel) == 1 {
                if let Err(e) = stop_unit(u) {
                    serial_println!("{} unit {}: stop FAILED: {}", NAME, u.index(), e);
                }
            }
        });
    }
}

/// NSCMD_DEVICEQUERY's command list.
static SUPPORTED_COMMANDS: [u16; 30] = [
    CMD_READ, CMD_WRITE, CMD_FLUSH,
    S2_DEVICEQUERY, S2_GETSTATIONADDRESS, S2_CONFIGINTERFACE,
    S2_ADDMULTICASTADDRESS, S2_DELMULTICASTADDRESS, S2_MULTICAST, S2_BROADCAST,
    S2_TRACKTYPE, S2_UNTRACKTYPE, S2_GETTYPESTATS, S2_GETSPECIALSTATS, S2_GETGLOBALSTATS,
    S2_ONEVENT, S2_READORPHAN, S2_ONLINE, S2_OFFLINE,
    S2_ADDMULTICASTADDRESSES, S2_DELMULTICASTADDRESSES, S2_GETEXTENDEDGLOBALSTATS,
    S2_SAMPLE_THROUGHPUT, S2_SANA2HOOK, NSCMD_DEVICEQUERY,
    // The SANA-II extensions (sana2ext).
    x::S2X_QUERY, x::S2X_READBATCH, x::S2X_WRITEBATCH, x::S2X_CONFIGURE, 0,
];

/// S2_GETSPECIALSTATS' record names.
/// The order is the record types (index + 1): add at the end only.
static SPECIAL_NAMES: [&[u8]; 25] = [
    amigaos4::amstr!("Frames dropped: no read pending"),
    amigaos4::amstr!("Frames for other stations dropped"),
    amigaos4::amstr!("Multicast frames for groups not added"),
    amigaos4::amstr!("Writes that waited for a transmit slot"),
    amigaos4::amstr!("Device recoveries"),
    amigaos4::amstr!("Bad frames"),
    amigaos4::amstr!("Interrupts claimed"),
    amigaos4::amstr!("Unit task wakes"),
    amigaos4::amstr!("Unit task wakes that received frames"),
    amigaos4::amstr!("Transmit ring publishes"),
    amigaos4::amstr!("Transmit notifications"),
    amigaos4::amstr!("Receive ring notifications"),
    amigaos4::amstr!("Pure TCP ACKs sent"),
    amigaos4::amstr!("Pure TCP ACKs received"),
    amigaos4::amstr!("Transmit notification time (0.1 us)"),
    amigaos4::amstr!("Interrupt ISR read time (0.1 us)"),
    amigaos4::amstr!("Write path time in the caller (0.1 us)"),
    amigaos4::amstr!("Unit task wakes from the write path's receive peek"),
    amigaos4::amstr!("Receive pass time (0.1 us)"),
    amigaos4::amstr!("Receive hand-over to the stack time (0.1 us)"),
    amigaos4::amstr!("Transmit head register reads"),
    amigaos4::amstr!("Receive passes run by a writer"),
    amigaos4::amstr!("Unit task priority changes"),
    amigaos4::amstr!("Transmit tail writes put off (batching)"),
    amigaos4::amstr!("Frames sent with checksum offload"),
];

const ALL_EVENTS: u32 = (1 << 11) - 1;

#[no_mangle]
pub extern "C" fn rust_dev_begin_io(ior: *mut IORequest) {
    // SAFETY: an opened request.
    let Some(io) = (unsafe { IoReq::from_raw(ior) }) else { return };
    let Some(u) = unit_of(ior) else {
        unsafe { io.finish(IOERR_OPENFAIL, 0) };
        return;
    };
    let cmd = io.command();
    unsafe {
        if cmd == NSCMD_DEVICEQUERY {
            let (e, n) = answer_device_query(io, NSDEVTYPE_SANA2, 0, &SUPPORTED_COMMANDS);
            io.finish(e, n);
            return;
        }
        if req_len(ior) < S2REQ_SIZE {
            io.finish(IOERR_BADLENGTH, 0);
            return;
        }
        let r = S2Req::from_raw(ior).unwrap();
        let bm = r.buffer_management();
        if !u.shared.with(|s| s.opener_ok(bm)) {
            r.finish(S2ERR_BAD_ARGUMENT, S2WERR_NULL_POINTER);
            return;
        }
        let op = bm as *mut Opener;
        match cmd {
            CMD_READ | S2_READORPHAN | x::S2X_READBATCH => {
                if !(*op).bm.complete() {
                    r.finish(S2ERR_BAD_STATE, S2WERR_FUNCTIONS_MISSING);
                    return;
                }
                let batch = if cmd == x::S2X_READBATCH {
                    match check_batch(r, u.rx_queues.load(Ordering::Acquire)) {
                        Ok((first, max, queue)) => Some(RxBatch { next: first, filled: 0, max, queue }),
                        Err((e, w)) => {
                            r.finish(e, w);
                            return;
                        }
                    }
                } else {
                    None
                };
                io.mark_pending();
                let wake = u.shared.with(|s| {
                    if cmd == S2_READORPHAN {
                        s.orphans.push(r);
                    } else {
                        // A read batch counts as a read of its type: a frame
                        // of it that finds none posted waits in the ring.
                        let t = r.packet_type();
                        if !s.read_types.iter().any(|&(ty, o)| ty == t && o == op) {
                            s.read_types.push((t, op));
                        }
                        let seq = s.seq;
                        s.seq = seq.wrapping_add(1);
                        s.reads.push_back(Rd { r, seq, ptype: t, op, batch });
                    }
                    s.rx_held
                });
                if wake {
                    u.worker.wake();
                }
            }
            CMD_WRITE | S2_BROADCAST | S2_MULTICAST => {
                if !(*op).bm.complete() {
                    r.finish(S2ERR_BAD_STATE, S2WERR_FUNCTIONS_MISSING);
                    return;
                }
                tx_from_caller(u, r);
            }
            x::S2X_WRITEBATCH => {
                if !(*op).bm.complete() {
                    r.finish(S2ERR_BAD_STATE, S2WERR_FUNCTIONS_MISSING);
                    return;
                }
                match check_batch(r, TX_QUEUES) {
                    Ok((first, count, _queue)) => tx_batch_from_caller(u, r, first, count),
                    Err((e, w)) => r.finish(e, w),
                }
            }
            x::S2X_QUERY => {
                let q = r.stat_data() as *mut x::S2XQuery;
                let caps = u.caps.load(Ordering::Acquire);
                if q.is_null() {
                    r.finish(S2ERR_BAD_ARGUMENT, S2WERR_NULL_POINTER);
                    return;
                }
                if caps.is_null() {
                    r.finish(S2ERR_OUTOFSERVICE, S2WERR_GENERIC_ERROR);
                    return;
                }
                let size = core::ptr::addr_of!((*q).s2xq_Size).read_unaligned() as usize;
                // What fits: whole fields only, up to s2xq_Size bytes.
                let ends = [8usize, 10, 12, 16, core::mem::size_of::<x::S2XQuery>()];
                let fill = ends.iter().copied().filter(|&e| e <= size).max().unwrap_or(4);
                let a = x::S2XQuery {
                    s2xq_Size: size as u32,
                    s2xq_Magic: x::S2X_MAGIC,
                    s2xq_Version: x::S2X_VERSION,
                    s2xq_Revision: S2X_REVISION,
                    s2xq_Features: (*caps).features,
                    s2xq_Capabilities: core::ptr::addr_of!((*caps).list),
                };
                if fill > 4 {
                    core::ptr::copy_nonoverlapping((&a as *const x::S2XQuery as *const u8).add(4), (q as *mut u8).add(4), fill - 4);
                }
                r.finish(0, 0);
            }
            S2_CONFIGINTERFACE | S2_ONLINE | S2_OFFLINE | S2_ADDMULTICASTADDRESS | S2_DELMULTICASTADDRESS
            | S2_ADDMULTICASTADDRESSES | S2_DELMULTICASTADDRESSES | CMD_FLUSH | x::S2X_CONFIGURE => to_task(u, r),
            #[cfg(feature = "diag")]
            E1000E_CMD_TESTFAULT | E1000E_CMD_DIAG | E1000E_CMD_TXMUTE | E1000E_CMD_TUNE | E1000E_CMD_ITR | E1000E_CMD_PRI => to_task(u, r),
            S2_SAMPLE_THROUGHPUT => {
                let st = r.stat_data() as *const u32;
                if st.is_null() {
                    r.finish(S2ERR_BAD_ARGUMENT, S2WERR_NULL_POINTER);
                } else if core::ptr::read_unaligned(st) < 8 {
                    r.finish(IOERR_BADLENGTH, S2WERR_BAD_STATDATA);
                } else {
                    to_task(u, r);
                }
            }
            S2_SANA2HOOK => {
                let res = u.shared.with(|s| {
                    let _ = s;
                    (*op).bm.install_hook(r.data() as *mut Sana2Hook, r.data_length())
                });
                match res {
                    Ok(()) => {
                        serial_println!("{} unit {}: stack copy hook installed", NAME, u.index());
                        r.finish(0, 0)
                    }
                    Err((e, w)) => r.finish(e, w),
                }
            }
            S2_DEVICEQUERY => {
                let q = r.stat_data() as *mut u8;
                if q.is_null() {
                    r.finish(S2ERR_BAD_ARGUMENT, S2WERR_NULL_POINTER);
                    return;
                }
                let avail = core::ptr::read_unaligned(q as *const u32) as usize;
                let (mtu, bps) = u.shared.with(|s| (s.mtu, s.bps));
                let mut full: Sana2DeviceQuery = core::mem::zeroed();
                full.SizeAvailable = avail as u32;
                let supply = avail.min(core::mem::size_of::<Sana2DeviceQuery>());
                full.SizeSupplied = supply as u32;
                full.AddrFieldSize = 48;
                full.MTU = mtu;
                full.BPS = bps;
                full.HardwareType = S2_WIRETYPE_ETHERNET;
                full.RawMTU = mtu + eth::HLEN as u32;
                // SizeAvailable is the caller's; write from SizeSupplied on.
                if supply >= 8 {
                    core::ptr::copy_nonoverlapping((&full as *const _ as *const u8).add(4), q.add(4), supply - 4);
                }
                r.finish(0, 0);
            }
            S2_GETSTATIONADDRESS => {
                let (st, fa, cfg) = u.shared.with(|s| (s.station, s.factory, s.configured));
                r.set_src_mac(if cfg { &st } else { &fa });
                r.set_dst_mac(&fa);
                r.finish(0, 0);
            }
            S2_GETGLOBALSTATS => {
                let p = r.stat_data() as *mut Sana2DeviceStats;
                if p.is_null() {
                    r.finish(S2ERR_BAD_ARGUMENT, S2WERR_NULL_POINTER);
                    return;
                }
                let (st, ls) = u.shared.with(|s| (s.stats, s.last_start));
                let v = Sana2DeviceStats {
                    PacketsReceived: st.rx_packets as u32,
                    PacketsSent: st.tx_packets as u32,
                    BadData: st.bad_data as u32,
                    Overruns: st.overruns as u32,
                    Unused: 0,
                    UnknownTypesReceived: st.unknown_types as u32,
                    Reconfigurations: st.reconfigurations as u32,
                    LastStart: ls,
                };
                core::ptr::write_unaligned(p, v);
                r.finish(0, 0);
            }
            S2_GETEXTENDEDGLOBALSTATS => {
                let p = r.stat_data() as *mut u8;
                if p.is_null() {
                    r.finish(S2ERR_BAD_ARGUMENT, S2WERR_NULL_POINTER);
                    return;
                }
                let len = core::ptr::read_unaligned(p as *const u32) as usize;
                if len < 8 {
                    r.finish(IOERR_BADLENGTH, S2WERR_BAD_STATDATA);
                    return;
                }
                let (st, ls, up) = u.shared.with(|s| (s.stats, s.last_start, s.link_up));
                let mut v: Sana2ExtDeviceStats = core::mem::zeroed();
                let actual = len.min(core::mem::size_of::<Sana2ExtDeviceStats>());
                v.s2xds_Length = len as u32;
                v.s2xds_Actual = actual as u32;
                v.s2xds_PacketsReceived = S2Quad::from(st.rx_packets);
                v.s2xds_PacketsSent = S2Quad::from(st.tx_packets);
                v.s2xds_BadData = S2Quad::from(st.bad_data);
                v.s2xds_Overruns = S2Quad::from(st.overruns);
                v.s2xds_UnknownTypesReceived = S2Quad::from(st.unknown_types);
                v.s2xds_Reconfigurations = S2Quad::from(st.reconfigurations);
                v.s2xds_LastStart = ls;
                if up {
                    // A wired link: connected since the unit went online.
                    v.s2xds_LastConnected = ls;
                    v.s2xds_TimeConnected = TimeVal { Seconds: 0, Microseconds: 1 };
                }
                core::ptr::copy_nonoverlapping(&v as *const _ as *const u8, p, actual);
                r.finish(0, 0);
            }
            S2_GETSPECIALSTATS => {
                let h = r.stat_data() as *mut Sana2SpecialStatHeader;
                if h.is_null() {
                    r.finish(S2ERR_BAD_ARGUMENT, S2WERR_NULL_POINTER);
                    return;
                }
                let st = u.shared.with(|s| s.stats);
                let p = |i: usize| u.perf[i].load(Ordering::Relaxed) as u64;
                let counts = [
                    st.rx_no_read,
                    st.rx_filtered,
                    st.mcast_rejects,
                    st.tx_waits,
                    st.recoveries,
                    st.bad_data,
                    u.irq_claimed.load(Ordering::Relaxed) as u64,
                    p(perf::WAKES),
                    p(perf::WAKES_RX),
                    p(perf::TX_PUBLISH),
                    p(perf::TX_NOTIFY),
                    p(perf::RX_NOTIFY),
                    p(perf::TX_ACKS),
                    p(perf::RX_ACKS),
                    p(perf::TX_NOTIFY_T),
                    p(perf::ISR_T),
                    p(perf::TX_CALL_T),
                    p(perf::PEEK_WAKES),
                    p(perf::RX_T),
                    p(perf::RX_DELIVER_T),
                    p(perf::TDH_READS),
                    p(perf::RX_BY_WRITER),
                    p(perf::PRI_CHANGES),
                    p(perf::TX_DEFERRED),
                    p(perf::TX_OFFLOADS),
                ];
                let hwc = u.shared.with(|s| s.hw);
                let counts: Vec<u64> = counts.iter().copied().chain(hwc.iter().copied()).collect();
                let max = core::ptr::read_unaligned(core::ptr::addr_of!((*h).RecordCountMax)) as usize;
                let n = max.min(counts.len());
                let recs = (h as *mut u8).add(8) as *mut Sana2SpecialStatRecord;
                for i in 0..n {
                    core::ptr::write_unaligned(
                        recs.add(i),
                        Sana2SpecialStatRecord {
                            Type: (S2_WIRETYPE_ETHERNET << 16) | (i as u32 + 1),
                            Count: counts[i] as u32,
                            String: if i < SPECIAL_NAMES.len() { SPECIAL_NAMES[i].as_ptr() } else { hw::COUNTERS[i - SPECIAL_NAMES.len()].1.as_ptr() },
                        },
                    );
                }
                core::ptr::write_unaligned(core::ptr::addr_of_mut!((*h).RecordCountSupplied), n as u32);
                r.finish(0, 0);
            }
            S2_TRACKTYPE | S2_UNTRACKTYPE | S2_GETTYPESTATS => {
                let t = r.packet_type();
                let op = r.buffer_management() as *mut Opener;
                let res = u.shared.with(|s| {
                    // Each opener tracks its own types and sees its own counts.
                    let pos = s.tracked.iter().position(|(x, _, o)| *x == t && *o == op);
                    match (cmd, pos) {
                        (S2_TRACKTYPE, Some(_)) => Err(S2WERR_ALREADY_TRACKED),
                        (S2_TRACKTYPE, None) => {
                            s.tracked.push((t, Sana2PacketTypeStats::default(), op));
                            Ok(None)
                        }
                        (S2_UNTRACKTYPE, Some(p)) => {
                            s.tracked.remove(p);
                            Ok(None)
                        }
                        (_, Some(p)) => Ok(Some(s.tracked[p].1)),
                        (_, None) => Err(S2WERR_NOT_TRACKED),
                    }
                });
                match res {
                    Ok(Some(st)) => {
                        let p = r.stat_data() as *mut Sana2PacketTypeStats;
                        if p.is_null() {
                            r.finish(S2ERR_BAD_ARGUMENT, S2WERR_NULL_POINTER);
                        } else {
                            core::ptr::write_unaligned(p, st);
                            r.finish(0, 0);
                        }
                    }
                    Ok(None) => r.finish(0, 0),
                    Err(w) => r.finish(S2ERR_BAD_STATE, w),
                }
            }
            S2_ONEVENT => {
                let want = r.wire_error();
                if want == 0 || want & !ALL_EVENTS != 0 {
                    r.finish(S2ERR_NOT_SUPPORTED, S2WERR_BAD_EVENT);
                    return;
                }
                let now_hit = u.shared.with(|s| {
                    let mut hit = 0;
                    if want & S2EVENT_ONLINE != 0 && s.online {
                        hit |= S2EVENT_ONLINE;
                    }
                    if want & S2EVENT_OFFLINE != 0 && !s.online {
                        hit |= S2EVENT_OFFLINE;
                    }
                    // The link too: a client waiting for CONNECT while the
                    // link is already up would otherwise wait forever.
                    if want & S2EVENT_CONNECT != 0 && s.link_up {
                        hit |= S2EVENT_CONNECT;
                    }
                    if want & S2EVENT_DISCONNECT != 0 && !s.link_up {
                        hit |= S2EVENT_DISCONNECT;
                    }
                    if hit == 0 {
                        io.mark_pending();
                        s.events.push(r);
                    }
                    hit
                });
                if now_hit != 0 {
                    r.finish(0, now_hit);
                }
            }
            // Point-to-point commands: not for Ethernet.
            _ => r.finish(IOERR_NOCMD, 0),
        }
    }
}

/// Queue a request for the unit task.
unsafe fn to_task(u: &Unit, r: S2Req) {
    let io = r.io();
    if !u.worker.is_ready() {
        r.finish(IOERR_UNITBUSY, 0);
        return;
    }
    // The unit task replies it later, so it is not quick: with a DoIO's
    // IOF_QUICK left set, DoIO returned at once, before the command was
    // done (io_Error not yet set), and the late ReplyMsg left the request
    // on the caller's port (S2_CONFIGINTERFACE, S2_ONLINE, S2_OFFLINE, the
    // multicast commands, CMD_FLUSH, S2_SAMPLE_THROUGHPUT).
    io.mark_pending();
    u.cmdq.push(io);
    if !u.worker.wake() && u.cmdq.abort(io) == Abort::Removed {
        r.reply(IOERR_UNITBUSY, 0);
    }
}

#[no_mangle]
pub extern "C" fn rust_dev_abort_io(ior: *mut IORequest) {
    let Some(u) = unit_of(ior) else { return };
    // SAFETY: the caller's request; lists under the unit's mutex.
    unsafe {
        let Some(io) = IoReq::from_raw(ior) else { return };
        if u.cmdq.abort(io) == Abort::Removed {
            io.reply(IOERR_ABORTED, 0);
            return;
        }
        if req_len(ior) < S2REQ_SIZE {
            return;
        }
        let r = S2Req::from_raw(ior).unwrap();
        // A read batch is replied with the frames it already holds counted
        // (s2xb_Done), a write batch with the frames already taken. A read
        // batch being filled right now is out of the list for that moment:
        // the abort then finds nothing, and the batch is replied at the end
        // of the receive pass anyway.
        enum Found {
            No,
            Read(Rd),
            Other,
            Write(Tx),
        }
        let found = u.shared.with(|s| {
            if let Some(rd) = s.reads.iter().position(|rd| rd.r == r).and_then(|pos| s.reads.remove(pos)) {
                return Found::Read(rd);
            }
            for list in [&mut s.orphans, &mut s.events, &mut s.samples] {
                if let Some(pos) = list.iter().position(|&o| o == r) {
                    list.remove(pos);
                    return Found::Other;
                }
            }
            Found::No
        });
        let found = match found {
            Found::No => u.tx.with(|tx| {
                let Some(tx) = tx.as_mut() else { return Found::No };
                match tx.pending.iter().position(|t| t.req() == r) {
                    Some(pos) => Found::Write(tx.pending.remove(pos).unwrap()),
                    None => Found::No,
                }
            }),
            f => f,
        };
        match found {
            Found::No => {}
            Found::Read(rd) => rd.end(IOERR_ABORTED, 0),
            Found::Other => r.reply(IOERR_ABORTED, 0),
            Found::Write(t) => t.end(IOERR_ABORTED, 0),
        }
    }
}
