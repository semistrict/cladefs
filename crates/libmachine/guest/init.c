// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// libmachine's test init: prints a marker with the kernel's uptime and the
// RAM it sees (MemTotal from /proc/meminfo), then powers the machine off; or, by the kernel command line's
// libmachine_action (the kernel passes unknown key=value parameters to init
// as environment variables): "reboot" reboots, "hang" idles forever,
// "spin" keeps every online CPU busy forever (a process pinned to each),
// "devices" first reports the devices the kernel found (report_devices).
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
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
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
#define PCI_DEVICES "/sys/bus/pci/devices"
#define VIRTIO_DEVICES "/sys/bus/virtio/devices"

// The first line of a file, without its newline; "?" if it cannot be read.
static void read_line(const char *path, char *buf, size_t len) {
    FILE *f = fopen(path, "r");
    if (!f || !fgets(buf, (int)len, f))
        snprintf(buf, len, "?");
    else
        buf[strcspn(buf, "\n")] = 0;
    if (f)
        fclose(f);
}

// The name of the driver bound to the device in `dir`, or "-".
static void driver_of(const char *dir, char *buf, size_t len) {
    char path[512], target[512];
    snprintf(path, sizeof(path), "%s/driver", dir);
    ssize_t n = readlink(path, target, sizeof(target) - 1);
    if (n < 0) {
        snprintf(buf, len, "-");
        return;
    }
    target[n] = 0;
    const char *slash = strrchr(target, '/');
    snprintf(buf, len, "%s", slash ? slash + 1 : target);
}

static void print_hex(const volatile uint8_t *p, size_t n) {
    for (size_t i = 0; i < n; i++)
        printf("%02x", p[i]);
}

// Each PCI function: its IDs, class and driver, its configuration space as
// the kernel reads it, and its BARs' resources.
static void report_pci(void) {
    DIR *d = opendir(PCI_DEVICES);
    if (!d) {
        printf(PREFIX "pci none (%s)\n", strerror(errno));
        return;
    }
    struct dirent *e;
    while ((e = readdir(d))) {
        if (e->d_name[0] == '.')
            continue;
        char dir[512], path[600], v[32], dv[32], sv[32], sd[32], cls[32], drv[512];
        snprintf(dir, sizeof(dir), PCI_DEVICES "/%s", e->d_name);
        snprintf(path, sizeof(path), "%s/vendor", dir);
        read_line(path, v, sizeof(v));
        snprintf(path, sizeof(path), "%s/device", dir);
        read_line(path, dv, sizeof(dv));
        snprintf(path, sizeof(path), "%s/subsystem_vendor", dir);
        read_line(path, sv, sizeof(sv));
        snprintf(path, sizeof(path), "%s/subsystem_device", dir);
        read_line(path, sd, sizeof(sd));
        snprintf(path, sizeof(path), "%s/class", dir);
        read_line(path, cls, sizeof(cls));
        driver_of(dir, drv, sizeof(drv));
        printf(PREFIX "pci %s vendor=%s device=%s subsystem_vendor=%s "
                      "subsystem_device=%s class=%s driver=%s\n",
               e->d_name, v, dv, sv, sd, cls, drv);

        uint8_t config[256];
        snprintf(path, sizeof(path), "%s/config", dir);
        int fd = open(path, O_RDONLY);
        ssize_t n = fd < 0 ? -1 : read(fd, config, sizeof(config));
        if (fd >= 0)
            close(fd);
        if (n > 0) {
            printf(PREFIX "pci-config %s ", e->d_name);
            print_hex(config, (size_t)n);
            printf("\n");
        }

        snprintf(path, sizeof(path), "%s/resource", dir);
        FILE *f = fopen(path, "r");
        char line[128];
        for (int bar = 0; f && fgets(line, sizeof(line), f); bar++) {
            line[strcspn(line, "\n")] = 0;
            if (strncmp(line, "0x0000000000000000 0x0000000000000000", 37) != 0)
                printf(PREFIX "pci-resource %s %d %s\n", e->d_name, bar, line);
        }
        if (f)
            fclose(f);
    }
    closedir(d);
}

// Each virtio device: its type, vendor, negotiated features and driver.
static void report_virtio(void) {
    DIR *d = opendir(VIRTIO_DEVICES);
    if (!d) {
        printf(PREFIX "virtio none (%s)\n", strerror(errno));
        return;
    }
    struct dirent *e;
    while ((e = readdir(d))) {
        if (e->d_name[0] == '.')
            continue;
        char dir[512], path[600], dv[32], v[32], feat[160], drv[512];
        snprintf(dir, sizeof(dir), VIRTIO_DEVICES "/%s", e->d_name);
        snprintf(path, sizeof(path), "%s/device", dir);
        read_line(path, dv, sizeof(dv));
        snprintf(path, sizeof(path), "%s/vendor", dir);
        read_line(path, v, sizeof(v));
        snprintf(path, sizeof(path), "%s/features", dir);
        read_line(path, feat, sizeof(feat));
        driver_of(dir, drv, sizeof(drv));
        printf(PREFIX "virtio %s device=%s vendor=%s driver=%s features=%s\n",
               e->d_name, dv, v, drv, feat);
    }
    closedir(d);
}

static void read_hwrng(void) {
    uint8_t buf[32];
    int fd = open("/dev/hwrng", O_RDONLY);
    ssize_t n = fd < 0 ? -1 : read(fd, buf, sizeof(buf));
    if (n > 0) {
        printf(PREFIX "hwrng ");
        print_hex(buf, (size_t)n);
        printf("\n");
    } else {
        printf(PREFIX "hwrng error %s\n", strerror(errno));
    }
    if (fd >= 0)
        close(fd);
}

// The interrupt lines of PCI devices (MSI and MSI-X), with their counts.
static void report_interrupts(void) {
    FILE *f = fopen("/proc/interrupts", "r");
    char line[512];
    while (f && fgets(line, sizeof(line), f)) {
        if (!strstr(line, "PCI-MSI"))
            continue;
        line[strcspn(line, "\n")] = 0;
        char *start = line;
        while (*start == ' ')
            start++;
        printf(PREFIX "irq %s\n", start);
    }
    if (f)
        fclose(f);
}

// Maps `len` bytes at `offset` of a PCI function's resource file (relative
// to /sys/bus/pci/devices, e.g. "0000:00:01.0/resource2").
static volatile uint8_t *map_resource(const char *file, size_t offset, size_t len,
                                      void **base, size_t *map_len) {
    char path[512];
    snprintf(path, sizeof(path), PCI_DEVICES "/%s", file);
    int fd = open(path, O_RDWR | O_SYNC);
    if (fd < 0)
        return NULL;
    size_t page = (size_t)sysconf(_SC_PAGESIZE);
    size_t start = offset & ~(page - 1);
    *map_len = (offset + len - start + page - 1) & ~(page - 1);
    *base = mmap(NULL, *map_len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, (off_t)start);
    close(fd);
    if (*base == MAP_FAILED)
        return NULL;
    return (volatile uint8_t *)*base + (offset - start);
}

// libmachine_peek=FILE:OFFSET:LEN[,...] prints LEN bytes at OFFSET of a
// resource file (byte accesses); libmachine_poke=FILE:OFFSET:HEX[,...]
// writes the bytes HEX spells there. Offsets and lengths are decimal.
static void peek_poke(const char *list, int poke) {
    char *specs = strdup(list), *save = NULL;
    for (char *spec = strtok_r(specs, ",", &save); spec; spec = strtok_r(NULL, ",", &save)) {
        // FILE holds colons (a PCI address): split OFFSET and the last part
        // off the right.
        char *arg = strrchr(spec, ':');
        if (arg)
            *arg++ = 0;
        char *off = arg ? strrchr(spec, ':') : NULL;
        if (!off) {
            printf(PREFIX "%s bad spec %s\n", poke ? "poke" : "peek", spec);
            continue;
        }
        *off++ = 0;
        const char *file = spec;
        size_t offset = strtoul(off, NULL, 10);
        size_t len = poke ? strlen(arg) / 2 : strtoul(arg, NULL, 10);
        void *base;
        size_t map_len;
        volatile uint8_t *p = map_resource(file, offset, len, &base, &map_len);
        if (!p) {
            printf(PREFIX "%s %s %zu error %s\n", poke ? "poke" : "peek", file, offset,
                   strerror(errno));
            continue;
        }
        if (poke) {
            for (size_t i = 0; i < len; i++) {
                unsigned byte;
                sscanf(arg + 2 * i, "%2x", &byte);
                p[i] = (uint8_t)byte;
            }
            printf(PREFIX "poke %s %zu ok\n", file, offset);
        } else {
            printf(PREFIX "peek %s %zu ", file, offset);
            print_hex(p, len);
            printf("\n");
        }
        munmap(base, map_len);
    }
    free(specs);
}

// The "devices" action: what the kernel found on its PCI bus and virtio
// bus, after using the virtio-rng and peeking and poking BARs as the
// command line asks, so that the interrupt counts include that use.
static void report_devices(void) {
    mkdir("/sys", 0555);
    if (mount("sysfs", "/sys", "sysfs", 0, NULL) != 0 && errno != EBUSY)
        printf(PREFIX "mount sysfs: %s\n", strerror(errno));
    if (mount("devtmpfs", "/dev", "devtmpfs", 0, NULL) != 0)
        printf(PREFIX "mount devtmpfs: %s\n", strerror(errno));
    const char *poke = getenv("libmachine_poke");
    if (poke)
        peek_poke(poke, 1);
    const char *peek = getenv("libmachine_peek");
    if (peek)
        peek_poke(peek, 0);
    read_hwrng();
    report_pci();
    report_virtio();
    report_interrupts();
    printf(PREFIX "devices done\n");
}

// The "spin" action: a busy process pinned to each online CPU.
static void spin(void) {
    long n = sysconf(_SC_NPROCESSORS_ONLN);
    fflush(stdout);
    for (long cpu = 0; cpu < n; cpu++) {
        pid_t pid = fork();
        if (pid == 0) {
            cpu_set_t set;
            CPU_ZERO(&set);
            CPU_SET(cpu, &set);
            sched_setaffinity(0, sizeof(set), &set);
            for (volatile unsigned long i = 0;; i++)
                ;
        }
        if (pid < 0)
            printf(PREFIX "fork: %s\n", strerror(errno));
    }
    printf(PREFIX "spinning on %ld CPUs\n", n);
}

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
    if (action && strcmp(action, "devices") == 0)
        report_devices();
    if (action && strcmp(action, "spin") == 0)
        spin();
    if (action && strcmp(action, "agent") == 0)
        run_agent();
    fflush(stdout);
    // The console transmits asynchronously: wait until it has, or the
    // machine stops before the line leaves.
    tcdrain(STDOUT_FILENO);

    if (action && (strcmp(action, "hang") == 0 || strcmp(action, "spin") == 0))
        for (;;)
            pause();
    if (action && strcmp(action, "reboot") == 0)
        reboot(RB_AUTOBOOT);
    reboot(RB_POWER_OFF);
    perror("libmachine-init: reboot");
    return 1;
}
