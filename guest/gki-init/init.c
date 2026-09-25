// First-stage loader for the prebuilt Android GKI kernel (android14-6.1).
//
// GKI builds the virtio drivers as vendor modules, so the kernel cannot
// mount the Apex system disk by itself. This /init (freestanding, raw
// syscalls, no libc) loads /lib/modules/*.ko in the order listed in
// /lib/modules/modules.load, waits for vda2 in sysfs, creates its node
// (GKI has no devtmpfs), mounts it read-only,
// moves it over / and execs the system's /init as PID 1 -- the same state
// the legacy system-as-root boot reaches when the kernel mounts root itself.

typedef unsigned long u64;
typedef long i64;

#define AT_FDCWD -100
#define O_RDONLY 0
#define O_WRONLY 1
#define O_CLOEXEC 02000000
#define MS_RDONLY 1
#define MS_MOVE 8192
#define MNT_DETACH 2

enum {
    SYS_openat = 56, SYS_close = 57, SYS_read = 63, SYS_write = 64, SYS_mkdirat = 34,
    SYS_mount = 40, SYS_umount2 = 39, SYS_chdir = 49, SYS_chroot = 51, SYS_faccessat = 48,
    SYS_nanosleep = 101, SYS_execve = 221, SYS_finit_module = 273, SYS_mknodat = 33,
};

#define S_IFCHR 0020000
#define S_IFBLK 0060000

static i64 sc6(i64 n, i64 a, i64 b, i64 c, i64 d, i64 e, i64 f) {
    register i64 x8 __asm__("x8") = n;
    register i64 x0 __asm__("x0") = a;
    register i64 x1 __asm__("x1") = b;
    register i64 x2 __asm__("x2") = c;
    register i64 x3 __asm__("x3") = d;
    register i64 x4 __asm__("x4") = e;
    register i64 x5 __asm__("x5") = f;
    __asm__ volatile("svc 0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2), "r"(x3), "r"(x4), "r"(x5) : "memory");
    return x0;
}
#define SC(n, a, b, c, d, e) sc6(n, (i64)(a), (i64)(b), (i64)(c), (i64)(d), (i64)(e), 0)

static int kmsg = -1;

static void log3(const char *a, const char *b, const char *c) {
    char buf[256];
    u64 n = 0;
    const char *parts[4] = {"<5>APEX-GKI: ", a, b, c};
    for (int i = 0; i < 4; i++)
        for (const char *p = parts[i]; p && *p && n < sizeof buf - 2; p++) buf[n++] = *p;
    buf[n++] = '\n';
    SC(SYS_write, kmsg >= 0 ? kmsg : 2, buf, n, 0, 0);
}

static void num(i64 v, char *out) {
    char t[24];
    int n = 0, neg = v < 0;
    u64 u = neg ? (u64)-v : (u64)v;
    do t[n++] = (char)('0' + u % 10); while (u /= 10);
    int i = 0;
    if (neg) out[i++] = '-';
    while (n) out[i++] = t[--n];
    out[i] = 0;
}

static void msleep(long ms) {
    long ts[2] = {ms / 1000, (ms % 1000) * 1000000};
    SC(SYS_nanosleep, ts, 0, 0, 0, 0);
}

static void die(const char *why) {
    log3("FATAL: ", why, "");
    for (;;) msleep(1000);
}

static void load_modules(void) {
    static char list[8192];
    int fd = (int)SC(SYS_openat, AT_FDCWD, "/lib/modules/modules.load", O_RDONLY | O_CLOEXEC, 0, 0);
    if (fd < 0) die("no /lib/modules/modules.load");
    i64 n = SC(SYS_read, fd, list, sizeof list - 1, 0, 0);
    SC(SYS_close, fd, 0, 0, 0, 0);
    if (n <= 0) die("empty modules.load");
    list[n] = 0;
    char path[160], err[24];
    for (char *p = list; *p;) {
        char *e = p;
        while (*e && *e != '\n') e++;
        char save = *e;
        *e = 0;
        if (*p) {
            u64 i = 0;
            for (const char *s = "/lib/modules/"; *s;) path[i++] = *s++;
            for (const char *s = p; *s && i < sizeof path - 1;) path[i++] = *s++;
            path[i] = 0;
            int mfd = (int)SC(SYS_openat, AT_FDCWD, path, O_RDONLY | O_CLOEXEC, 0, 0);
            i64 r = mfd < 0 ? mfd : SC(SYS_finit_module, mfd, "", 0, 0, 0);
            if (mfd >= 0) SC(SYS_close, mfd, 0, 0, 0, 0);
            num(r, err);
            log3(r == 0 ? "loaded " : "FAILED to load ", p, r == 0 ? "" : err);
        }
        *e = save;
        p = *e ? e + 1 : e;
    }
}

static u64 mkdev(u64 major, u64 minor) {
    return (minor & 0xff) | (major << 8) | ((minor & ~0xffUL) << 12);
}

// Block device node from sysfs ("MAJ:MIN\n"): GKI has no devtmpfs, Android's
// init creates /dev itself, so the loader does the same for the root disk.
static int mknod_from_sysfs(const char *sysdev, const char *node) {
    char buf[32];
    int fd = (int)SC(SYS_openat, AT_FDCWD, sysdev, O_RDONLY | O_CLOEXEC, 0, 0);
    if (fd < 0) return -1;
    i64 n = SC(SYS_read, fd, buf, sizeof buf - 1, 0, 0);
    SC(SYS_close, fd, 0, 0, 0, 0);
    if (n <= 0) return -1;
    u64 maj = 0, min = 0, *v = &maj;
    for (i64 i = 0; i < n; i++) {
        if (buf[i] == ':') v = &min;
        else if (buf[i] >= '0' && buf[i] <= '9') *v = *v * 10 + (u64)(buf[i] - '0');
    }
    return (int)SC(SYS_mknodat, AT_FDCWD, node, S_IFBLK | 0600, mkdev(maj, min), 0);
}

void _start_c(void) {
    SC(SYS_mount, "tmpfs", "/dev", "tmpfs", 0, "mode=0755");
    SC(SYS_mount, "sysfs", "/sys", "sysfs", 0, 0);
    SC(SYS_mknodat, AT_FDCWD, "/dev/kmsg", S_IFCHR | 0600, mkdev(1, 11), 0);
    kmsg = (int)SC(SYS_openat, AT_FDCWD, "/dev/kmsg", O_WRONLY | O_CLOEXEC, 0, 0);
    log3("loading virtio modules for GKI", "", "");
    load_modules();

    int waited = 0;
    while (mknod_from_sysfs("/sys/class/block/vda2/dev", "/dev/vda2") != 0) {
        if (waited++ > 1000) die("/sys/class/block/vda2 did not appear within 10 s");
        msleep(10);
    }
    SC(SYS_mkdirat, AT_FDCWD, "/newroot", 0755, 0, 0);
    i64 r = SC(SYS_mount, "/dev/vda2", "/newroot", "ext4", MS_RDONLY, 0);
    if (r != 0) die("mount /dev/vda2 (ext4, ro) failed");
    SC(SYS_umount2, "/dev", MNT_DETACH, 0, 0, 0);
    SC(SYS_umount2, "/sys", MNT_DETACH, 0, 0, 0);
    if (SC(SYS_chdir, "/newroot", 0, 0, 0, 0) != 0) die("chdir /newroot");
    if (SC(SYS_mount, ".", "/", 0, MS_MOVE, 0) != 0) die("move root");
    if (SC(SYS_chroot, ".", 0, 0, 0, 0) != 0) die("chroot");
    SC(SYS_chdir, "/", 0, 0, 0, 0);
    log3("root /dev/vda2 mounted, exec /init", "", "");
    if (kmsg >= 0) SC(SYS_close, kmsg, 0, 0, 0, 0);
    static char *const argv[] = {"/init", 0};
    static char *const envp[] = {0};
    SC(SYS_execve, "/init", argv, envp, 0, 0);
    die("execve /init failed");
}

__asm__(".globl _start\n_start:\n  mov x29, #0\n  bl _start_c\n1: b 1b\n");
