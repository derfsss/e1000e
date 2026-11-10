# SANA-II extensions in e1000e.device

Besides plain SANA-II, e1000e.device offers the unofficial **SANA-II
extensions** (sana2ext, version 1). They let a TCP/IP stack hand the card
work that would otherwise cost CPU time: many frames per request,
checksums, and cutting large TCP sends into packets.

A stack that does not know the extensions (Roadshow, for example) never
sees them: to it, e1000e.device is an ordinary SANA-II driver. A stack that
knows them, such as [rustsocket](https://github.com/derfsss/RustSocket)
4.3, finds them by itself.

The full specification is in the amigaos4 crate:
[docs/devices/sana2ext.md](https://github.com/derfsss/rust-amigaos4-crate/blob/main/docs/devices/sana2ext.md),
with the C header `include/devices/sana2ext.h`.

## What it gains

On QEMU's AmigaOne with its emulated 82574L, rustsocket, netprof, against
virtio-net in the same boot:

| e1000e.device | Receive | Send |
|---|---|---|
| 0.1, plain SANA-II | 1.0 times virtio | 1.4 times virtio |
| 0.2, with the extensions | 1.6-1.7 times virtio | 4.0-4.4 times virtio |

Real cards have not been measured yet. On a real card the gain is mostly
CPU time; the same extensions on an Intel I210 with
[igb.device](https://github.com/derfsss/igb) raised rustsocket's receive
from 69 to 94-96 MB/s on an AmigaOne X5000.

## What the driver offers

| Capability | What e1000e.device offers |
|---|---|
| Batches (`S2XCAP_BATCH`) | `S2X_READBATCH` and `S2X_WRITEBATCH`, up to 64 frames a batch |
| Receive checksums (`S2XCAP_RXCHECKSUM`) | results for the IPv4 header, and for TCP and UDP over IPv4 |
| Send checksums (`S2XCAP_TXCHECKSUM`) | the IPv4 header, and TCP and UDP over IPv4 and IPv6, filled in by the card |
| MTU (`S2XCAP_MTU`) | 68 to 9000 bytes (jumbo frames), changed with `S2X_CONFIGURE` |
| Interrupt moderation (`S2XCAP_MODERATION`) | up to 16,776 microseconds between interrupts, one value for both directions |
| Flow control (`S2XCAP_FLOWCONTROL`) | pause frames each way, negotiated with the switch (the default) or forced |
| TCP segmentation (`S2XCAP_SEGMENT`) | TCP over IPv4, sends of up to 65000 bytes, segments of at least 64 bytes |

Receive-side scaling (`S2XCAP_RSS`) is not offered: the 82574 has only two
receive queues and they are meant for MSI-X, and a single CPU gains
nothing from them.

## How it behaves

### Batches

- A read batch takes every frame that is waiting when the driver looks
  (at least one, up to its count), then comes back. It does not wait to
  fill up.
- A received frame goes to the oldest waiting request of its type, a
  `CMD_READ` or a read batch. With several openers, each gets the frame
  once.
- A write batch is sent in the caller's task, with one notification to
  the card for the whole batch. If the transmit ring fills, the batch
  waits for room, as `CMD_WRITE` does.
- A frame that is refused (too long, say) stops a write batch: that frame
  and the batch get the error, and the frames after it are left alone,
  so the stack can send them again.
- Batches with more than 64 frames, with no frames, or for a queue other
  than 0 are refused.

### Checksums

- Receive results are given only to read batch frames that carry a
  checksum item, and only for IPv4 frames. A frame with a bad checksum is
  still delivered, marked bad, so the stack decides. IP fragments never
  get a TCP or UDP result.
- On send, the card fills in the IPv4 header checksum and a TCP or UDP
  checksum straight after the IP header (the 82574's TCP/IP context
  descriptor). Anything else a request describes (an ICMP checksum, say)
  the driver computes itself. A frame with a checksum request uses two
  transmit descriptors.
- Short frames with a checksum request go to the card at their real
  length and the card pads them to the Ethernet minimum, so the padding
  is never counted in a length or a checksum.

### Settings (`S2X_CONFIGURE`)

- **MTU:** changing it resets the card (with buffers and the packet
  buffer split for the new size), so the link may go down for a moment
  and come back. It is refused (`S2ERR_BAD_STATE`) while any receive
  request is posted: the stack aborts its reads first, changes the MTU,
  reads it back with `S2_DEVICEQUERY` and posts reads again. Requests
  posted during the reset stay queued.
- **Moderation and flow control** apply at once and are kept over card
  resets. Moderation is rounded to the card's 256 ns steps.
- With `S2XCF_READ`, every setting item is filled in with the value in
  force.

## Testing

- **s2xtest** (RustSocket, `testprogs/s2xtest`) checks the extensions
  item by item. It needs two units joined by one link, with no TCP/IP
  stack on them. In QEMU, two e1000e cards linked by `dgram` netdevs that
  send to each other, plus one on the user network for the echo checks:

  ```
  -netdev user,id=n3,net=10.0.5.0/24,dhcpstart=10.0.5.15 -device e1000e,netdev=n3,addr=0x0a
  -netdev dgram,id=a,local.type=inet,local.host=127.0.0.1,local.port=12360,remote.type=inet,remote.host=127.0.0.1,remote.port=12362
  -device e1000e,netdev=a,addr=0x0c
  -netdev dgram,id=b,local.type=inet,local.host=127.0.0.1,local.port=12362,remote.type=inet,remote.host=127.0.0.1,remote.port=12360
  -device e1000e,netdev=b,addr=0x0e
  ```

  then `s2xtest networks/e1000e_diag.device 1 2 echo=0,10.0.5.15,10.0.5.2,PORT`
  (PORT a host port something listens on). Result for 0.2: 72 passed,
  0 failed, 2 skipped (TSO over IPv6 and RSS, which are not offered).
- **s2test** (VirtioRust, `testprogs/s2test`): 32 of 32 on the
  diagnostics build. The everyday build has no test-fault command, so its
  recovery check fails (31 of 32), by design.
- **netprof** (RustSocket, `testprogs/netprof`, with `tools/netprof-host`
  on a PC) measures throughput through a stack, with the driver's own
  counters.
