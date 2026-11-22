# Magma OS — Project Specification

> **Document type:** Product vision, architecture specification, and phased roadmap  
> **Project name:** Magma OS  
> **Status:** Design / pre-implementation  
> **Target platform:** x86-64 PCs and laptops  
> **Kernel language:** Rust (`no_std`)  
> **Initial boot path:** UEFI + Limine  
> **Primary development host:** Windows 11  
> **Initial virtual test platform:** QEMU  
> **Primary product goal:** A controller-first, console-style gaming operating system

This document records the agreed direction for Magma OS. It describes the intended system; it does **not** mean that any listed feature has already been implemented. Architecture decisions remain subject to explicit approval by the human project owner.

---

## 1. Product vision

Magma OS is a gaming-focused operating system with a console-style experience and a custom Rust kernel.

The central experience is **Magma Home**: a unified game library presented as a console dashboard. A user selects a game tile and Magma should take care of selecting and preparing the appropriate runtime, configuring input and display, launching the game, and returning to Magma Home when the game exits.

The user should not have to manually open a different emulator or compatibility application for every game. Advanced options may be available in Magma's own settings, but the default experience should be straightforward and controller-first.

Magma should support three primary game categories:

1. **Windows PC games** — launched through a Magma Windows-compatibility environment.
2. **Console games** — launched through platform-specific emulators.
3. **Steam games** — discovered and launched through a future Steam integration.

Magma should also provide a route for software and games built specifically for Magma.

### 1.1 Product principles

- Console-like simplicity, with PC-level flexibility.
- A custom kernel rather than a Linux distribution with a new desktop.
- Clear boundaries between kernel, system services, user interface, compatibility runtimes, and games.
- Hardware-independent APIs wherever practical.
- Controller-first interaction, while retaining keyboard and mouse support.
- Incremental engineering, measurable milestones, and repeatable tests.
- Honest compatibility reporting: a game is not considered supported until it has actually been tested.
- No unnecessary feature expansion beyond the active, human-approved milestone.

## 2. Important technical distinctions

The following distinctions are fundamental to the project.

### 2.1 CPU architecture is not the same as operating-system compatibility

Magma targets x86-64. Most modern Windows PC games built for x86-64 already contain x86-64 machine instructions. An x86-64 Magma CPU therefore does not normally need to translate those instructions merely because the executable was built for Windows.

However, a Windows executable expects Windows-specific services and conventions, including executable loading, process and memory behaviour, system calls, Win32 APIs, files, input, audio, graphics APIs, and sometimes DRM or anti-cheat components.

Therefore, running a Windows game on Magma is primarily an **operating-system and API compatibility problem**, not simply a CPU instruction conversion problem.

### 2.2 A compiler does not automatically turn a Windows game into a native Magma game

A compiler normally translates source code into machine code for a target architecture and ABI. A game that has already been compiled for Windows is not source code waiting to be recompiled. It is an executable containing machine code and references to Windows-specific interfaces.

Magma cannot make that executable natively compatible merely by running a compiler over it.

There are two different paths:

- **Windows compatibility path:** run the Windows executable in a compatibility environment that implements the Windows interfaces the game needs.
- **Native Magma path:** build or port the engine's platform layer to Magma's own ABI and APIs, then compile the engine and game for the Magma target.

Because both Windows and Magma target x86-64, native Magma software can use the same CPU instruction set. The difference is the operating-system ABI, runtime, system calls, and platform APIs.

### 2.3 The Magma Translation Engine is a runtime framework, not a universal compiler

The planned **Magma Translation Engine (MTE)** is a coordinating framework. It should inspect a game, select a suitable backend, prepare the required runtime, manage caches and profiles, and report compatibility.

It may coordinate several separate technologies:

- Executable inspection and format handling.
- CPU instruction translation when the guest CPU architecture differs from x86-64.
- Windows API compatibility.
- Graphics API translation.
- Audio and input adaptation.
- Console hardware emulation.
- Per-game runtime profiles and translation caches.

These are distinct systems. Do not combine them into one fictional universal compiler.

### 2.4 Console games require console emulation

A console game expects the hardware and software environment of its original console. Emulating it normally requires modelling or reproducing the console's CPU, memory, graphics, audio, input, storage, timing, and operating-system behaviour.

A console game cannot generally be converted into a Magma-native game by a simple install-time compilation step.

## 3. System architecture

Magma is planned as a **modular monolithic kernel** with user-space services and applications above it.

```text
+----------------------------------------------------------------+
|                            MAGMA HOME                           |
|           Console dashboard, game tiles, library, settings     |
+----------------------------------------------------------------+
|                           USER SPACE                            |
|                                                                |
|  Game Manager | Library Manager | Input Manager | Display      |
|  Audio Manager | Network Services | Settings | Other Services   |
|                                                                |
|  Magma Translation Engine (MTE)                                 |
|   - Windows compatibility backend                               |
|   - Console emulator backends                                   |
|   - Runtime selection and profile management                    |
|                                                                |
|  Native Magma applications and games                            |
+----------------------------------------------------------------+
|                       MAGMA SYSTEM ABI                          |
|            System calls, IPC, handles, user/kernel boundary     |
+----------------------------------------------------------------+
|                         MAGMA KERNEL                            |
|                                                                |
|  Scheduler | Processes | Memory | Interrupts | VFS              |
|  Device discovery | Drivers | Timers | Security foundations     |
+----------------------------------------------------------------+
|                    HARDWARE / PLATFORM LAYER                    |
+----------------------------------------------------------------+
| CPU | RAM | GPU | Display | Audio | USB | Storage | Network      |
+----------------------------------------------------------------+
```

### 3.1 Kernel model

The initial kernel design is modular monolithic:

- Core kernel facilities such as scheduling, memory management, interrupt handling, and essential drivers initially run in kernel space.
- Subsystems have explicit module boundaries and interfaces.
- Services that can safely run in user space should move there when the process, memory-protection, and IPC foundations are mature.
- A future user-space driver framework may be considered, but it is not a prerequisite for the first bootable kernel.

The project is committed to a custom kernel. Replacing it with Linux, a Linux distribution, or an existing desktop OS is an architectural change and requires the human owner's explicit approval.

### 3.2 Boot architecture

- Target x86-64 UEFI systems.
- Use Limine as the initial bootloader and boot protocol.
- Limine loads the kernel and provides boot information; it is not the Magma kernel.
- Early milestones use the bootloader-provided memory map and framebuffer.
- Magma gradually takes ownership of its own memory management, interrupt configuration, and device initialization.
- Do not write a custom bootloader during the initial kernel milestone.

### 3.3 Core kernel subsystems

Planned kernel responsibilities include:

- CPU and architecture initialization.
- GDT, IDT, exception handling, and interrupt routing.
- Physical and virtual memory management.
- Kernel heap allocation.
- DMA-safe memory allocation.
- Timer infrastructure and scheduling.
- Processes, threads, and context switching.
- System-call boundary and user/kernel transitions.
- Inter-process communication (IPC).
- Virtual filesystem (VFS) and storage interfaces.
- ACPI and PCI device discovery.
- Driver framework and device lifecycle.
- Security and privilege boundaries.

These are design targets, not an instruction to implement them all at once.

## 4. Hardware and I/O architecture

The operating system must separate application-facing APIs from the details of individual hardware devices.

A game should request a controller, audio stream, display mode, or network connection through a Magma service or API. It should not need to know whether a particular laptop uses Intel, NVIDIA, AMD, USB, Bluetooth, or a proprietary wireless receiver.

### 4.1 Graphics and display

The initial kernel milestone uses the framebuffer provided by Limine. This is sufficient for early text and simple pixel output, but it is **not** a complete accelerated gaming graphics stack.

The longer-term graphics system will need:

- Display discovery and display-mode management.
- Framebuffer and display-output management.
- GPU device discovery and driver interfaces.
- GPU memory and buffer management.
- Command submission and synchronization.
- Rendering and presentation interfaces.
- A compositor or display manager.
- Multiple display and resolution handling where supported.
- Graphics API support or translation for games and emulators.

Hardware acceleration requires appropriate GPU drivers. Intel, NVIDIA, and AMD devices have different hardware and driver requirements. Do not claim that one generic driver supports all vendors.

### 4.2 Audio

The planned Magma Audio Engine (MAE) will provide a common audio interface to applications and games.

The audio path is expected to include:

- Kernel-level device and DMA support.
- Audio buffers and interrupt handling.
- A user-space audio service or mixer.
- PCM playback and capture foundations.
- Multiple application streams and mixing.
- Sample-rate and format conversion where needed.
- Stereo and later multichannel output, including 5.1 and 7.1 where hardware supports it.
- Low-latency paths for games.
- Later support for HDMI/DisplayPort audio, USB audio, and Bluetooth audio.

Intel High Definition Audio (Intel HDA) is a possible initial device target, subject to actual hardware inspection.

### 4.3 Input and controllers

Controller-first input is a core Magma requirement.

The input system should ultimately support:

- USB keyboards and mice.
- USB HID game controllers.
- Bluetooth HID controllers.
- Proprietary 2.4 GHz controller receivers where their protocol can be implemented.
- Controller discovery, connection, disconnection, and reconnection.
- Multiple simultaneous controllers.
- Button and axis mapping.
- Configurable dead zones and calibration.
- Trigger values and analogue sticks.
- Controller battery reporting where available.
- Haptics and rumble where supported.

Bluetooth controllers and proprietary wireless receivers are not the same thing. A proprietary receiver may appear as a USB HID device or may need a device-specific protocol and driver.

Applications should consume a normalized controller model, not raw device-specific reports. A conceptual state structure is:

```rust
pub struct GamepadState {
    pub connected: bool,
    pub buttons: u32,
    pub left_stick_x: i16,
    pub left_stick_y: i16,
    pub right_stick_x: i16,
    pub right_stick_y: i16,
    pub left_trigger: u16,
    pub right_trigger: u16,
}
```

This is an illustrative API shape, not an approved final ABI.

### 4.4 Storage

Storage work will require:

- Device discovery.
- Block-device interfaces.
- DMA and interrupt integration as appropriate.
- Partition handling.
- A VFS abstraction.
- Filesystem drivers.
- Safe mounting and unmounting.
- Game-library scanning and metadata management.

FAT-family support may be useful for removable boot media. The final system and game-library filesystem choice remains open and requires a deliberate decision.

### 4.5 Networking and wireless

The planned network stack and services include:

- Ethernet device support.
- Network interface abstractions.
- TCP/IP and UDP foundations.
- DHCP and DNS clients.
- Wi-Fi device support and firmware handling.
- WPA2/WPA3 support through an appropriate wireless stack.
- Bluetooth host-controller interface (HCI) support.
- Bluetooth Classic and BLE foundations as needed for controllers and audio.
- Connection management and user-facing network settings.

Wi-Fi and Bluetooth support are hardware- and firmware-dependent. They will not appear automatically when a network stack is added.

## 5. Magma Home and user-space system

Magma Home is the central user-facing application. The project owner designs the visual interface.

The rest of the system must provide stable interfaces for Home to use:

- **Game Manager:** resolves a selected game to the right executable and runtime, launches it, tracks its process/session, and handles exit.
- **Library Manager:** scans configured game locations, stores metadata and artwork references, and presents installed games to Home.
- **Input Manager:** presents consistent controller, keyboard, and mouse state.
- **Display Manager:** handles display mode transitions and returns the system to Home after a game exits.
- **Audio Manager:** manages output devices, audio streams, and system/game audio policy.
- **Network Services:** expose connectivity to system applications and games through approved APIs.
- **Settings:** centralizes controller, display, audio, network, storage, and runtime options.

The game-launch lifecycle should eventually look like this:

1. The user selects a game tile.
2. Home asks the Game Manager to launch the selected title.
3. The Game Manager asks MTE which runtime/backend is appropriate.
4. Required services and the selected runtime are prepared.
5. Input, display, and audio are handed to the game session according to policy.
6. The game runs.
7. On exit, Magma cleans up the session and restores Home.

A game session must not be allowed to permanently strand the user on a blank screen. Recovery and failure handling are design requirements.

## 6. Magma Translation Engine (MTE)

MTE is the runtime selection and compatibility framework.

### 6.1 Planned components

- **Binary Inspector:** identifies executable formats, architecture, imports, metadata, and likely runtime requirements.
- **Runtime Manager:** selects, prepares, starts, monitors, and stops a backend.
- **Windows Compatibility Backend:** supplies the Windows interfaces implemented by Magma's chosen compatibility approach.
- **Graphics Translation Backend:** maps supported game graphics interfaces to the Magma graphics stack.
- **Audio/Input Adapters:** maps supported Windows or emulator input/audio interfaces to Magma services.
- **Instruction Translation Backend:** added only where a guest instruction set differs from the host CPU architecture or where a genuine translation requirement exists.
- **Emulator Framework:** common lifecycle and integration facilities for console-specific emulator implementations.
- **Translation/Shader Cache:** stores generated artefacts where a backend genuinely needs them.
- **Game Profiles:** records per-game backend selection, launch options, compatibility status, and known workarounds.

MTE should be extensible through documented backend interfaces. Do not implement all backends before the kernel and system APIs exist.

### 6.2 Windows PC game compatibility

Windows game execution may require:

- PE executable inspection and loading.
- A compatible process and memory model.
- A Windows API compatibility layer.
- A supported subset of Win32 and related runtime interfaces.
- Threading, synchronization, file, registry/configuration, and timing behaviour required by target games.
- Input and controller translation.
- Audio API adaptation.
- Graphics API translation.
- Runtime and per-game configuration.
- Compatibility testing.

Wine/Proton are examples of compatibility technology, not simple compilers. Proton and the surrounding Steam stack rely heavily on Linux user-space and ABI facilities. They cannot simply be copied onto a new custom kernel and expected to work. Reusing or porting such technology would require a substantial compatibility strategy and a separate technical review.

Some games may fail because of DRM, anti-cheat, launchers, undocumented APIs, kernel-level components, or unsupported graphics features. Magma must report these limitations honestly.

### 6.3 Console emulation

The emulator framework should allow each console implementation to have its own hardware model and compatibility status.

The project has discussed interest in:

- PlayStation 1.
- PlayStation 2.
- Nintendo 64.
- Nintendo Switch.
- Original Xbox.
- Xbox 360.

These systems differ greatly in complexity. PS1 and Nintendo 64 are more approachable first emulator projects than modern consoles. PS2, Switch, and Xbox 360 require substantially more complex hardware and software emulation. This is a project-risk ranking, not a promise that any platform will be fully supported.

Each emulator may need to implement or integrate:

- CPU execution.
- Memory and bus behaviour.
- Graphics processor or graphics command interpretation.
- Audio processing.
- Controller input.
- Timers and interrupts.
- Storage and media access.
- Firmware or BIOS interfaces where applicable.
- Save states, memory cards, and configuration as appropriate.

Only legally obtained game images and firmware/BIOS material should be used. Do not place copyrighted ROMs, game images, or firmware in the Magma source repository.

## 7. Native Magma software and the custom game engine

The project owner is also building a game engine. Native Magma support is a first-class long-term goal.

The recommended direction is to keep the engine's **core** separate from its **platform layer**.

### 7.1 Suggested engine architecture

```text
Game Engine
|
+-- Core
|   +-- Scene / entity systems
|   +-- Resource and asset management
|   +-- Physics
|   +-- Animation
|   +-- Gameplay systems
|   +-- Engine utilities
|
+-- Platform layer
|   +-- Windows backend
|   +-- Magma backend (future native target)
|
+-- Rendering abstraction
|   +-- Windows graphics backend
|   +-- Magma graphics backend (future)
|
+-- Audio abstraction
+-- Input abstraction
+-- File / storage abstraction
+-- Networking abstraction
+-- Build and packaging tools
```

The exact language, renderer, and engine architecture are decisions for the engine's own design. Do not silently impose a language or graphics API.

### 7.2 Two valid deployment routes

**Route A — Windows build on Magma**

The engine produces a Windows executable. Magma launches it through the Windows compatibility backend. This can be a useful early route, but it is not native Magma execution.

**Route B — Native Magma build**

The engine gains a Magma platform backend and targets Magma's documented ABI, system calls, graphics, input, audio, filesystem, and other APIs. A compatible compiler and linker target then produce a Magma-native executable.

The native route should be developed after Magma has stable user-space and application interfaces. A portable engine core makes the later port easier.

## 8. Phased development roadmap

The project is divided into the following phases. A phase is not considered complete merely because code exists; its exit criteria must be demonstrated.

### Phase 0 — Project preparation

**Purpose:** establish a safe and reproducible development workspace before kernel implementation.

Work includes:

- Create the MagmaOS repository and agreed top-level structure.
- Record toolchain and hardware inventory.
- Confirm Rust toolchain and target configuration.
- Install and verify QEMU and debugging tools.
- Establish build instructions and a repeatable boot-image workflow.
- Establish Git usage and baseline repository hygiene.
- Keep design decisions and implementation status documented.

**Exit criteria:**

- A clean repository exists.
- The documented build environment is reproducible.
- QEMU can be launched and inspected.
- The primary and secondary test laptops have verified CPU, RAM, GPU, firmware, and network-device identities.

**Important hardware note:** the secondary laptop was described as having a 4th-generation Intel Core i5 and Intel HD 520. Those labels may not agree. Verify the exact CPU and GPU IDs before selecting drivers. The primary laptop is reported as Intel Core i3-7020U, Intel HD 620, and 10 GB DDR4 RAM; verify those values in the actual machine as well.

### Phase 1 — Magma Kernel

**Purpose:** build the first bootable custom Rust kernel and establish the foundations required by every later subsystem.

#### 1A. Boot and kernel entry

- UEFI boot path through Limine.
- Rust `no_std` kernel entry.
- Panic handler and basic diagnostics.
- Serial logging for QEMU/debugging where configured.
- Validate bootloader information.
- Read and validate the memory map.
- Obtain and validate framebuffer information.
- Display a first kernel status message.

**First milestone:** `MAGMA OS KERNEL INITIALIZED`.

#### 1B. CPU and exception foundations

- x86-64 CPU initialization.
- GDT and IDT setup.
- Exception handlers and diagnostic output.
- Interrupt-controller strategy.
- Timer foundation.

#### 1C. Memory management

- Physical memory manager.
- Virtual memory and paging.
- Kernel heap.
- Memory-map ownership and reservation rules.
- DMA-capable allocation design.
- Memory safety and allocator tests.

#### 1D. Device discovery and kernel services

- ACPI parsing foundation.
- PCI enumeration.
- Device identity and driver matching model.
- Interrupt routing and APIC support as required.
- Block-device and VFS foundations.
- System-call design.
- IPC foundations.

#### 1E. Execution and basic input

- Threads and scheduler.
- Context switching.
- Process/address-space foundations.
- User-mode transition when the preceding foundations are ready.
- Initial keyboard and basic USB HID work, in a deliberately limited scope.

#### Phase 1 exit criteria

- Magma boots reproducibly in QEMU from the agreed boot path.
- The kernel validates and records boot information and the memory map.
- The kernel writes to the framebuffer.
- Exceptions and panic conditions are diagnosable.
- Memory-management foundations have tests or repeatable validation.
- The implemented scheduler/process facilities meet the explicitly defined kernel milestone.
- Hardware support claims are limited to devices actually tested.

**Scope warning:** this phase is intentionally large. It must be divided into small approved milestones. Do not attempt to implement every driver, all USB, the full network stack, and GPU acceleration in one kernel milestone.

### Phase 2 — GUI and Magma Home

**Purpose:** create the first usable console-style Magma experience.

Work includes:

- Display and framebuffer interface suitable for the first UI.
- A rendering path for the initial Home interface.
- Input-service interface and controller/keyboard navigation model.
- Magma Home application and UI structure.
- Game-library tile model and metadata.
- Settings interface foundations.
- Game Manager service interface.
- Application launch and exit lifecycle.
- Recovery path to restore Home after an application exits or fails.
- Initial display-mode ownership policy.

The project owner is responsible for the visual design direction of Magma Home. System architecture should expose stable services without taking over the owner's UI design.

**Initial limitation:** Home may use a simple framebuffer/software-rendered path. A polished, GPU-accelerated interface depends on later graphics-driver work.

**Phase 2 exit criteria:**

- Magma can boot into Home rather than only a kernel diagnostic screen.
- The interface can be navigated with the implemented input device(s).
- A test application can be selected, launched, exited, and return to Home.
- Failures have a defined recovery route.
- UI code does not depend directly on one GPU vendor.

### Phase 3 — Emulation and compatibility foundations

**Purpose:** build MTE's framework and prove the architecture with a narrowly scoped first backend.

Work includes:

- Binary and game metadata inspection.
- Runtime/backend registration and selection.
- Game profiles and compatibility status.
- Runtime launch/exit lifecycle integration with Game Manager.
- Emulator interface and lifecycle abstraction.
- A first small, clearly bounded emulator target selected by the human owner.
- Windows executable compatibility research and a staged architecture proposal.
- A graphics/audio/input adaptation design.
- Compatibility test harness and per-game reporting.

The first emulator must be chosen deliberately. PS1 or Nintendo 64 is a more realistic initial in-house emulator target than Switch or Xbox 360.

**Phase 3 does not promise:** universal Windows compatibility, a fully working Proton/Steam stack, or complete emulation of every listed console.

**Phase 3 exit criteria:**

- MTE can identify a test title and select a registered backend.
- A demonstrator backend can be launched and stopped through the Game Manager.
- The selected first emulator can run a defined test workload, with limitations documented.
- Compatibility results are recorded rather than guessed.
- Windows compatibility has an approved staged implementation plan based on real API and runtime requirements.

### Phase 4 — Hardware I/O and gaming performance

**Purpose:** mature the hardware services needed for a practical gaming system.

Work includes:

- USB host-controller support and robust HID handling.
- More complete keyboard, mouse, and gamepad support.
- Bluetooth host/controller stack and Bluetooth HID.
- Investigation of proprietary 2.4 GHz controller receivers.
- Controller mapping, calibration, reconnect, battery, and haptics features where supported.
- Audio device framework and initial hardware driver.
- Audio mixer and low-latency game audio.
- GPU driver and accelerated graphics strategy.
- Display management, buffer lifecycle, and presentation.
- Ethernet and network stack.
- Wi-Fi drivers, firmware, and connection management.
- Bluetooth audio where supported.
- Performance measurement, tracing, and latency testing.

Hardware work must be driven by verified hardware IDs and realistic driver feasibility. A driver for one device does not imply support for every device using the same brand name.

**Phase 4 exit criteria:**

- Defined target devices work and have repeatable tests.
- Controller input is stable across connection and disconnection.
- Audio playback is functional on a documented target device.
- The chosen GPU path can present frames reliably on a documented target device.
- Networking is functional on documented target adapters.
- Performance and compatibility claims are backed by measurements.

### Phase 5 — Native Magma SDK and game engine

**Purpose:** let developers build software that targets Magma directly.

Work includes:

- Public Magma application ABI and SDK design.
- Stable system-call and user-space library interfaces.
- Native executable format/loader contract.
- Compiler and linker target strategy.
- Graphics, input, audio, filesystem, and timing APIs for native applications.
- Packaging and application installation format.
- Engine platform abstraction.
- Native Magma engine backend.
- Native engine build and test examples.
- Documentation and sample applications.

The Windows engine build may continue to work through the Windows compatibility route. The native route is a separate target and must not be described as a Windows executable being magically recompiled at install time.

**Phase 5 exit criteria:**

- A small native Magma application can be compiled, packaged, installed, launched, and exited.
- The engine can build and run a controlled sample using the Magma backend.
- The public APIs are documented and versioned.
- The Windows-targeted engine path and native Magma path are clearly distinguished.

### Phase 6 — Steam, broader compatibility, and product readiness

**Purpose:** expand the library, runtime coverage, and reliability after the underlying system is mature.

Work may include:

- Steam integration investigation and implementation.
- Steam authentication, library, launch, overlay, update, and DRM constraints review.
- Further Windows API coverage.
- Graphics compatibility improvements and per-title profiles.
- Additional console emulators, selected by feasibility and priority.
- Save management, cloud-save integration where supported, and game updates.
- Installer, recovery environment, system update, and rollback strategy.
- Security review, crash reporting, logging, and performance tuning.
- Hardware compatibility matrix and release criteria.

Steam/Proton integration must not be assumed to work simply because Magma is x86-64. It depends on the relevant user-space, ABI, graphics, audio, network, and runtime facilities.

**Phase 6 exit criteria:**

- Supported titles have been tested and documented.
- Installation, update, recovery, and rollback paths are defined and tested.
- Hardware support matrix is published.
- Known incompatibilities and limitations are disclosed.
- A release candidate passes the project's agreed reliability and security checks.

---

## 9. Development environment and test strategy

### 9.1 Initial development setup

The intended development host is Windows 11. The initial toolchain is expected to include:

- Rust toolchain and Cargo.
- Required Rust components and the appropriate x86-64 freestanding target/toolchain configuration.
- Limine boot files and configuration.
- QEMU for virtual hardware testing.
- GDB or an equivalent debugger workflow.
- Git.
- Any assembly/linker utilities required by the verified build approach.

Exact tool versions and build commands must be recorded after the project owner selects and verifies the toolchain. Do not assume that a tutorial, template, or old command is current.

### 9.2 Test progression

1. Fast iteration in QEMU.
2. Debuggable virtual hardware and deterministic test cases.
3. Primary physical laptop.
4. Secondary physical laptop after hardware inventory is verified.
5. Additional hardware only when available and specifically included in the test matrix.

The primary laptop is reported as an Intel Core i3-7020U system with Intel HD 620 and 10 GB DDR4 RAM. QEMU should initially be configured conservatively, for example with 2 virtual CPUs and 2 GB of guest memory, then adjusted based on actual host load.

### 9.3 Test discipline

- Every milestone has an explicit success condition.
- Test the smallest useful vertical slice before broadening scope.
- Keep known-good boot images or commits.
- Record the exact machine, firmware, QEMU configuration, and toolchain used for a test.
- Do not report a test as passed unless it was actually run and its result observed.
- A successful QEMU boot is not proof of physical-hardware compatibility.
- A successful boot on one laptop is not proof of support for all x86-64 machines.

## 10. Proposed repository structure

This is a proposed structure, not a requirement to create every directory immediately. Create files and directories only as an explicitly approved milestone needs them.

```text
MagmaOS/
├── MAGMA_OS.md
├── AGENTS.md
├── boot/
│   └── limine/
├── kernel/
│   ├── Cargo.toml
│   ├── rust-toolchain.toml
│   ├── linker.ld
│   └── src/
│       ├── main.rs
│       ├── panic.rs
│       ├── arch/
│       │   └── x86_64/
│       ├── boot/
│       ├── mm/
│       ├── interrupts/
│       ├── scheduler/
│       ├── process/
│       ├── syscall/
│       ├── ipc/
│       ├── vfs/
│       ├── drivers/
│       └── io/
├── system/
│   ├── game-manager/
│   ├── input-manager/
│   ├── display-manager/
│   ├── audio-manager/
│   ├── network-manager/
│   └── library-manager/
├── home/
│   └── magma-home/
├── mte/
│   ├── core/
│   ├── windows-compat/
│   └── emulators/
├── sdk/
├── engine/
├── tools/
├── tests/
└── docs/
```

The tree may evolve after an approved architecture review. Avoid empty scaffolding and speculative modules.

## 11. Open decisions requiring the human owner's approval

The following are not yet settled and must not be silently decided by an automated coding agent:

- Exact Rust toolchain channel/version and kernel build approach.
- Final Limine version and boot protocol details.
- Kernel/user-space ABI and system-call design.
- Filesystem choice for the main installation and game library.
- User-space executable format and loader contract.
- Graphics API and GPU-driver strategy.
- Audio architecture and initial device target.
- Exact first USB host-controller and gamepad hardware targets.
- Wi-Fi and Bluetooth chipsets to support first.
- First console emulator to implement.
- Windows compatibility strategy and scope.
- Whether and how to reuse or port third-party compatibility/emulation projects.
- Steam integration approach.
- The game engine's language, renderer, and native Magma backend design.
- Installer, update, recovery, and security architecture.

## 12. Project status vocabulary

Use these labels consistently:

- **Planned:** described as a future goal.
- **Proposed:** a candidate design that still needs approval.
- **Approved:** explicitly accepted by the human owner.
- **In progress:** implementation has begun under an authorized task.
- **Implemented:** code exists; this does not necessarily mean it works.
- **Verified:** the specified test was actually run and passed.
- **Blocked:** progress depends on an unresolved technical or human decision.
- **Deferred:** deliberately postponed.

Never turn a planned or proposed feature into a claim that it already exists.
