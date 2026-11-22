# Magma OS

A custom console-style, controller-first gaming operating system written from scratch in freestanding Rust (`no_std`), targeting `x86_64` via UEFI and the Limine bootloader.

## Current Progress

- **Phase 1A — UEFI Boot & Serial Console:** Higher-half `x86_64` ELF kernel booted via UEFI + Limine v11 (protocol revision 6) with `COM1` (`0x3F8`) serial logging.
- **Phase 1B — Memory Map & Framebuffer Graphics:** Physical memory map parsing (`MemmapRequest`) and a 32-bit GOP framebuffer renderer with an integer-scaled 8×8 bitmap font.
- **Phase 1C — CPU Structures & Interrupts:** 64-bit GDT, TSS with dedicated double-fault emergency stack, 256-entry IDT, exception handlers (`#BP`, `#DF`, `#GP`, `#PF`), 8259 PIC remapping, and a 100 Hz PIT timer.
- **Phase 1D (Next) — Physical Frame Allocator & Kernel Heap:** Bitmap physical memory manager (`4 KiB` frames) and dynamic kernel heap allocator.

## Running in QEMU (Windows)

```powershell
.\run.ps1