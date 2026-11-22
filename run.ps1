# QEMU 11.1.0 UEFI + TCG launch script for Magma OS
# Requires: QEMU 11.1.0, OVMF UEFI firmware

 = "qemu-system-x86_64"
 = "OVMF_CODE.UEFI"
 = "OVMF_VARS.UEFI"

# Build the kernel first
cargo build

# Run QEMU with Limine bootloader
&  
    -bios  
    -drive if=pflash,format=raw,readonly=on,file= 
    -drive if=pflash,format=raw,file= 
    -drive format=fat,readonly=on,file=boot 
    -m 512M 
    -cpu qemu64 
    -smp 1 
    -no-reboot 
    -serial stdio 
    -display none
