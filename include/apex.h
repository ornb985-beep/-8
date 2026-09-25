/*
 * Project Apex-AOSP — C ABI of the VMM core (libapex_vmm.a).
 *
 * Threading: all functions are thread safe. apex_vm_wait() blocks and is
 * meant to be called from a background thread. Callbacks may be invoked
 * from VMM worker threads and must not block for long.
 *
 * Memory: frames returned by apex_display_acquire() point into page-aligned
 * swapchain memory owned by the VMM. The pointer stays valid until the
 * matching apex_display_release(). On Apple Silicon wrap it with
 * -[MTLDevice newBufferWithBytesNoCopy:length:options:deallocator:]
 * (length = ApexFrame.len) for zero-copy presentation; re-create the buffer
 * whenever ApexFrame.generation changes for that slot.
 */
#ifndef APEX_H
#define APEX_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct ApexVm ApexVm;

enum {
    APEX_OK = 0,
    APEX_ERR = -1,
    APEX_ERR_PANIC = -2,
};

/* apex_vm_wait() / apex_vm_stop_reason() results */
enum {
    APEX_STOP_NONE = 0,
    APEX_STOP_POWEROFF = 1,
    APEX_STOP_RESET = 2, /* guest rebooted: destroy and create the VM again */
    APEX_STOP_REQUESTED = 3,
    APEX_STOP_ERROR = 4, /* see apex_vm_last_error() */
};

/* ApexFrame.layout: byte order of each 32-bit pixel in memory */
enum {
    APEX_PIXEL_BGRA = 0,
    APEX_PIXEL_RGBA = 1,
    APEX_PIXEL_ARGB = 2,
    APEX_PIXEL_ABGR = 3,
};

typedef void (*ApexLogFn)(void *ctx, int32_t level, const char *message);
typedef void (*ApexBytesFn)(void *ctx, const uint8_t *data, size_t len);

typedef struct ApexHooks {
    ApexBytesFn serial; /* console output when profile has console.serial = "callback" */
    void *serial_ctx;
    ApexBytesFn net_tx; /* frames from the guest when network.mode = "host" */
    void *net_ctx;
} ApexHooks;

typedef struct ApexFrame {
    uint32_t slot;
    uint32_t width;
    uint32_t height;
    uint32_t stride; /* bytes per row, multiple of 256 */
    uint32_t layout; /* APEX_PIXEL_* */
    uint32_t opaque; /* 1: ignore alpha */
    uint64_t seq;
    uint64_t generation;
    const uint8_t *data;
    size_t len; /* multiple of the host page size */
} ApexFrame;

typedef struct ApexTouch {
    uint32_t id; /* stable while the finger is down */
    int32_t x;   /* guest display pixels */
    int32_t y;
    int32_t pressure; /* 0..255 */
    int32_t major;    /* contact size 0..255 */
} ApexTouch;

typedef struct ApexDisplayInfo {
    uint32_t width;
    uint32_t height;
    uint32_t refresh_hz;
    uint32_t dpi;
} ApexDisplayInfo;

typedef struct ApexStats {
    uint64_t frames_submitted;
    uint64_t frames_presented;
    uint64_t vsyncs;
    uint64_t exits_mmio;
    uint64_t exits_sysreg;
    uint64_t exits_wfi;
    uint64_t exits_psci;
    uint64_t exits_vtimer;
} ApexStats;

const char *apex_version(void);
/* level: 1 error .. 5 trace. fn = NULL restores stderr logging. */
void apex_set_log(ApexLogFn fn, void *ctx, int32_t max_level);
char *apex_host_capabilities(void);
void apex_string_free(char *s);

/* Lifecycle. Only one VM may exist per process (Hypervisor.framework). */
ApexVm *apex_vm_create(const char *profile_path, const ApexHooks *hooks, char *err, size_t err_len);
int32_t apex_vm_start(ApexVm *vm);
void apex_vm_request_stop(ApexVm *vm);
int32_t apex_vm_wait(ApexVm *vm);
int32_t apex_vm_stop_reason(ApexVm *vm);
const char *apex_vm_last_error(ApexVm *vm);
void apex_vm_destroy(ApexVm *vm);

/* Display */
void apex_display_info(ApexVm *vm, ApexDisplayInfo *out);
bool apex_display_acquire(ApexVm *vm, uint64_t after_seq, ApexFrame *out);
void apex_display_release(ApexVm *vm, uint32_t slot);
void apex_display_vsync(ApexVm *vm); /* only with display.vsync = "host" */

/* Input */
void apex_touch_frame(ApexVm *vm, const ApexTouch *contacts, uint32_t count);
void apex_key(ApexVm *vm, uint16_t linux_code, bool down);
uint16_t apex_mac_keycode_to_linux(uint16_t mac_virtual_keycode);
void apex_console_input(ApexVm *vm, const uint8_t *data, size_t len);

/* Hardware state */
void apex_battery_set(ApexVm *vm, uint32_t percent, bool charging, bool ac_online);
void apex_net_rx(ApexVm *vm, const uint8_t *frame, size_t len);
void apex_stats(ApexVm *vm, ApexStats *out);

/* Linux key codes for the hardware buttons */
#define APEX_KEY_VOLUMEDOWN 114
#define APEX_KEY_VOLUMEUP 115
#define APEX_KEY_POWER 116
#define APEX_KEY_BACK 158
#define APEX_KEY_HOMEPAGE 172
#define APEX_KEY_APPSELECT 580

#ifdef __cplusplus
}
#endif

#endif /* APEX_H */
