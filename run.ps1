$ErrorActionPreference = "Stop"

cargo build
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Build GPT disk image with 3 partitions
$diskImage = "target\magma_disk.img"
Write-Host "Building GPT disk image..."
python tools\build_gpt_disk.py "$diskImage"
if ($LASTEXITCODE -ne 0) {
    Write-Error "Failed to build disk image"
    exit 1
}

$codeFd = "C:\Program Files\qemu\share\edk2-x86_64-code.fd"
$varsTemplate = "C:\Program Files\qemu\share\edk2-i386-vars.fd"
$varsFd = "target\edk2-vars.fd"
if (-not (Test-Path $varsFd)) {
    Copy-Item $varsTemplate $varsFd
}

& "C:\Program Files\qemu\qemu-system-x86_64.exe" `
    -accel tcg `
    -m 256M `
    -no-reboot `
    -d int,cpu_reset -D qemu_fault.log `
    -drive if=pflash,format=raw,readonly=on,file="$codeFd" `
    -drive if=pflash,format=raw,file="$varsFd" `
    -drive format=raw,file="$diskImage" `
    -serial stdio