/*
 * diag_glue.c - entry point for e1000ediag.
 *
 * No C runtime (-nostartfiles -nodefaultlibs -lgcc). _start() sets IExec,
 * opens dos.library (Shell output) and expansion.library's "pci" interface
 * (RAW mode), and passes the argument string to Rust.
 */

#define __NOLIBBASE__
#define __NOGLOBALIFACE__
#include <proto/exec.h>
#include <proto/dos.h>
#include <proto/expansion.h>
#include <exec/types.h>

/* Read by the amigaos4 crate (sys::IExec, sys::IDOS, sys::IPCI). */
struct ExecIFace *IExec = NULL;
struct DOSIFace  *IDOS  = NULL;
struct PCIIFace  *IPCI  = NULL;

#ifndef GIT_HASH
#define GIT_HASH "unknown"
#endif
/* The version and the commit it was built from (Staging rule 2). */
const char build_banner[] = "$VER: e1000e_diag 0.2 (10.11.2026) build " GIT_HASH;

extern int32 rust_main(STRPTR args, int32 arglen);

int32 _start(STRPTR argstring, int32 arglen, struct ExecBase *sysbase)
{
    struct Library *DOSBase, *ExpansionBase;
    int32 ret = RETURN_FAIL;

    IExec = (struct ExecIFace *)sysbase->MainInterface;
    (void)*(volatile const char *)build_banner;   /* keep it linked */
    IExec->Obtain();

    ExpansionBase = IExec->OpenLibrary("expansion.library", 50);
    if (ExpansionBase)
        IPCI = (struct PCIIFace *)IExec->GetInterface(ExpansionBase, "pci", 1, NULL);

    DOSBase = IExec->OpenLibrary("dos.library", 52);
    if (DOSBase) {
        IDOS = (struct DOSIFace *)IExec->GetInterface(DOSBase, "main", 1, NULL);
        if (IDOS) {
            ret = rust_main(argstring, arglen);
            IExec->DropInterface((struct Interface *)IDOS);
        }
        IExec->CloseLibrary(DOSBase);
    }

    if (IPCI)
        IExec->DropInterface((struct Interface *)IPCI);
    if (ExpansionBase)
        IExec->CloseLibrary(ExpansionBase);
    IExec->Release();
    return ret;
}
