/* Faults target only synthetic paths explicitly enabled by process tests. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int rename(const char *from, const char *to) {
    int (*real_rename)(const char *, const char *) = dlsym(RTLD_NEXT, "rename");
    const char *mode = getenv("BRAIN_TEST_BACKUP_FAULT");
    if (mode && !strcmp(mode, "stage-failure") &&
        (strstr(to, "sessions.sqlite.was") ||
         (strstr(to, ".brain-restore-") && strstr(to, "/archive/sessions.sqlite")))) {
        errno = EIO;
        return -1;
    }
    int result = real_rename(from, to);
    if (mode && result == 0) {
        if (!strcmp(mode, "crash-before") && strstr(to, "brain.sqlite.was")) _exit(99);
        if (!strcmp(mode, "crash-after") && !strstr(to, ".was") &&
            strstr(from, "/brain-20") && strstr(to, "/.brain/brain.sqlite")) _exit(99);
    }
    return result;
}

int renameat2(int afd, const char *from, int bfd, const char *to, unsigned flags) {
    int (*real_exchange)(int, const char *, int, const char *, unsigned) = dlsym(RTLD_NEXT, "renameat2");
    const char *mode = getenv("BRAIN_TEST_BACKUP_FAULT");
    if (mode && !strcmp(mode, "crash-before")) _exit(99);
    if (mode && !strcmp(mode, "exchange-failure")) { errno = EIO; return -1; }
    int result = real_exchange(afd, from, bfd, to, flags);
    if (result == 0 && mode && !strcmp(mode, "crash-after")) _exit(99);
    return result;
}
