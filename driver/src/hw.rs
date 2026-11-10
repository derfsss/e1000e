//! The Intel 82574L: registers, descriptors, reset and set-up.
//!
//! Register offsets and bit values are Intel's (the BSD-licensed
//! e1000_regs.h / e1000_defines.h / e1000_82571.h that FreeBSD carries);
//! the reset and initialisation follow the 82574 path of Intel's 82571
//! family code (e1000_reset_hw_82571, e1000_initialize_hw_bits_82571,
//! e1000_init_hw_82571), the PHY and flow-control steps e1000_phy.c and
//! e1000_mac.c, and the pause time, packet buffer split and ASPM handling
//! FreeBSD's em driver (if_em.c), all written afresh here. See
//! THIRD_PARTY_NOTICES.md for their licences. QEMU's `-device e1000e`
//! emulates the same chip.
//!
//! Registers are little-endian and reached through BAR 0 (128 KB of
//! memory): `arch::mmio_r32` / `mmio_w32` are byte-reversing loads and
//! stores. Descriptors are little-endian in memory.

// The release build leaves out the diag build's register uses.
#![cfg_attr(not(feature = "diag"), allow(dead_code))]

use amigaos4::arch;

/// PCI IDs of the 82574 family this driver takes.
pub const VENDOR_INTEL: u32 = 0x8086;
pub const DEVICE_IDS: [u32; 2] = [
    0x10D3, // 82574L (QEMU's e1000e)
    0x10F6, // 82574LA
];

pub mod reg {
    pub const CTRL: u32 = 0x00000;
    pub const STATUS: u32 = 0x00008;
    pub const EECD: u32 = 0x00010;
    pub const EERD: u32 = 0x00014;
    pub const CTRL_EXT: u32 = 0x00018;
    pub const ICR: u32 = 0x000C0;
    pub const ITR: u32 = 0x000C4;
    pub const RXCSUM: u32 = 0x05000;
    pub const IMS: u32 = 0x000D0;
    pub const IMC: u32 = 0x000D8;
    pub const RCTL: u32 = 0x00100;
    pub const TCTL: u32 = 0x00400;
    pub const TIPG: u32 = 0x00410;
    pub const EXTCNF_CTRL: u32 = 0x00F00;
    pub const RDBAL: u32 = 0x02800;
    pub const RDBAH: u32 = 0x02804;
    pub const RDLEN: u32 = 0x02808;
    pub const RDH: u32 = 0x02810;
    pub const RDT: u32 = 0x02818;
    pub const RDTR: u32 = 0x02820;
    pub const RADV: u32 = 0x0282C;
    pub const TDBAL: u32 = 0x03800;
    pub const TDBAH: u32 = 0x03804;
    pub const TDLEN: u32 = 0x03808;
    pub const TDH: u32 = 0x03810;
    pub const TDT: u32 = 0x03818;
    pub const TIDV: u32 = 0x03820;
    pub const TXDCTL0: u32 = 0x03828;
    pub const TARC0: u32 = 0x03840;
    pub const CRCERRS: u32 = 0x04000;
    pub const MPC: u32 = 0x04010;
    pub const MTA: u32 = 0x05200;
    pub const RAL0: u32 = 0x05400;
    pub const RAH0: u32 = 0x05404;
    pub const GCR: u32 = 0x05B00;
    pub const GCR2: u32 = 0x05B64;
    pub const MDIC: u32 = 0x00020;
    pub const FCAL: u32 = 0x00028;
    pub const FCAH: u32 = 0x0002C;
    pub const FCT: u32 = 0x00030;
    pub const ICS: u32 = 0x000C8;
    pub const FCTTV: u32 = 0x00170;
    pub const PBA: u32 = 0x01000;
    pub const FCRTL: u32 = 0x02160;
    pub const FCRTH: u32 = 0x02168;
    // Statistics, all clear on read.
    pub const ALGNERRC: u32 = 0x04004;
    pub const RXERRC: u32 = 0x0400C;
    pub const LATECOL: u32 = 0x04020;
    pub const COLC: u32 = 0x04028;
    pub const SEC: u32 = 0x04038;
    pub const RLEC: u32 = 0x04040;
    pub const XONRXC: u32 = 0x04048;
    pub const XONTXC: u32 = 0x0404C;
    pub const XOFFRXC: u32 = 0x04050;
    pub const XOFFTXC: u32 = 0x04054;
    pub const RNBC: u32 = 0x040A0;
    pub const RUC: u32 = 0x040A4;
    pub const RFC: u32 = 0x040A8;
    pub const ROC: u32 = 0x040AC;
}

/// PHY (MII) registers and bits (e1000_defines.h). The 82574's PHY is at
/// MDIO address 1.
pub mod phy {
    pub const ADDR: u32 = 1;
    pub const CONTROL: u32 = 0x00;
    pub const STATUS: u32 = 0x01;
    pub const ID1: u32 = 0x02;
    pub const ID2: u32 = 0x03;
    pub const AUTONEG_ADV: u32 = 0x04;
    pub const LP_ABILITY: u32 = 0x05;
    pub const GBIT_CTRL: u32 = 0x09;
    pub const GBIT_STATUS: u32 = 0x0A;
    /// M88-style PHY specific status (real-time speed, duplex, link).
    pub const SPEC_STATUS: u32 = 0x11;
    pub const CR_RESTART_AUTO_NEG: u16 = 0x0200;
    pub const CR_POWER_DOWN: u16 = 0x0800;
    pub const CR_AUTO_NEG_EN: u16 = 0x1000;
    pub const AR_SELECTOR_FIELD: u16 = 0x0001;
    pub const AR_10T_HD_CAPS: u16 = 0x0020;
    pub const AR_10T_FD_CAPS: u16 = 0x0040;
    pub const AR_100TX_HD_CAPS: u16 = 0x0080;
    pub const AR_100TX_FD_CAPS: u16 = 0x0100;
    pub const AR_PAUSE: u16 = 0x0400;
    pub const AR_ASM_DIR: u16 = 0x0800;
    pub const LPAR_PAUSE: u16 = 0x0400;
    pub const LPAR_ASM_DIR: u16 = 0x0800;
    pub const CR_1000T_FD_CAPS: u16 = 0x0200;
    /// What the driver advertises: every speed but 1000 half duplex (which
    /// Intel's code does not advertise either), and symmetric plus
    /// asymmetric pause.
    pub const WANT_ADV: u16 = AR_SELECTOR_FIELD
        | AR_10T_HD_CAPS
        | AR_10T_FD_CAPS
        | AR_100TX_HD_CAPS
        | AR_100TX_FD_CAPS
        | AR_PAUSE
        | AR_ASM_DIR;
    /// The advertisement bits the driver compares and sets.
    pub const ADV_MASK: u16 = 0x0DE1;
}

pub mod bit {
    // CTRL
    pub const CTRL_GIO_MASTER_DISABLE: u32 = 0x0000_0004;
    pub const CTRL_SLU: u32 = 0x0000_0040;
    pub const CTRL_RST: u32 = 0x0400_0000;
    pub const CTRL_RFCE: u32 = 0x0800_0000;
    pub const CTRL_TFCE: u32 = 0x1000_0000;
    // MDIC
    pub const MDIC_REG_SHIFT: u32 = 16;
    pub const MDIC_PHY_SHIFT: u32 = 21;
    pub const MDIC_REG_MASK: u32 = 0x001F_0000;
    pub const MDIC_OP_WRITE: u32 = 0x0400_0000;
    pub const MDIC_OP_READ: u32 = 0x0800_0000;
    pub const MDIC_READY: u32 = 0x1000_0000;
    pub const MDIC_ERROR: u32 = 0x4000_0000;
    // Flow control (e1000_defines.h; FCTTV is FreeBSD em's EM_FC_PAUSE_TIME)
    pub const FLOW_CONTROL_ADDRESS_LOW: u32 = 0x00C2_8001;
    pub const FLOW_CONTROL_ADDRESS_HIGH: u32 = 0x0000_0100;
    pub const FLOW_CONTROL_TYPE: u32 = 0x8808;
    pub const FC_PAUSE_TIME: u32 = 0x0680;
    pub const FCRTL_XONE: u32 = 0x8000_0000;
    pub const FCRT_MASK: u32 = 0x0000_FFF8;
    // CTRL_EXT
    pub const CTRL_EXT_DRV_LOAD: u32 = 0x1000_0000;
    // STATUS
    pub const STATUS_FD: u32 = 0x0000_0001;
    pub const STATUS_LU: u32 = 0x0000_0002;
    pub const STATUS_SPEED_MASK: u32 = 0x0000_00C0;
    pub const STATUS_SPEED_100: u32 = 0x0000_0040;
    pub const STATUS_SPEED_1000: u32 = 0x0000_0080;
    pub const STATUS_GIO_MASTER_ENABLE: u32 = 0x0008_0000;
    // EECD, EERD
    pub const EECD_AUTO_RD: u32 = 0x0000_0200;
    pub const EERD_START: u32 = 1 << 0;
    pub const EERD_DONE: u32 = 1 << 1;
    pub const EERD_ADDR_SHIFT: u32 = 2;
    pub const EERD_DATA_SHIFT: u32 = 16;
    // EXTCNF_CTRL
    pub const EXTCNF_MDIO_SW_OWNERSHIP: u32 = 0x0000_0020;
    // Interrupt causes (ICR / IMS / IMC)
    #[allow(dead_code)]
    pub const INT_TXDW: u32 = 0x0000_0001;
    pub const INT_TXQE: u32 = 0x0000_0002;
    pub const INT_LSC: u32 = 0x0000_0004;
    pub const INT_RXDMT0: u32 = 0x0000_0010;
    pub const INT_RXO: u32 = 0x0000_0040;
    pub const INT_RXT0: u32 = 0x0000_0080;
    // RCTL
    pub const RCTL_EN: u32 = 0x0000_0002;
    pub const RCTL_UPE: u32 = 0x0000_0008;
    pub const RCTL_MPE: u32 = 0x0000_0010;
    pub const RCTL_BAM: u32 = 0x0000_8000;
    pub const RCTL_SZ_2048: u32 = 0x0000_0000;
    pub const RCTL_SECRC: u32 = 0x0400_0000;
    /// Long packets (jumbo frames) accepted.
    pub const RCTL_LPE: u32 = 0x0000_0020;
    /// Receive buffers of 16384 bytes (with BSEX).
    pub const RCTL_SZ_16384: u32 = 0x0001_0000;
    pub const RCTL_BSEX: u32 = 0x0200_0000;
    /// Packet buffer split (receive KB): 32 for normal frames, 22 when
    /// frames exceed 8192 bytes, so the transmit side (40 - this) holds a
    /// whole jumbo frame (FreeBSD em's em_reset for the 82574).
    pub const PBA_32K: u32 = 0x0020;
    pub const PBA_22K: u32 = 0x0016;
    // TCTL
    pub const TCTL_EN: u32 = 0x0000_0002;
    pub const TCTL_PSP: u32 = 0x0000_0008;
    pub const TCTL_CT_SHIFT: u32 = 4;
    pub const TCTL_COLD_SHIFT: u32 = 12;
    pub const COLLISION_THRESHOLD: u32 = 15;
    pub const COLLISION_DISTANCE: u32 = 63;
    // RAH
    pub const RAH_AV: u32 = 0x8000_0000;
    // GCR (e1000_82571.h)
    pub const GCR_L1_ACT_WITHOUT_L0S_RX: u32 = 0x0800_0000;
    // Legacy transmit descriptor, command byte (in the length word)
    pub const TXD_CMD_EOP: u32 = 0x0100_0000;
    pub const TXD_CMD_IFCS: u32 = 0x0200_0000;
    // Legacy receive descriptor
    pub const RXD_STAT_DD: u8 = 0x01;
    pub const RXD_STAT_EOP: u8 = 0x02;
    /// CE | SE | SEQ | CXE | RXE: a frame with any of these is bad.
    pub const RXD_ERR_FRAME: u8 = 0x01 | 0x02 | 0x04 | 0x10 | 0x80;
    // Receive checksum results (legacy descriptor, RXCSUM IPOFL/TUOFL on):
    // IPCS the IPv4 header was checked, TCPCS / UDPCS the TCP or UDP
    // checksum was; IPE / TCPE (errors) say it was bad.
    pub const RXD_STAT_IPCS: u8 = 0x40;
    /// TCPCS | UDPCS: a TCP or UDP checksum was checked.
    pub const RXD_STAT_L4I: u8 = 0x20 | 0x10;
    pub const RXD_ERR_IPE: u8 = 0x40;
    /// TCPE: the TCP or UDP checksum was bad.
    pub const RXD_ERR_L4E: u8 = 0x20;
    // RXCSUM
    pub const RXCSUM_IPOFL: u32 = 0x0000_0100;
    pub const RXCSUM_TUOFL: u32 = 0x0000_0200;
    // TCP/IP context descriptor, TUCMD (the descriptor's byte 11): TCP
    // (else UDP), IP (IPv4, else IPv6), TSE (segmentation), DEXT.
    pub const TXCTX_TUCMD_TCP: u32 = 0x01;
    pub const TXCTX_TUCMD_IP: u32 = 0x02;
    pub const TXCTX_TUCMD_TSE: u32 = 0x04;
    pub const TXCTX_TUCMD_DEXT: u32 = 0x20;
    // TCP/IP data descriptor (extended): DTYP 1, DCMD EOP, IFCS, TSE, DEXT;
    // POPTS (byte 13) IXSM (insert the IPv4 header checksum), TXSM (insert
    // the TCP or UDP checksum).
    pub const TXD_DTYP_DATA: u32 = 0x0010_0000;
    pub const TXD_DCMD_TSE: u32 = 0x0400_0000;
    pub const TXD_DCMD_DEXT: u32 = 0x2000_0000;
    pub const TXD_POPTS_IXSM: u32 = 0x01;
    pub const TXD_POPTS_TXSM: u32 = 0x02;
}

/// Bytes per legacy descriptor (receive and transmit).
pub const DESC_SIZE: usize = 16;
/// The multicast table: 128 registers of 32 bits (4096 hash buckets).
pub const MTA_WORDS: usize = 128;

/// The interrupt causes the driver enables: receive (timer, overrun,
/// descriptors running low), transmit queue empty, link change.
/// Transmit-descriptor write-back (TXDW) is not used: the driver never asks
/// for write-back (see `TxRing` in main.rs).
pub const IRQ_CAUSES: u32 = bit::INT_RXT0 | bit::INT_RXO | bit::INT_RXDMT0 | bit::INT_TXQE | bit::INT_LSC;

/// The interrupt throttling interval in 256 ns units: 196 = at most about
/// 20,000 interrupts a second (QEMU raises anything below 500).
pub const ITR_DEFAULT: u32 = 196;

/// What `phy_setup` found and did.
pub struct PhySetup {
    pub id: u32,
    pub powered_up: bool,
    pub restarted: bool,
    pub error: Option<&'static str>,
}

/// The card's statistics registers the driver adds up and reports (all
/// clear on read), with their S2_GETSPECIALSTATS names.
pub const COUNTERS: [(u32, &[u8]); 16] = [
    (reg::CRCERRS, amigaos4::amstr!("Card: CRC errors")),
    (reg::ALGNERRC, amigaos4::amstr!("Card: alignment errors")),
    (reg::SEC, amigaos4::amstr!("Card: symbol errors")),
    (reg::RXERRC, amigaos4::amstr!("Card: receive errors")),
    (reg::MPC, amigaos4::amstr!("Card: frames missed (no room in the card)")),
    (reg::RNBC, amigaos4::amstr!("Card: receive descriptors ran out")),
    (reg::RLEC, amigaos4::amstr!("Card: length errors")),
    (reg::RUC, amigaos4::amstr!("Card: undersize frames")),
    (reg::ROC, amigaos4::amstr!("Card: oversize frames")),
    (reg::RFC, amigaos4::amstr!("Card: fragments")),
    (reg::COLC, amigaos4::amstr!("Card: collisions")),
    (reg::LATECOL, amigaos4::amstr!("Card: late collisions")),
    (reg::XONRXC, amigaos4::amstr!("Card: XON pause frames received")),
    (reg::XOFFRXC, amigaos4::amstr!("Card: XOFF pause frames received")),
    (reg::XONTXC, amigaos4::amstr!("Card: XON pause frames sent")),
    (reg::XOFFTXC, amigaos4::amstr!("Card: XOFF pause frames sent")),
];

/// The card's registers: BAR 0's CPU address. Copy: the interrupt server
/// and the reset callback keep one too.
#[derive(Clone, Copy)]
pub struct Regs {
    base: u32,
}

impl Regs {
    pub const fn new(base: u32) -> Self {
        Regs { base }
    }

    /// # Safety
    /// `off` is a register of a live, mapped 82574 BAR 0.
    #[inline]
    pub unsafe fn r(&self, off: u32) -> u32 {
        // SAFETY: the caller's contract.
        unsafe { arch::mmio_r32(self.base + off) }
    }

    /// # Safety
    /// As `r`.
    #[inline]
    pub unsafe fn w(&self, off: u32, v: u32) {
        // SAFETY: the caller's contract.
        unsafe { arch::mmio_w32(self.base + off, v) }
    }

    /// Read STATUS, so that posted writes reach the card first.
    ///
    /// # Safety
    /// As `r`.
    pub unsafe fn flush(&self) {
        // SAFETY: the caller's contract.
        let _ = unsafe { self.r(reg::STATUS) };
    }

    /// Read (and so clear) the interrupt causes. Without MSI-X, reading ICR
    /// always clears it.
    ///
    /// # Safety
    /// As `r`.
    pub unsafe fn read_icr(&self) -> u32 {
        // SAFETY: the caller's contract.
        unsafe { self.r(reg::ICR) }
    }

    /// # Safety
    /// As `r`.
    pub unsafe fn mask_all(&self) {
        // SAFETY: the caller's contract.
        unsafe { self.w(reg::IMC, 0xFFFF_FFFF) };
    }

    /// # Safety
    /// As `r`.
    pub unsafe fn enable_irqs(&self) {
        // SAFETY: the caller's contract.
        unsafe { self.w(reg::IMS, IRQ_CAUSES) };
    }

    /// Stop all DMA and interrupts at once (the reset callback, the storm
    /// breaker): no interrupts, receive and transmit off.
    ///
    /// # Safety
    /// As `r`.
    pub unsafe fn quiesce(&self) {
        // SAFETY: the caller's contract.
        unsafe {
            self.mask_all();
            self.w(reg::RCTL, 0);
            let t = self.r(reg::TCTL);
            self.w(reg::TCTL, t & !bit::TCTL_EN);
            let _ = self.read_icr();
        }
    }

    /// Link state, and the speed in bit/s (0 when the link is down).
    ///
    /// # Safety
    /// As `r`.
    pub unsafe fn link(&self) -> (bool, u32) {
        // SAFETY: the caller's contract.
        let s = unsafe { self.r(reg::STATUS) };
        let up = s & bit::STATUS_LU != 0;
        let bps = match s & bit::STATUS_SPEED_MASK {
            bit::STATUS_SPEED_1000 => 1_000_000_000,
            bit::STATUS_SPEED_100 => 100_000_000,
            _ => 10_000_000,
        };
        (up, if up { bps } else { 0 })
    }

    /// One 16-bit word of the NVM through EERD; None if the read does not
    /// finish.
    ///
    /// # Safety
    /// As `r`.
    pub unsafe fn nvm_word(&self, addr: u32) -> Option<u16> {
        // SAFETY: the caller's contract.
        unsafe {
            self.w(reg::EERD, (addr << bit::EERD_ADDR_SHIFT) | bit::EERD_START);
            for _ in 0..10_000 {
                let v = self.r(reg::EERD);
                if v & bit::EERD_DONE != 0 {
                    return Some((v >> bit::EERD_DATA_SHIFT) as u16);
                }
                amigaos4::timer::micro_delay(5);
            }
        }
        None
    }

    /// The factory station address: receive address 0 (loaded from the NVM
    /// by the reset), else the NVM's first three words.
    ///
    /// # Safety
    /// As `r`.
    pub unsafe fn factory_mac(&self) -> Option<[u8; 6]> {
        // SAFETY: the caller's contract.
        unsafe {
            let (lo, hi) = (self.r(reg::RAL0), self.r(reg::RAH0));
            if hi & bit::RAH_AV != 0 && (lo != 0 || hi & 0xFFFF != 0) {
                let l = lo.to_le_bytes();
                let h = hi.to_le_bytes();
                return Some([l[0], l[1], l[2], l[3], h[0], h[1]]);
            }
            let mut m = [0u8; 6];
            for i in 0..3 {
                let w = self.nvm_word(i)?.to_le_bytes();
                m[i as usize * 2] = w[0];
                m[i as usize * 2 + 1] = w[1];
            }
            Some(m)
        }
    }

    /// Set receive address 0 (the station address the card accepts).
    ///
    /// # Safety
    /// As `r`.
    pub unsafe fn set_station(&self, m: &[u8; 6]) {
        // SAFETY: the caller's contract.
        unsafe {
            self.w(reg::RAL0, u32::from_le_bytes([m[0], m[1], m[2], m[3]]));
            self.w(reg::RAH0, u32::from_le_bytes([m[4], m[5], 0, 0]) | bit::RAH_AV);
        }
    }

    /// Reset the card and leave it quiet: the 82574 sequence of Intel's
    /// e1000_reset_hw_82571. `delay_ms` sleeps (the caller's timer).
    ///
    /// # Safety
    /// As `r`; nothing else may use the card meanwhile.
    pub unsafe fn reset(&self, delay_ms: &mut dyn FnMut(u32)) -> Result<(), &'static str> {
        // SAFETY: the caller's contract.
        unsafe {
            // Stop the card's PCIe bus mastering first, so the reset cannot
            // leave a transaction hanging (up to 800 x 100 us).
            let c = self.r(reg::CTRL);
            self.w(reg::CTRL, c | bit::CTRL_GIO_MASTER_DISABLE);
            let mut n = 0;
            while self.r(reg::STATUS) & bit::STATUS_GIO_MASTER_ENABLE != 0 && n < 800 {
                amigaos4::timer::micro_delay(100);
                n += 1;
            }
            self.mask_all();
            self.w(reg::RCTL, 0);
            let t = self.r(reg::TCTL);
            self.w(reg::TCTL, t & !bit::TCTL_EN);
            self.flush();
            delay_ms(10);
            // MDIO ownership around the MAC reset (it defaults to the
            // firmware afterwards).
            let owned = self.take_mdio(delay_ms);
            let c = self.r(reg::CTRL);
            self.w(reg::CTRL, c | bit::CTRL_RST);
            delay_ms(1);
            if owned {
                let e = self.r(reg::EXTCNF_CTRL);
                self.w(reg::EXTCNF_CTRL, e & !bit::EXTCNF_MDIO_SW_OWNERSHIP);
            }
            // The NVM is read back into the registers: wait for it (10 ms at
            // most), then for the PHY configuration that follows (25 ms).
            let mut n = 0;
            while self.r(reg::EECD) & bit::EECD_AUTO_RD == 0 && n < 10 {
                delay_ms(1);
                n += 1;
            }
            delay_ms(25);
            self.mask_all();
            let _ = self.read_icr();
            if self.r(reg::STATUS) == 0xFFFF_FFFF {
                return Err("the card does not answer after its reset");
            }
        }
        Ok(())
    }

    /// The MDIO software ownership flag (e1000_get_hw_semaphore_82574):
    /// true when taken.
    unsafe fn take_mdio(&self, delay_ms: &mut dyn FnMut(u32)) -> bool {
        // SAFETY: the caller's contract (reset's).
        unsafe {
            for _ in 0..200 {
                let e = self.r(reg::EXTCNF_CTRL);
                self.w(reg::EXTCNF_CTRL, e | bit::EXTCNF_MDIO_SW_OWNERSHIP);
                if self.r(reg::EXTCNF_CTRL) & bit::EXTCNF_MDIO_SW_OWNERSHIP != 0 {
                    return true;
                }
                delay_ms(1);
            }
        }
        false
    }

    /// The chip settings Intel's driver makes after a reset for the 82574
    /// (e1000_initialize_hw_bits_82571, e1000_init_hw_82571), then link up,
    /// an empty multicast table, and the driver-loaded flag for the
    /// firmware.
    ///
    /// # Safety
    /// As `r`.
    pub unsafe fn init_hw(&self) {
        // SAFETY: the caller's contract.
        unsafe {
            let v = self.r(reg::TXDCTL0);
            self.w(reg::TXDCTL0, v | (1 << 22));
            let v = self.r(reg::TARC0);
            self.w(reg::TARC0, (v & !(0xF << 27)) | (1 << 26));
            let v = self.r(reg::CTRL);
            self.w(reg::CTRL, (v & !(1 << 29)) | bit::CTRL_SLU);
            let v = self.r(reg::CTRL_EXT);
            self.w(reg::CTRL_EXT, (v & !(1 << 23)) | (1 << 22) | bit::CTRL_EXT_DRV_LOAD);
            // PCIe: Intel's settings and the 82574's errata 25 workaround.
            let v = self.r(reg::GCR);
            self.w(reg::GCR, (v | (1 << 22)) & !bit::GCR_L1_ACT_WITHOUT_L0S_RX);
            let v = self.r(reg::GCR2);
            self.w(reg::GCR2, v | 1);
            for i in 0..MTA_WORDS as u32 {
                self.w(reg::MTA + i * 4, 0);
            }
            // At most ~20,000 interrupts a second (ITR_DEFAULT); no receive or
            // transmit delay timers (the write path also picks up frames).
            self.w(reg::ITR, ITR_DEFAULT);
            self.w(reg::RDTR, 0);
            self.w(reg::RADV, 0);
            self.w(reg::TIDV, 0);
            // The statistics registers clear on read: start from zero.
            let _ = self.r(reg::CRCERRS);
            let _ = self.r(reg::MPC);
        }
    }

    /// Run `f` with the MDIO ownership flag held (the PHY is shared with
    /// the card's firmware); None if the flag cannot be taken.
    ///
    /// # Safety
    /// As `r`; only the unit task touches the PHY.
    pub unsafe fn with_mdio<R>(&self, f: impl FnOnce(&Self) -> R) -> Option<R> {
        // SAFETY: the caller's contract.
        unsafe {
            let owned = self.take_mdio(&mut |ms| amigaos4::timer::micro_delay(ms * 1000));
            if !owned {
                return None;
            }
            let r = f(self);
            let e = self.r(reg::EXTCNF_CTRL);
            self.w(reg::EXTCNF_CTRL, e & !bit::EXTCNF_MDIO_SW_OWNERSHIP);
            Some(r)
        }
    }

    /// Wait for an MDIC transaction (up to 1920 x 50 us, as Intel's code).
    unsafe fn mdic_wait(&self) -> Option<u32> {
        // SAFETY: the caller's contract (with_mdio's).
        unsafe {
            for _ in 0..1920 {
                amigaos4::timer::micro_delay(50);
                let v = self.r(reg::MDIC);
                if v & bit::MDIC_READY != 0 {
                    return if v & bit::MDIC_ERROR != 0 { None } else { Some(v) };
                }
            }
        }
        None
    }

    /// Read a PHY register (MDIO ownership held: `with_mdio`).
    ///
    /// # Safety
    /// As `with_mdio`.
    pub unsafe fn phy_read(&self, r: u32) -> Option<u16> {
        // SAFETY: the caller's contract.
        unsafe {
            self.w(reg::MDIC, (r << bit::MDIC_REG_SHIFT) | (phy::ADDR << bit::MDIC_PHY_SHIFT) | bit::MDIC_OP_READ);
            let v = self.mdic_wait()?;
            if (v & bit::MDIC_REG_MASK) >> bit::MDIC_REG_SHIFT != r {
                return None;
            }
            Some(v as u16)
        }
    }

    /// Write a PHY register (MDIO ownership held: `with_mdio`).
    ///
    /// # Safety
    /// As `with_mdio`.
    pub unsafe fn phy_write(&self, r: u32, v: u16) -> bool {
        // SAFETY: the caller's contract.
        unsafe {
            self.w(
                reg::MDIC,
                v as u32 | (r << bit::MDIC_REG_SHIFT) | (phy::ADDR << bit::MDIC_PHY_SHIFT) | bit::MDIC_OP_WRITE,
            );
            self.mdic_wait().is_some()
        }
    }

    /// Make sure the copper PHY is powered and auto-negotiating with the
    /// driver's advertisement (e1000_power_up_phy_copper,
    /// e1000_phy_setup_autoneg). Auto-negotiation is restarted only when
    /// something had to change: a restart takes a real link down for
    /// seconds, so an open of a card that is already set up keeps its link.
    ///
    /// # Safety
    /// As `r`; only the unit task (or setup) touches the PHY.
    pub unsafe fn phy_setup(&self, pause: u16) -> PhySetup {
        // SAFETY: the caller's contract.
        let done = unsafe {
            self.with_mdio(|r| -> Option<PhySetup> {
                let id = ((r.phy_read(phy::ID1)? as u32) << 16) | r.phy_read(phy::ID2)? as u32;
                let mut cr = r.phy_read(phy::CONTROL)?;
                let adv = r.phy_read(phy::AUTONEG_ADV)?;
                let gtc = r.phy_read(phy::GBIT_CTRL)?;
                let mut s = PhySetup { id, powered_up: false, restarted: false, error: None };
                if cr & phy::CR_POWER_DOWN != 0 {
                    cr &= !phy::CR_POWER_DOWN;
                    if !r.phy_write(phy::CONTROL, cr) {
                        return None;
                    }
                    s.powered_up = true;
                }
                // 1000 full duplex must be advertised; the half-duplex bit is
                // left as it is (a PHY default; changing it alone would cost a
                // restart, and the link partner never picks 1000 half anyway).
                let want_gtc = gtc | phy::CR_1000T_FD_CAPS;
                let want_adv = (adv & !phy::ADV_MASK) | (phy::WANT_ADV & !(phy::AR_PAUSE | phy::AR_ASM_DIR)) | (pause & (phy::AR_PAUSE | phy::AR_ASM_DIR));
                if adv != want_adv || gtc != want_gtc || cr & phy::CR_AUTO_NEG_EN == 0 {
                    if !r.phy_write(phy::AUTONEG_ADV, want_adv) || !r.phy_write(phy::GBIT_CTRL, want_gtc) {
                        return None;
                    }
                    if !r.phy_write(phy::CONTROL, cr | phy::CR_AUTO_NEG_EN | phy::CR_RESTART_AUTO_NEG) {
                        return None;
                    }
                    s.restarted = true;
                }
                Some(s)
            })
        };
        match done {
            None => PhySetup { id: 0, powered_up: false, restarted: false, error: Some("MDIO ownership not granted") },
            Some(None) => PhySetup { id: 0, powered_up: false, restarted: false, error: Some("a PHY register access failed") },
            Some(Some(s)) => s,
        }
    }

    /// Flow control set-up (e1000_setup_link_generic and FreeBSD em's
    /// em_reset): the pause frame address and type, the pause time, and
    /// receive thresholds from the packet buffer size (PBA).
    ///
    /// # Safety
    /// As `r`.
    pub unsafe fn fc_init(&self, max_frame: u32) {
        // SAFETY: the caller's contract.
        unsafe {
            self.w(reg::FCAL, bit::FLOW_CONTROL_ADDRESS_LOW);
            self.w(reg::FCAH, bit::FLOW_CONTROL_ADDRESS_HIGH);
            self.w(reg::FCT, bit::FLOW_CONTROL_TYPE);
            self.w(reg::FCTTV, bit::FC_PAUSE_TIME);
            let mut kb = self.r(reg::PBA) & 0xFFFF;
            if kb == 0 || kb > 40 {
                kb = 32; // the 82574's receive share (40 KB in all)
            }
            // High water: room for one maximum frame (rounded up to 1 KB)
            // after XOFF; low water 1500 bytes below it.
            let high = (kb << 10).saturating_sub((max_frame + 1023) & !1023).max(4096);
            let low = high - 1500;
            self.w(reg::FCRTL, (low & bit::FCRT_MASK) | bit::FCRTL_XONE);
            self.w(reg::FCRTH, high & bit::FCRT_MASK);
        }
    }

    /// At link up: pause frames each way as auto-negotiation resolved them
    /// (IEEE 802.3 annex 28B, as e1000_config_fc_after_link_up), set in
    /// CTRL. Half duplex never pauses. Returns the mode, for the log.
    ///
    /// # Safety
    /// As `r`; only the unit task touches the PHY.
    pub unsafe fn fc_resolve(&self) -> &'static str {
        // SAFETY: the caller's contract.
        unsafe {
            let fd = self.r(reg::STATUS) & bit::STATUS_FD != 0;
            let lp = self.with_mdio(|r| (r.phy_read(phy::AUTONEG_ADV), r.phy_read(phy::LP_ABILITY)));
            let (rfce, tfce, mode) = match lp {
                Some((Some(adv), Some(lpa))) if fd => {
                    let (lp_pause, lp_asm) = (lpa & phy::LPAR_PAUSE != 0, lpa & phy::LPAR_ASM_DIR != 0);
                    let (my_pause, my_asm) = (adv & phy::AR_PAUSE != 0, adv & phy::AR_ASM_DIR != 0);
                    if my_pause && lp_pause {
                        (true, true, "flow control both ways")
                    } else if !my_pause && my_asm && lp_pause && lp_asm {
                        (false, true, "flow control: sends pause frames")
                    } else if my_pause && my_asm && !lp_pause && lp_asm {
                        (true, false, "flow control: obeys pause frames")
                    } else {
                        (false, false, "no flow control (partner)")
                    }
                }
                Some(_) if !fd => (false, false, "no flow control (half duplex)"),
                _ => (false, false, "no flow control (PHY not readable)"),
            };
            let c = self.r(reg::CTRL) & !(bit::CTRL_RFCE | bit::CTRL_TFCE);
            self.w(reg::CTRL, c | if rfce { bit::CTRL_RFCE } else { 0 } | if tfce { bit::CTRL_TFCE } else { 0 });
            mode
        }
    }

    /// Read (and so clear) the statistics registers the driver reports, in
    /// the order of `COUNTERS`.
    ///
    /// # Safety
    /// As `r`; only the unit task reads them (they clear on read).
    pub unsafe fn read_counters(&self) -> [u32; COUNTERS.len()] {
        let mut v = [0u32; COUNTERS.len()];
        for (i, (off, _)) in COUNTERS.iter().enumerate() {
            // SAFETY: the caller's contract.
            v[i] = unsafe { self.r(*off) };
        }
        v
    }

    /// Hand the card back to its firmware (clear the driver-loaded flag).
    ///
    /// # Safety
    /// As `r`.
    pub unsafe fn release(&self) {
        // SAFETY: the caller's contract.
        unsafe {
            let v = self.r(reg::CTRL_EXT);
            self.w(reg::CTRL_EXT, v & !bit::CTRL_EXT_DRV_LOAD);
        }
    }

    /// Point the card at a receive ring of `n` descriptors (empty: the
    /// caller sets the tail once descriptors are posted) and enable
    /// receive with `rctl_mode` (UPE/MPE) added.
    ///
    /// # Safety
    /// As `r`; `phys` is a ring of `n` descriptors, 128-byte aligned.
    pub unsafe fn rx_queue_init(&self, q: usize, phys: u32, n: usize, _buf_size: usize) -> bool {
        // SAFETY: the caller's contract.
        unsafe {
            debug_assert!(q == 0, "the 82574 is used with one receive queue");
            self.w(reg::RDBAL, phys);
            self.w(reg::RDBAH, 0);
            self.w(reg::RDLEN, (n * DESC_SIZE) as u32);
            self.w(reg::RDH, 0);
            self.w(reg::RDT, 0);
        }
        true
    }

    /// The receive tail register of queue `q` (only queue 0 here).
    pub const fn rdt(_q: usize) -> u32 {
        reg::RDT
    }

    /// Receive off (RCTL 0), before the ring is set up.
    ///
    /// # Safety
    /// As `r`.
    pub unsafe fn rx_stop(&self) {
        // SAFETY: the caller's contract.
        unsafe { self.w(reg::RCTL, 0) };
    }

    /// Start receiving with `rctl_mode` (buffer size bits, UPE/MPE) added,
    /// with the card checking IPv4 header, TCP and UDP checksums.
    ///
    /// # Safety
    /// As `r`; the ring is set up (`rx_queue_init`).
    pub unsafe fn rx_enable(&self, rctl_mode: u32) {
        // SAFETY: the caller's contract.
        unsafe {
            self.w(reg::RXCSUM, bit::RXCSUM_IPOFL | bit::RXCSUM_TUOFL);
            self.w(reg::RCTL, bit::RCTL_EN | bit::RCTL_BAM | bit::RCTL_SECRC | bit::RCTL_SZ_2048 | rctl_mode);
        }
    }

    /// The interrupt throttling interval (256 ns units, 0: off).
    ///
    /// # Safety
    /// As `r`.
    pub unsafe fn itr(&self) -> u32 {
        // SAFETY: the caller's contract.
        unsafe { self.r(reg::ITR) & 0xFFFF }
    }

    /// Set the interrupt throttling interval (256 ns units, 0: off).
    ///
    /// # Safety
    /// As `r`.
    pub unsafe fn set_itr(&self, v: u32) {
        // SAFETY: the caller's contract.
        unsafe { self.w(reg::ITR, v & 0xFFFF) };
    }

    /// Pause frames forced, not negotiated: obey them (`rx`) and send them
    /// (`tx`) as given. Returns the mode, for the log.
    ///
    /// # Safety
    /// As `r`.
    pub unsafe fn fc_force(&self, rx: bool, tx: bool) -> &'static str {
        // SAFETY: the caller's contract.
        unsafe {
            let c = self.r(reg::CTRL) & !(bit::CTRL_RFCE | bit::CTRL_TFCE);
            self.w(reg::CTRL, c | if rx { bit::CTRL_RFCE } else { 0 } | if tx { bit::CTRL_TFCE } else { 0 });
        }
        match (rx, tx) {
            (true, true) => "flow control both ways (set)",
            (true, false) => "flow control: obeys pause frames (set)",
            (false, true) => "flow control: sends pause frames (set)",
            (false, false) => "no flow control (set)",
        }
    }

    /// After negotiation: turn off the directions the setting does not
    /// allow.
    ///
    /// # Safety
    /// As `r`.
    pub unsafe fn fc_mask(&self, rx: bool, tx: bool) {
        // SAFETY: the caller's contract.
        unsafe {
            let c = self.r(reg::CTRL);
            let off = if rx { 0 } else { bit::CTRL_RFCE } | if tx { 0 } else { bit::CTRL_TFCE };
            self.w(reg::CTRL, c & !off);
        }
    }

    /// Point the card at a transmit ring of `n` descriptors and enable
    /// transmit (short frames padded, full duplex timing).
    ///
    /// # Safety
    /// As `r`; `phys` is a ring of `n` descriptors, 128-byte aligned.
    pub unsafe fn tx_init(&self, phys: u32, n: usize) {
        // SAFETY: the caller's contract.
        unsafe {
            self.w(reg::TDBAL, phys);
            self.w(reg::TDBAH, 0);
            self.w(reg::TDLEN, (n * DESC_SIZE) as u32);
            self.w(reg::TDH, 0);
            self.w(reg::TDT, 0);
            // Inter-packet gap for copper: IPGT 8, IPGR1 8, IPGR2 6.
            self.w(reg::TIPG, 8 | (8 << 10) | (6 << 20));
            self.w(
                reg::TCTL,
                bit::TCTL_EN
                    | bit::TCTL_PSP
                    | (bit::COLLISION_THRESHOLD << bit::TCTL_CT_SHIFT)
                    | (bit::COLLISION_DISTANCE << bit::TCTL_COLD_SHIFT),
            );
        }
    }
}

/// The multicast table bucket of a group address (RCTL.MO = 0: address bits
/// 36..47), as (register, bit).
pub fn mta_bucket(m: &[u8; 6]) -> (usize, u32) {
    let h = ((m[4] as u32 >> 4) | ((m[5] as u32) << 4)) & 0xFFF;
    ((h >> 5) as usize, h & 31)
}

/// The multicast table for a list of group addresses.
pub fn mta_for(groups: &[[u8; 6]]) -> [u32; MTA_WORDS] {
    let mut t = [0u32; MTA_WORDS];
    for g in groups {
        let (r, b) = mta_bucket(g);
        t[r] |= 1 << b;
    }
    t
}

// Legacy descriptors, little-endian in memory.

/// Write a receive descriptor for the buffer at `phys` (status cleared).
///
/// # Safety
/// `d` points at a 16-byte descriptor.
pub unsafe fn rx_desc_set(d: *mut u8, phys: u32) {
    // SAFETY: the caller's contract; byte writes need no alignment.
    unsafe {
        core::ptr::copy_nonoverlapping((phys as u64).to_le_bytes().as_ptr(), d, 8);
        core::ptr::write_bytes(d.add(8), 0, 8);
    }
}

/// A receive descriptor's (length, status, errors).
///
/// # Safety
/// `d` points at a 16-byte descriptor.
pub unsafe fn rx_desc_get(d: *const u8) -> (u16, u8, u8) {
    // SAFETY: the caller's contract; volatile, since the card writes it.
    unsafe {
        let len = u16::from_le_bytes([core::ptr::read_volatile(d.add(8)), core::ptr::read_volatile(d.add(9))]);
        let status = core::ptr::read_volatile(d.add(12));
        let errors = core::ptr::read_volatile(d.add(13));
        (len, status, errors)
    }
}

/// Write a transmit descriptor: one whole frame of `len` bytes at `phys`,
/// with the CRC added by the card and no status write-back asked for.
///
/// # Safety
/// `d` points at a 16-byte descriptor.
pub unsafe fn tx_desc_set(d: *mut u8, phys: u32, len: u16) {
    // SAFETY: the caller's contract.
    unsafe {
        core::ptr::copy_nonoverlapping((phys as u64).to_le_bytes().as_ptr(), d, 8);
        let lower = len as u32 | bit::TXD_CMD_EOP | bit::TXD_CMD_IFCS;
        core::ptr::copy_nonoverlapping(lower.to_le_bytes().as_ptr(), d.add(8), 4);
        core::ptr::write_bytes(d.add(12), 0, 4);
    }
}

/// Write a TCP/IP context descriptor (the 82574's legacy offload context)
/// for a frame whose IP header starts at `maclen` and is `iplen` long:
/// the IPv4 header checksum over it (inserted at +10), and the TCP or UDP
/// checksum (`tucmd` TCP or not) from the end of the IP header to the end
/// of the frame, inserted at +16 or +6. With TSE (`mss` non-zero) the card
/// cuts the `paylen` bytes after the `maclen + iplen + l4len` bytes of
/// headers into `mss`-byte segments.
///
/// # Safety
/// `d` points at a 16-byte descriptor.
pub unsafe fn tx_ctx_set(d: *mut u8, maclen: u32, iplen: u32, tucmd: u32, mss: u32, l4len: u32, paylen: u32) {
    let ipcss = maclen;
    let tucss = maclen + iplen;
    let tucso = tucss + if tucmd & bit::TXCTX_TUCMD_TCP != 0 { 16 } else { 6 };
    let ipcse = (maclen + iplen).saturating_sub(1);
    let mut cmd = tucmd | bit::TXCTX_TUCMD_DEXT;
    let mut hdr = 0;
    if mss != 0 {
        cmd |= bit::TXCTX_TUCMD_TSE;
        hdr = maclen + iplen + l4len;
    }
    let w0 = (ipcss & 0xFF) | (((ipcss + 10) & 0xFF) << 8) | ((ipcse & 0xFFFF) << 16);
    let w1 = (tucss & 0xFF) | ((tucso & 0xFF) << 8);
    let w2 = (paylen & 0xF_FFFF) | ((cmd & 0xFF) << 24);
    let w3 = ((hdr & 0xFF) << 8) | ((mss & 0xFFFF) << 16);
    // SAFETY: the caller's contract.
    unsafe {
        for (k, w) in [w0, w1, w2, w3].iter().enumerate() {
            core::ptr::copy_nonoverlapping(w.to_le_bytes().as_ptr(), d.add(4 * k), 4);
        }
    }
}

/// Write a TCP/IP data descriptor (extended): `len` bytes at `phys`, the
/// whole frame, with the checksums `popts` asks for (IXSM, TXSM) and TSE
/// when the context asked for segmentation. No status write-back.
///
/// # Safety
/// `d` points at a 16-byte descriptor.
pub unsafe fn tx_desc_set_offload(d: *mut u8, phys: u32, len: u32, popts: u32, tse: bool) {
    let lower = (len & 0xF_FFFF) | bit::TXD_DTYP_DATA | bit::TXD_DCMD_DEXT | bit::TXD_CMD_EOP | bit::TXD_CMD_IFCS
        | if tse { bit::TXD_DCMD_TSE } else { 0 };
    let upper = (popts & 0xFF) << 8;
    // SAFETY: the caller's contract.
    unsafe {
        core::ptr::copy_nonoverlapping((phys as u64).to_le_bytes().as_ptr(), d, 8);
        core::ptr::copy_nonoverlapping(lower.to_le_bytes().as_ptr(), d.add(8), 4);
        core::ptr::copy_nonoverlapping(upper.to_le_bytes().as_ptr(), d.add(12), 4);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mta_bucket_of_all_hosts() {
        // 01:00:5e:00:00:01: bits 36..47 = 0x010 -> register 0, bit 16.
        assert_eq!(mta_bucket(&[0x01, 0x00, 0x5e, 0x00, 0x00, 0x01]), (0, 16));
        // 33:33:00:00:00:01 (IPv6 all nodes) -> 0x010 too.
        assert_eq!(mta_bucket(&[0x33, 0x33, 0x00, 0x00, 0x00, 0x01]), (0, 16));
        // ff:ff:ff:ff:ff:ff -> 0xfff -> register 127, bit 31.
        assert_eq!(mta_bucket(&[0xff; 6]), (127, 31));
    }
}
