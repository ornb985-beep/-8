// Apex VMM 5-second probe: a static, libc-free /init for an initramfs.
// Prints "APEX VMM OK" plus the kernel version to the console, then powers
// the VM off through reboot(2) -> PSCI SYSTEM_OFF, so the VMM exits by itself.
// Rebuild: scripts/build-probe.sh

    .equ SYS_write,     64
    .equ SYS_uname,     160
    .equ SYS_sync,      81
    .equ SYS_reboot,    142
    .equ LINUX_REBOOT_MAGIC1, 0xfee1dead
    .equ LINUX_REBOOT_MAGIC2, 672274793
    .equ LINUX_REBOOT_CMD_POWER_OFF, 0x4321fedc

    .text
    .global _start
_start:
    mov     x0, #1
    adr     x1, msg
    mov     x2, #(msg_end - msg)
    mov     x8, #SYS_write
    svc     #0

    // uname(&utsbuf): print release (offset 130) with a trailing newline
    adr     x0, utsbuf
    mov     x8, #SYS_uname
    svc     #0
    adr     x1, utsbuf + 130
    mov     x2, #0
1:  ldrb    w3, [x1, x2]
    cbz     w3, 2f
    add     x2, x2, #1
    cmp     x2, #64
    b.lt    1b
2:  mov     x0, #1
    mov     x8, #SYS_write
    svc     #0
    mov     x0, #1
    adr     x1, nl
    mov     x2, #1
    mov     x8, #SYS_write
    svc     #0

    mov     x8, #SYS_sync
    svc     #0
    ldr     x0, =LINUX_REBOOT_MAGIC1
    ldr     x1, =LINUX_REBOOT_MAGIC2
    ldr     x2, =LINUX_REBOOT_CMD_POWER_OFF
    mov     x3, #0
    mov     x8, #SYS_reboot
    svc     #0
3:  b       3b                      // not reached

msg:
    .ascii  "\n=====================================\n APEX VMM OK  (probe initramfs, kernel "
msg_end:
nl: .ascii  "\n"
    .ltorg

    .bss
    .balign 8
utsbuf:
    .skip   390
