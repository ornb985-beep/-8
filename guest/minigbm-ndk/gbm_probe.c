// apex_gbm_probe: allocate a 1080x2400 scanout buffer through minigbm on the
// guest's virtio-gpu (/dev/dri/card0), write and verify it through a CPU
// mapping, and export it as a dma-buf. Results go to the kernel log.
#include <fcntl.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

#include "gbm.h"

static int kmsg = -1;

static void say(const char *fmt, ...) {
    char buf[512];
    va_list ap;
    va_start(ap, fmt);
    int n = snprintf(buf, sizeof buf, "<5>APEX-GBM: ");
    n += vsnprintf(buf + n, sizeof buf - (size_t)n, fmt, ap);
    va_end(ap);
    if (kmsg >= 0) write(kmsg, buf, (size_t)n);
    fprintf(stderr, "%s\n", buf + 3);
}

int main(int argc, char **argv) {
    const char *node = argc > 1 ? argv[1] : "/dev/dri/card0";
    kmsg = open("/dev/kmsg", O_WRONLY | O_CLOEXEC);
    for (int i = 0; i < 100 && access(node, F_OK) != 0; i++) usleep(100000);
    int fd = open(node, O_RDWR | O_CLOEXEC);
    if (fd < 0) { say("FAIL open %s", node); return 1; }
    struct gbm_device *gbm = gbm_create_device(fd);
    if (!gbm) { say("FAIL gbm_create_device(%s)", node); return 1; }
    say("backend %s on %s", gbm_device_get_backend_name(gbm), node);
    const uint32_t w = 1080, h = 2400;
    struct gbm_bo *bo = gbm_bo_create(gbm, w, h, GBM_FORMAT_ARGB8888,
                                      GBM_BO_USE_SCANOUT | GBM_BO_USE_LINEAR | GBM_BO_USE_SW_READ_OFTEN |
                                          GBM_BO_USE_SW_WRITE_OFTEN);
    if (!bo) { say("FAIL gbm_bo_create %ux%u ARGB8888 scanout", w, h); return 1; }
    uint32_t stride = 0;
    void *map_data = NULL;
    uint32_t *px = gbm_bo_map(bo, 0, 0, w, h, GBM_BO_TRANSFER_READ_WRITE, &stride, &map_data);
    if (!px) { say("FAIL gbm_bo_map"); return 1; }
    for (uint32_t y = 0; y < h; y++)
        for (uint32_t x = 0; x < w; x++) px[y * (stride / 4) + x] = 0xff000000u | ((y << 12) ^ x);
    gbm_bo_unmap(bo, map_data);
    map_data = NULL;
    px = gbm_bo_map(bo, 0, 0, w, h, GBM_BO_TRANSFER_READ, &stride, &map_data);
    uint64_t bad = 0;
    for (uint32_t y = 0; y < h; y++)
        for (uint32_t x = 0; x < w; x++)
            if (px[y * (stride / 4) + x] != (0xff000000u | ((y << 12) ^ x))) bad++;
    gbm_bo_unmap(bo, map_data);
    int dmabuf = gbm_bo_get_fd(bo);
    say("%s: bo %ux%u stride %u modifier 0x%llx dma-buf fd %d, %u px written+verified, %llu errors",
        bad == 0 && dmabuf >= 0 ? "PASS" : "FAIL", w, h, stride,
        (unsigned long long)gbm_bo_get_modifier(bo), dmabuf, w * h, (unsigned long long)bad);
    return bad == 0 && dmabuf >= 0 ? 0 : 1;
}
