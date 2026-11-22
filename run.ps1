$ErrorActionPreference = "Stop"

cargo build

$esp = "target\esp"
New-Item -ItemType Directory -Force -Path "$esp\EFI\BOOT", "$esp\boot" | Out-Null

Copy-Item "boot\limine\BOOTX64.EFI" "$esp\EFI\BOOT\BOOTX64.EFI" -Force
Copy-Item "boot\limine.conf" "$esp\boot\limine.conf" -Force
Copy-Item "target\x86_64-unknown-none\debug\magma_os" "$esp\boot\magma_os" -Force

$codeFd = "C:\Program Files\qemu\share\edk2-x86_64-code.fd"
$varsTemplate = "C:\Program Files\qemu\share\edk2-i386-vars.fd"
$varsFd = "target\edk2-vars.fd"
if (-not (Test-Path $varsFd)) {
    Copy-Item $varsTemplate $varsFd
}

& "C:\Program Files\qemu\qemu-system-x86_64.exe" `
    -accel tcg `
    -m 256M `
    -drive if=pflash,format=raw,readonly=on,file="$codeFd" `
    -drive if=pflash,format=raw,file="$varsFd" `
    -drive format=raw,file=fat:rw:$esp `
    -serial stdio