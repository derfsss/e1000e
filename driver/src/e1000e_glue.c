/*
 * e1000e_glue.c - e1000e.device, a SANA-II network driver for the Intel
 * 82574L gigabit controller (QEMU -device e1000e, and real 82574L cards),
 * loaded from DEVS:Networks/.
 *
 * One Resident, RTF_NATIVE | RTF_AUTOINIT. devInit() opens
 * expansion.library's "pci" interface and utility.library (CallHookPkt, for
 * the stacks' copy hooks), then hands over to Rust, which finds and locks
 * the cards. Each unit is set up at its first open and stopped at its last
 * close (Rust). Expunge, with nothing open, asks Rust to unlock the cards,
 * then removes the device and returns its segment list. Close runs a
 * deferred expunge through Expunge.
 */

#include <proto/exec.h>
#include <proto/expansion.h>
#include <proto/utility.h>
#ifdef DIAG_BUILD
#include <proto/dos.h>
#endif
#include <exec/types.h>
#include <exec/resident.h>
#include <exec/devices.h>
#include <exec/execbase.h>
#include <exec/io.h>
#include <exec/errors.h>
#include <dos/dos.h>   /* BPTR */

#ifdef DIAG_BUILD
/* The diag build: private diagnostic, tuning and test commands, transmit
 * batching, jumbo frames, the RAM: extras log (which needs DOS). */
#define DEVNAME    "e1000e_diag.device"
#define VSTRING    "e1000e_diag.device 0.2 (10.11.2026)"
#else
#define DEVNAME    "e1000e.device"
#define VSTRING    "e1000e.device 0.2 (10.11.2026)"
#endif
#ifndef GIT_HASH
#define GIT_HASH   "unknown"
#endif
#define DEVVER     0
#define DEVREV     2
#define DEVPRI     0

/* Read by the amigaos4 crate (sys::IExec, sys::IPCI, sys::IUtility). */
struct ExecIFace    *IExec    = NULL;
struct PCIIFace     *IPCI     = NULL;
struct UtilityIFace *IUtility = NULL;
#ifdef DIAG_BUILD
struct DOSIFace     *IDOS     = NULL;   /* sys::IDOS: the RAM: log, ENV */
struct Library *DOSBase       = NULL;
#endif
struct Library *ExpansionBase = NULL;
struct Library *UtilityBase   = NULL;

struct MyDevice {
    struct Device devNode;
    uint16        pad;
    BPTR          segList;
};

extern int32 rust_net_init(void);
extern int32 rust_net_expunge(void);
extern int8  rust_dev_open(struct IORequest *ior, uint32 unit, uint32 flags);
extern void  rust_dev_close(struct IORequest *ior);
extern void  rust_dev_begin_io(struct IORequest *ior);
extern void  rust_dev_abort_io(struct IORequest *ior);

/* Not a program: run from a Shell, it returns at once. */
int32 _start(void)
{
    return -1;
}

static uint32 _manager_Obtain(struct Interface *self)
{
    return self->Data.RefCount++;
}

static uint32 _manager_Release(struct Interface *self)
{
    return self->Data.RefCount--;
}

static void close_libs(void)
{
#ifdef DIAG_BUILD
    if (IDOS) {
        IExec->DropInterface((struct Interface *)IDOS);
        IDOS = NULL;
    }
    if (DOSBase) {
        IExec->CloseLibrary(DOSBase);
        DOSBase = NULL;
    }
#endif
    if (IPCI) {
        IExec->DropInterface((struct Interface *)IPCI);
        IPCI = NULL;
    }
    if (ExpansionBase) {
        IExec->CloseLibrary(ExpansionBase);
        ExpansionBase = NULL;
    }
    if (IUtility) {
        IExec->DropInterface((struct Interface *)IUtility);
        IUtility = NULL;
    }
    if (UtilityBase) {
        IExec->CloseLibrary(UtilityBase);
        UtilityBase = NULL;
    }
}

static struct MyDevice *devInit(struct MyDevice *devBase, BPTR segList,
                                struct ExecIFace *iexec)
{
    IExec = iexec;

    devBase->devNode.dd_Library.lib_Node.ln_Type = NT_DEVICE;
    devBase->devNode.dd_Library.lib_Node.ln_Pri  = DEVPRI;
    devBase->devNode.dd_Library.lib_Node.ln_Name = (STRPTR)DEVNAME;
    devBase->devNode.dd_Library.lib_Flags        = LIBF_SUMUSED | LIBF_CHANGED;
    devBase->devNode.dd_Library.lib_Version      = DEVVER;
    devBase->devNode.dd_Library.lib_Revision     = DEVREV;
    devBase->devNode.dd_Library.lib_IdString     = (STRPTR)VSTRING;
    devBase->segList                             = segList;

    IExec->DebugPrintF(DEVNAME ": init (" VSTRING ", build " GIT_HASH ")\n");

    ExpansionBase = IExec->OpenLibrary("expansion.library", 50);
    if (ExpansionBase)
        IPCI = (struct PCIIFace *)IExec->GetInterface(ExpansionBase, "pci", 1, NULL);
    UtilityBase = IExec->OpenLibrary("utility.library", 50);
    if (UtilityBase)
        IUtility = (struct UtilityIFace *)IExec->GetInterface(UtilityBase, "main", 1, NULL);
#ifdef DIAG_BUILD
    DOSBase = IExec->OpenLibrary("dos.library", 52);
    if (DOSBase)
        IDOS = (struct DOSIFace *)IExec->GetInterface(DOSBase, "main", 1, NULL);
    if (!IDOS) {
        IExec->DebugPrintF(DEVNAME ": cannot open dos.library; device not added\n");
        close_libs();
        return NULL;
    }
#endif
    if (!IPCI || !IUtility) {
        IExec->DebugPrintF(DEVNAME ": cannot open expansion.library \"pci\" or utility.library; "
                           "device not added\n");
        close_libs();
        return NULL;
    }

    if (rust_net_init() != 0) {
        close_libs();
        return NULL;
    }
    return devBase;
}

static APTR devExpunge(struct Interface *self);

static void devOpen(struct Interface *self, struct IORequest *ior,
                    uint32 unit, uint32 flags)
{
    struct MyDevice *devBase = (struct MyDevice *)self->Data.LibBase;

    /* Counted first, so an Expunge while the unit starts finds it open. */
    devBase->devNode.dd_Library.lib_OpenCnt++;
    int8 err = rust_dev_open(ior, unit, flags);

    ior->io_Error = err;
    if (err == 0) {
        devBase->devNode.dd_Library.lib_Flags &= ~LIBF_DELEXP;
        ior->io_Device = (struct Device *)devBase;
        ior->io_Message.mn_Node.ln_Type = NT_REPLYMSG;
    } else {
        devBase->devNode.dd_Library.lib_OpenCnt--;
        ior->io_Device = NULL;
        ior->io_Unit   = NULL;
    }
}

static APTR devClose(struct Interface *self, struct IORequest *ior)
{
    struct MyDevice *devBase = (struct MyDevice *)self->Data.LibBase;

    rust_dev_close(ior);
    ior->io_Device = NULL;
    ior->io_Unit   = NULL;
    devBase->devNode.dd_Library.lib_OpenCnt--;

    if (devBase->devNode.dd_Library.lib_OpenCnt == 0 &&
        (devBase->devNode.dd_Library.lib_Flags & LIBF_DELEXP))
        return devExpunge(self);
    return NULL;
}

static APTR devExpunge(struct Interface *self)
{
    struct MyDevice *devBase = (struct MyDevice *)self->Data.LibBase;

    if (devBase->devNode.dd_Library.lib_OpenCnt > 0 || rust_net_expunge() == 0) {
        devBase->devNode.dd_Library.lib_Flags |= LIBF_DELEXP;
        return NULL;
    }
    close_libs();
    IExec->Remove(&devBase->devNode.dd_Library.lib_Node);
    BPTR seg = devBase->segList;
    IExec->DeleteLibrary(&devBase->devNode.dd_Library);
    return (APTR)seg;
}

static void devBeginIO(struct Interface *self __attribute__((unused)),
                       struct IORequest *ior)
{
    rust_dev_begin_io(ior);
}

static void devAbortIO(struct Interface *self __attribute__((unused)),
                       struct IORequest *ior)
{
    rust_dev_abort_io(ior);
}

static void *devManagerVectors[] = {
    (void *)_manager_Obtain,
    (void *)_manager_Release,
    (void *)0,
    (void *)0,
    (void *)devOpen,
    (void *)devClose,
    (void *)devExpunge,
    (void *)0,
    (void *)devBeginIO,
    (void *)devAbortIO,
    (void *)(APTR)-1,
};

static struct TagItem devManagerTags[] = {
    { MIT_Name,        (uint32)"__device" },
    { MIT_VectorTable, (uint32)devManagerVectors },
    { MIT_Version,     1 },
    { MIT_DataSize,    0 },
    { TAG_DONE,        0 }
};

static uint32 devInterfaces[] = {
    (uint32)devManagerTags,
    0
};

static struct TagItem devCreateTags[] = {
    { CLT_DataSize,   (uint32)sizeof(struct MyDevice) },
    { CLT_Interfaces, (uint32)devInterfaces },
    { CLT_InitFunc,   (uint32)devInit },
    { TAG_DONE,       0 }
};

static const char devName[]    = DEVNAME;
static const char devVString[] = "$VER: " VSTRING;

const struct Resident NetTag __attribute__((used, aligned(4))) = {
    RTC_MATCHWORD,
    (struct Resident *)&NetTag,
    (APTR)(&NetTag + 1),
    RTF_NATIVE | RTF_AUTOINIT,
    DEVVER,
    NT_DEVICE,
    DEVPRI,
    (STRPTR)devName,
    (STRPTR)(devVString + 6),
    (APTR)devCreateTags
};
