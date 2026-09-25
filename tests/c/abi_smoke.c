/* Links against libapex_vmm.a and exercises the C ABI without a hypervisor. */
#include "apex.h"

#include <assert.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>

_Static_assert(sizeof(ApexFrame) == 56, "ApexFrame layout");
_Static_assert(offsetof(ApexFrame, seq) == 24, "ApexFrame.seq");
_Static_assert(offsetof(ApexFrame, data) == 40, "ApexFrame.data");
_Static_assert(sizeof(ApexTouch) == 20, "ApexTouch layout");
_Static_assert(sizeof(ApexStats) == 64, "ApexStats layout");

int main(void) {
    printf("apex %s\n", apex_version());
    assert(apex_mac_keycode_to_linux(0x31) == 57); /* space */
    char err[256] = {0};
    ApexVm *vm = apex_vm_create("/definitely/missing.toml", NULL, err, sizeof err);
    assert(vm == NULL);
    assert(strstr(err, "missing.toml") != NULL);
    assert(apex_vm_start(NULL) == APEX_ERR);
    ApexFrame f;
    assert(!apex_display_acquire(NULL, 0, &f));
    char *caps = apex_host_capabilities();
    assert(caps != NULL);
    printf("%s\n", caps);
    apex_string_free(caps);
    puts("ABI smoke test passed");
    return 0;
}
