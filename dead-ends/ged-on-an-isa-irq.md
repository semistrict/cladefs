# Raise the VM generation ID's ACPI event on an ISA IRQ (GSI 5)

**Tried:** 2026-11-14, board 13095, branch `task-36-8-snapshots` (before its commit).
**Idea:** the Generic Event Device that tells the guest its VM generation ID changed needs an
interrupt; ISA IRQ 5 is free on libmachine's PC (COM1 has 4, the PIT 0/2), and KVM_IRQ_LINE on
GSI 5 after a restore raises it.
**Result:** clones of a booted guest were fine, but a snapshot taken before the machine's first
run restored into a guest that reset within 0.2 s, before printing a byte (even with
`earlyprintk`). KVM's default routing sends GSIs 0-15 to the legacy PIC as well as the I/O APIC;
a guest that has not yet masked its PIC (Linux does early in boot) gets the pulse through the
PIC's ExtINT on LINT0 with a vector nobody programmed. Moved to GSI 16, the restored guest boots.
**Why it's dead:** any GSI below 16 also reaches the legacy PIC, so an event raised at restore
can hit a guest in a state where the PIC is live. I/O APIC pins 16-23 have no PIC route.
**Reopen if:** the platform drops the legacy PIC from KVM's routing (no MADT PCAT_COMPAT, a
routing table without PIC entries), or the event moves to an MSI.
