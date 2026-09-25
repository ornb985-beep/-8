// Apex-AOSP hypervisor self test.
//
// A bare-metal AArch64 guest disguised as a Linux `Image` (so the normal
// boot path loads it). It exercises every critical VMM path on real
// hardware without needing an Android build:
//   1. MMIO stores to the PL011 UART        (data abort with ISV)
//   2. MMIO load of the virtio-mmio magic   (load + register write-back)
//   3. GICv3 distributor/redistributor/ICC   (in-kernel vGIC or userspace model)
//   4. virtual timer interrupt through the GIC while the vCPU sits in WFI
//   5. PSCI CPU_ON of vCPU 1 and PSCI SYSTEM_OFF from the secondary
// Expected console output: "APEX selftest: MGT2\n".
// Rebuild: scripts/gen-selftest.sh

    .text
    .global _head
_head:
    b       _start                  // code0: entry point
    .word   0                       // code1
    .quad   0                       // text_offset
    .quad   0x10000                 // image_size
    .quad   0xa                     // flags: LE, 4K pages, place anywhere
    .quad   0, 0, 0
    .word   0x644d5241              // "ARM\x64"
    .word   0

_start:
    ldr     x19, =0x09000000        // PL011 data register
    mov     x20, #0
    adr     x1, banner
    bl      puts

    // 2. virtio-mmio slot 0 must answer with "virt"
    ldr     x2, =0x0a000000
    ldr     w3, [x2]
    ldr     w4, =0x74726976
    cmp     w3, w4
    b.ne    fail
    mov     w0, #'M'
    bl      putc

    // 3. exception vectors + GICv3
    adr     x0, vectors
    msr     vbar_el1, x0
    isb
    ldr     x1, =0x08000000         // GICD
    mov     w2, #0x12               // ARE | EnableGrp1
    str     w2, [x1]
    ldr     x1, =0x080a0000         // GICR of vCPU 0 (RD_base)
    str     wzr, [x1, #0x14]        // GICR_WAKER.ProcessorSleep = 0
1:  ldr     w2, [x1, #0x14]
    tbnz    w2, #2, 1b              // wait for ChildrenAsleep == 0
    add     x1, x1, #0x10, lsl #12  // SGI_base
    mov     w2, #(1 << 27)
    str     w2, [x1, #0x80]         // IGROUPR0: INTID 27 (vtimer) -> group 1
    mov     w2, #0x80
    strb    w2, [x1, #0x41b]        // IPRIORITYR byte 27 = 0x80
    mov     w2, #(1 << 27)
    str     w2, [x1, #0x100]        // ISENABLER0
    mov     x2, #0xf0
    msr     S3_0_C4_C6_0, x2        // ICC_PMR_EL1
    mov     x2, #1
    msr     S3_0_C12_C12_7, x2      // ICC_IGRPEN1_EL1
    isb
    mov     w0, #'G'
    bl      putc

    // 4. virtual timer in 1 ms, taken as an IRQ out of WFI
    mrs     x2, cntfrq_el0
    mov     x3, #1000
    udiv    x2, x2, x3
    msr     cntv_tval_el0, x2
    mov     x2, #1                  // ENABLE, not masked
    msr     cntv_ctl_el0, x2
    isb
    msr     daifclr, #2
2:  wfi
    cbz     x20, 2b                 // set by irq_handler
    msr     daifset, #2
    mov     w0, #'T'
    bl      putc

    // 5. bring up vCPU 1; it finishes the test
    ldr     x0, =0xc4000003         // PSCI CPU_ON (SMC64)
    mov     x1, #1                  // target MPIDR
    adr     x2, secondary
    mov     x3, #0
    hvc     #0
    cbnz    x0, fail
3:  wfe
    b       3b

secondary:
    ldr     x19, =0x09000000
    mov     w0, #'2'
    bl      putc
    mov     w0, #'\n'
    bl      putc
    ldr     x0, =0x84000008         // PSCI SYSTEM_OFF: success
    hvc     #0
    b       .

fail:
    ldr     x19, =0x09000000
    mov     w0, #'!'
    bl      putc
    ldr     x0, =0x84000009         // PSCI SYSTEM_RESET: failure
    hvc     #0
    b       .

putc:                               // w0 -> UART
    strb    w0, [x19]
    ret

puts:                               // NUL terminated string at x1
    mov     x9, x30
4:  ldrb    w0, [x1], #1
    cbz     w0, 5f
    bl      putc
    b       4b
5:  mov     x30, x9
    ret

irq_handler:
    mrs     x0, S3_0_C12_C12_0      // ICC_IAR1_EL1
    msr     cntv_ctl_el0, xzr       // stop the timer (drops the PPI)
    msr     S3_0_C12_C12_1, x0      // ICC_EOIR1_EL1
    cmp     x0, #27
    b.ne    fail
    mov     x20, #1
    eret

bad_exception:
    b       fail

banner:
    .asciz  "APEX selftest: "

    .balign 0x800
vectors:                            // VBAR_EL1: 16 entries x 0x80
    .rept 5
    b       bad_exception
    .balign 0x80
    .endr
    b       irq_handler             // +0x280: current EL, SPx, IRQ
    .balign 0x80
    .rept 10
    b       bad_exception
    .balign 0x80
    .endr
    .ltorg
