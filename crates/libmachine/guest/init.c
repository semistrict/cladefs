// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// libmachine's test init: prints a marker with the kernel's uptime and the
// RAM it sees (MemTotal from /proc/meminfo), then powers the machine off; or, by the kernel command line's
// libmachine_action (the kernel passes unknown key=value parameters to init
// as environment variables): "reboot" reboots, "hang" idles forever,
// "sh" runs the shell script libmachine_sh (with /sys and /dev mounted),
// prints "libmachine-init: sh exited STATUS", and powers off.
// libmachine_cat=PATH[,PATH...] prints those files first (procfs at /proc,
// sysfs at /sys), each between "libmachine-init: PATH:" and
// "libmachine-init: end of PATH", or "libmachine-init: PATH: ERROR"; a
// directory prints the names in it, a line each.
// libmachine_action=agent runs libmachine's guest agent
// (/sbin/libmachine-agent) with /sys, /dev and /tmp, reaps every orphan, and
// powers off when the agent exits.
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <termios.h>
#include <unistd.h>

static void cat(const char *path) {
    DIR *dir = opendir(path);
    if (dir) {
        printf("libmachine-init: %s:\n", path);
        for (struct dirent *e; (e = readdir(dir));)
            if (strcmp(e->d_name, ".") != 0 && strcmp(e->d_name, "..") != 0)
                printf("%s\n", e->d_name);
        closedir(dir);
        printf("libmachine-init: end of %s\n", path);
        return;
    }
    FILE *f = fopen(path, "r");
    if (!f) {
        printf("libmachine-init: %s: %s\n", path, strerror(errno));
        return;
    }
    printf("libmachine-init: %s:\n", path);
    char buf[4096];
    size_t n;
    while ((n = fread(buf, 1, sizeof buf, f)) > 0)
        fwrite(buf, 1, n, stdout);
    fclose(f);
    printf("libmachine-init: end of %s\n", path);
}

#define PREFIX "libmachine-init: "
#define PREFIX "libmachine-init: "

#define AGENT "/sbin/libmachine-agent"

// The "agent" action: the agent as init's child, init reaping whatever the
// agent's commands leave behind, until the agent exits.
static void run_agent(void) {
    if (mount("sysfs", "/sys", "sysfs", 0, NULL) != 0 && errno != EBUSY)
        printf(PREFIX "mount sysfs: %s\n", strerror(errno));
    if (mount("devtmpfs", "/dev", "devtmpfs", 0, NULL) != 0)
        printf(PREFIX "mount devtmpfs: %s\n", strerror(errno));
    setenv("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin", 1);
    fflush(stdout);
    pid_t agent = fork();
    if (agent == 0) {
        execl(AGENT, "libmachine-agent", (char *)NULL);
        perror(PREFIX "exec " AGENT);
        _exit(127);
    }
    if (agent < 0) {
        printf(PREFIX "fork: %s\n", strerror(errno));
        return;
    }
    printf(PREFIX "agent started\n");
    fflush(stdout);
    for (;;) {
        int status;
        pid_t pid = wait(&status);
        if (pid == agent) {
            printf(PREFIX "agent exited (status %d)\n", status);
            return;
        }
        if (pid < 0 && errno == ECHILD)
            return;
    }
}

// The "sh" action: libmachine_sh run by the shell, its output on the console.
static void run_sh(void) {
    if (mount("sysfs", "/sys", "sysfs", 0, NULL) != 0 && errno != EBUSY)
        printf(PREFIX "mount sysfs: %s\n", strerror(errno));
    if (mount("devtmpfs", "/dev", "devtmpfs", 0, NULL) != 0)
        printf(PREFIX "mount devtmpfs: %s\n", strerror(errno));
    setenv("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin", 1);
    const char *script = getenv("libmachine_sh");
    fflush(stdout);
    pid_t sh = fork();
    if (sh == 0) {
        execl("/bin/sh", "sh", "-c", script ? script : "true", (char *)NULL);
        perror(PREFIX "exec /bin/sh");
        _exit(127);
    }
    if (sh < 0) {
        printf(PREFIX "fork: %s\n", strerror(errno));
        return;
    }
    for (;;) {
        int status;
        pid_t pid = wait(&status);
        if (pid == sh) {
            printf(PREFIX "sh exited %d\n", WIFEXITED(status) ? WEXITSTATUS(status) : -1);
            return;
        }
        if (pid < 0 && errno == ECHILD)
            return;
    }
}

int main(void) {
    char uptime[64] = "?";
    unsigned long mem_total_kb = 0;
    if (mount("proc", "/proc", "proc", 0, NULL) == 0) {
        FILE *f = fopen("/proc/uptime", "r");
        if (f) {
            if (fscanf(f, "%63s", uptime) != 1)
                strcpy(uptime, "?");
            fclose(f);
        }
        f = fopen("/proc/meminfo", "r");
        if (f) {
            if (fscanf(f, "MemTotal: %lu kB", &mem_total_kb) != 1)
                mem_total_kb = 0;
            fclose(f);
        }
    }
    printf("libmachine-init: up %s s\n", uptime);
    printf("libmachine-init: MemTotal %lu kB\n", mem_total_kb);

    const char *paths = getenv("libmachine_cat");
    if (paths) {
        mount("sysfs", "/sys", "sysfs", 0, NULL);
        char *list = strdup(paths);
        for (char *p = strtok(list, ","); p; p = strtok(NULL, ","))
            cat(p);
        free(list);
    }

    const char *action = getenv("libmachine_action");
    if (action && strcmp(action, "sh") == 0)
        run_sh();
    if (action && strcmp(action, "agent") == 0)
        run_agent();
    fflush(stdout);
    // The console transmits asynchronously: wait until it has, or the
    // machine stops before the line leaves.
    tcdrain(STDOUT_FILENO);

    if (action && strcmp(action, "hang") == 0)
        for (;;)
            pause();
    if (action && strcmp(action, "reboot") == 0)
        reboot(RB_AUTOBOOT);
    reboot(RB_POWER_OFF);
    perror("libmachine-init: reboot");
    return 1;
}
