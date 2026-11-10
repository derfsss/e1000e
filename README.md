# e1000e.device

A network driver for AmigaOS 4.1 Final Edition that lets your Amiga use an
**Intel 82574L gigabit Ethernet card**.

It is a SANA-II driver: once it is installed, your TCP/IP stack (Roadshow,
which comes with AmigaOS 4.1, or [rustsocket](https://github.com/derfsss/RustSocket))
can use the card like any other network card.

> **Status: beta.** The driver passes all 32 checks of the SANA-II test
> program s2test on QEMU (AmigaOne, Pegasos II and Sam460 machines), and
> every check of the SANA-II extensions test s2xtest that applies to it.
> It has not been tried on a real card yet. If you have one, your test
> results are very welcome.


## Which cards work?

Cards built around the **Intel 82574L** chip (PCI IDs `8086:10D3` and
`8086:10F6`), for example Intel's Gigabit CT Desktop Adapter (EXPI9301CT)
and its many clones. These are small PCI Express (PCIe x1) cards with one
network socket.

QEMU's `-device e1000e` emulates the same chip, so the driver also works in
QEMU.

Cards with other Intel chips (82540, I210, I350 and so on) are **not**
supported.


## What you need

- AmigaOS 4.1 Final Edition, on a machine with a free PCIe slot (for
  example an X5000).
- An 82574L card and a network cable.


## Installing

1. Download the release archive (`e1000e-<date>-<id>.lha`) and unpack it,
   for example with UnArc or LhA.
2. Open the `e1000e` drawer and double-click **install**.
3. Choose what to install:
   - **Everyday driver** (`e1000e.device`): for normal use. Pick this if
     you are unsure.
   - **Diagnostics build** (`e1000e_diag.device` and the `e1000e_diag`
     command): for testing and tuning (see below).
   - **Both**.
4. On the last page, tick **Open the New Connection wizard now**, then click
   **Finish**.

The installer only copies files. It does not change your network settings.


## Setting up the network connection

The Internet connection wizard (`SYS:Internet/New Connection`) sets the
card up:

1. Choose **Local Area Network (DSL/LAN/Router/Switch/Hub)** and click
   **Next**.
2. Choose **Manual configuration** and click **Next**. (The card is not in
   the wizard's list of known cards, so automatic configuration won't
   find it.)
3. In the **Name** box below the list, type `e1000e.device` (or
   `e1000e_diag.device` for the diagnostics build), or click the button
   beside it to pick the file from `DEVS:Networks`. Leave **Port** at
   **0** and ignore the speed slider. You can click
   **Test this configuration**. Then click **Next**.
4. Leave **Try dynamic configuration** ticked to get an address
   automatically (DHCP), or untick it and enter the IP address, subnet
   mask, gateway and DNS server yourself. Click **Next**.
5. Follow the remaining pages to name and save the connection.

Unit 0 is the first 82574L card; the driver supports up to four cards
(units 0 to 3).

### Setting it up by hand

You can also create the interface file yourself. Make a text file called
`DEVS:NetInterfaces/E1000E` containing:

```
device=e1000e.device
unit=0
configure=dhcp
```

For a fixed address, use `address=192.168.1.50` and
`netmask=255.255.255.0` instead of `configure=dhcp`. Restart the network
(or the computer) afterwards.


## The diagnostics build

`e1000e_diag.device` is the same driver with extra tools for testing. It has:

- a diagnostics report: the card's registers, link state and error counters
- settings you can change: jumbo frames, the interrupt rate and the speed
  options
- a log of its extra activity in `RAM:e1000e_diag.log`

Everything is controlled with the `e1000e_diag` command in a Shell:

```
e1000e_diag                 show the settings and the driver's report
e1000e_diag JUMBO=ON        use jumbo frames (MTU 9000)
e1000e_diag JUMBO=OFF       back to normal frames (MTU 1500)
e1000e_diag ITR=196         interrupt rate limit (196 = about 20,000 a second)
e1000e_diag BATCH=OFF       turn one speed option off (or ON again)
e1000e_diag SAVE ...        keep a setting after a reboot
e1000e_diag DEFAULTS        go back to the built-in settings
e1000e_diag RAW FULL        read the card directly, without any driver
e1000e_diag ?               list all the options
```

The speed options are `BATCH`, `WRITERRX`, `ADAPTIVE`, `LAZY`, `GROUPINV` and
`LEAN`, each `ON` or `OFF`. A changed MTU takes effect the next time the
driver starts, which is after a reboot.

Only use jumbo frames if your switch and the computer at the other end are
set to the same large MTU. Otherwise large transfers will fail.


## If something goes wrong

- **The device cannot be opened:** check that the card is seated
  properly. `e1000e_diag RAW` should list it. If it doesn't, AmigaOS does
  not see the card.
- **No link:** check the cable and the lights on the card.
  `e1000e_diag RAW FULL` shows the link state and the card's PHY (the part
  that talks to the cable).
- **Link, but no traffic:** install the diagnostics build and send the
  output of `e1000e_diag` and the file `RAM:e1000e_diag.log` with your
  report.

[docs/bring-up.md](docs/bring-up.md) describes a careful first test on new
hardware, with scripts in `tools/bring-up/`.


## Features

- Up to four cards.
- Gigabit speed, with auto-negotiation down to 10 Mbit/s.
- Flow control (pause frames), as agreed with the switch.
- Recovers automatically if the card stops responding.
- Works on machines whose caches do not snoop PCI traffic.
- Full SANA-II support: statistics, events, multicast, and promiscuous and
  exclusive modes.
- The unofficial SANA-II extensions, for stacks that know them (such as
  rustsocket 4.3): frames in batches, checksums checked and filled in by
  the card, TCP segmentation, jumbo frames, and interrupt moderation and
  flow control settings. Roadshow sees an ordinary SANA-II driver.
  Details: [docs/sana2-extensions.md](docs/sana2-extensions.md).


## Speed

Measured on QEMU's AmigaOne with its emulated 82574L, against virtio-net in
the same boot (QEMU's speed varies from boot to boot, so the ratio is what
counts), netprof, every byte checked:

| Stack | Receive | Send |
|---|---|---|
| rustsocket 4.3 (uses the SANA-II extensions) | 1.6-1.7 times virtio | 4.0-4.4 times virtio |
| Roadshow 1.15 | about the same as virtio | 1.05-1.3 times virtio |

With e1000e 0.1, which had no extensions, rustsocket received at the same
speed as virtio and sent at 1.4 times it. Real cards have not been
measured yet.


## Building from source

You only need this if you want to change the driver.

You need Linux (WSL on Windows works), Rust with the `amigaos` toolchain
from the amigaos4 crate (`amigaos4/tools/build-toolchain.sh`), and Docker
with the `walkero/amigagccondocker:os4-gcc15-3.6.1` image.

```
git clone --recursive https://github.com/derfsss/e1000e.git
cd e1000e
tools/build-driver.sh
```

This builds `driver/e1000e.device`, `driver/e1000e_diag.device` and
`diag/e1000e_diag`.

Testing in QEMU: add `-netdev user,id=n1 -device e1000e,netdev=n1` to an
AmigaOne, Pegasos II or Sam460 command line. The SANA-II checks are
s2test ([VirtioRust](https://github.com/derfsss/VirtioRust),
`testprogs/s2test`) and, for the extensions, s2xtest
([RustSocket](https://github.com/derfsss/RustSocket),
`testprogs/s2xtest`), which needs two cards linked to each other; see
[docs/sana2-extensions.md](docs/sana2-extensions.md#testing).


## Reporting a problem

Please raise problems on the
[GitHub issues page](https://github.com/derfsss/e1000e/issues).
It helps to include:

- **The machine and AmigaOS version**: for example an AmigaOne X5000 with
  AmigaOS 4.1 Final Edition Update 3.
- **The card**: its make and model, and the chip line from
  `e1000e_diag RAW` (for example `8086:10d3`).
- **The driver's version**: `Version DEVS:Networks/e1000e.device FULL` (or
  `e1000e_diag.device`).
- **The TCP/IP stack**: Roadshow or rustsocket, with its version
  (`Version bsdsocket.library FULL`).
- **What you did and what happened**: whether the device opens, whether
  the link comes up, whether traffic flows, and what you expected.
- **The diagnostics**: with the diagnostics build installed, the output
  of `e1000e_diag` and the file `RAM:e1000e_diag.log`.
- **The debug output**: straight after the problem, before rebooting,
  `DumpDebugBuffer >RAM:debug.txt`, and attach the file (or the lines
  that start with `e1000e`).
- **For a crash**: the crash report (Grim Reaper's details, or the serial
  output if you capture it).


## Licence

MIT. See [LICENSE](LICENSE).

Parts of the driver follow Intel's e1000 code and FreeBSD's em driver,
which are under BSD licences. See
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

Richard Gibbs <rich_gibbs@hotmail.com>
