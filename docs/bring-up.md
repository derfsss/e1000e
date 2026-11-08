# Bringing e1000e.device up on a new machine

These steps are for the first run on real hardware, for example an X5000 with
an 82574L card. They leave the machine's own network alone, so it stays
reachable remotely all the time.

## What you need

From the build, copied to one directory on the Amiga:

- `driver/e1000e_diag.device` (the diag build: diagnostics, settings, extras log)
- `diag/e1000e_diag` (the settings and diagnostics command)
- `s2test`, the SANA-II checker from VirtioRust's `testprogs/s2test`
- the scripts in `tools/bring-up/`: `firstboot` and `roadshow-try`

You also need a network cable from the card to a switch or a PC, and the
address of a machine on that network that answers ping.

## 1. Checks without a TCP/IP stack

```
Execute firstboot <dir> <free address> <machine that answers ping>
```

The script runs four steps and writes a report for each to `<dir>`:

1. `e1000e_diag RAW FULL`: the card's PCI and PCIe set-up, its registers,
   the PHY and the NVM, read before any driver runs.
2. It copies the driver to `DEVS:Networks/`.
3. s2test: 32 SANA-II checks, with ARP and ping on the card.
4. `e1000e_diag`: the settings and the driver's own report, with its rings, interrupts,
   flow control and the card's error counters.

## 2. Roadshow, for this boot only

```
Execute roadshow-try <address> <netmask>
```

Use a network the machine's built-in port is not on. Then measure with
netprof against netprof-host on the PC at the other end.

## 3. Tuning

With Roadshow on the card, change settings while netprof measures:

```
e1000e_diag ITR=100            more interrupts, lower latency
e1000e_diag BATCH=OFF          compare with transmit batching off
e1000e_diag SHOW               what is set now
e1000e_diag DEFAULTS           back to the built-in settings
```

`RAM:e1000e_diag.log` records each change and a summary every 5 seconds.
Settings that win become the everyday build's defaults. Add `SAVE` only to
keep a setting after a reboot.

Jumbo frames: `e1000e_diag MTU=9000`, then reboot, or let every program
close the unit. The switch and the other end must be set to the same MTU.

## When something fails

| Symptom | Look at |
|---|---|
| The device doesn't open | `firstboot-raw.txt`: is BAR 0 a memory range, and is memory decoding on? |
| No link | The PHY line: BMCR power-down bit (0x0800), the BMSR link bit (0x0004), the partner's abilities (ANLPAR) |
| Link up but no traffic | The `irq:` line (raised and claimed), RDH/RDT and TDH/TDT, and the card counters (CRC, missed, no descriptors) |
| Transmit stops after a while | The serial log: `transmit stalled` means the watchdog reset the card; check the ASPM state on the PCIe line |

A debug build writes the ring and interrupt state to the serial port once a
second:

```
tools/build-package.sh driver --features debug-log
```
