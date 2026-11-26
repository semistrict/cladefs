# Describe all 256 buses of the ECAM window in the MCFG and the root bridge's _CRS

**Tried:** 2026-11-14, board 13111, branch `task-16-pci` (before its first commit).
**Idea:** the address map reserves a 256 MiB ECAM window, room for buses 0-255, so describe it
whole (MCFG end bus 0xff, `_CRS` WordBusNumber 0-255): the most general description, ready for
bridges later.
**Result:** every boot spent ~60 ms more in the guest kernel (kernel uptime at init 0.07 s instead
of 0.01 s; quiet PVH boot ~95 ms instead of ~37 ms). x86 Linux's `pci_subsys_init` calls
`pcibios_fixup_peer_bridges` (arch/x86/pci/legacy.c), which, for every bus up to
`pcibios_last_bus` (set from the MCFG's end bus) that it has not found, reads the vendor ID of
all 32 devices looking for peer root bridges: 8160 configuration reads, each an ECAM exit.
With bus 0 alone described, a boot makes 610 configuration space accesses in all, and boots as
fast as before PCI existed.
**Why it's dead:** describing buses the platform does not have costs a guest exit per device per
bus at every boot, for nothing: libmachine has bus 0 only.
**Reopen if:** libmachine gets PCI bridges (more buses). Describe exactly the buses behind them;
the peer-bridge scan skips buses Linux already found, but every described bus without a bridge
still costs 32 exits. The `devices::guest_tests` test bounds a boot's configuration space
accesses (under 2000) and fails if this comes back.
