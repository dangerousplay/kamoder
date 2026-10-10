/*
 * mtdpatch - rewrite only the erase blocks of an MTD partition that differ
 * from an image, verifying every block by reading it back.
 *
 *   mtdpatch [-n] DEVICE IMAGE [RESTORE_IMAGE]
 *
 *   -n             dry run: report differing blocks, write nothing
 *   DEVICE         e.g. /dev/mtd6
 *   IMAGE          image to write; bytes past its end are left untouched
 *   RESTORE_IMAGE  written back (same way) if IMAGE cannot be written
 *
 * Running it again after success changes nothing (every block matches).
 * Exit: 0 ok, 1 usage/IO setup error, 2 write failed but restore ok,
 * 3 write failed and restore failed (or no restore image given).
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <mtd/mtd-user.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <unistd.h>

#define RETRIES 3

static int dry_run;

static int read_full(int fd, void *buf, size_t len, off_t off)
{
    size_t done = 0;
    while (done < len) {
        ssize_t r = pread(fd, (char *)buf + done, len - done, off + done);
        if (r < 0 && errno == EINTR)
            continue;
        if (r <= 0)
            return -1;
        done += r;
    }
    return 0;
}

static int write_full(int fd, const void *buf, size_t len, off_t off)
{
    size_t done = 0;
    while (done < len) {
        ssize_t r = pwrite(fd, (const char *)buf + done, len - done, off + done);
        if (r < 0 && errno == EINTR)
            continue;
        if (r <= 0)
            return -1;
        done += r;
    }
    return 0;
}

static unsigned char *load(const char *path, size_t *len)
{
    int fd = open(path, O_RDONLY);
    struct stat st;
    unsigned char *buf;

    if (fd < 0 || fstat(fd, &st) < 0) {
        fprintf(stderr, "mtdpatch: %s: %s\n", path, strerror(errno));
        return NULL;
    }
    buf = malloc(st.st_size ? st.st_size : 1);
    if (!buf || read_full(fd, buf, st.st_size, 0) < 0) {
        fprintf(stderr, "mtdpatch: read %s failed\n", path);
        free(buf);
        close(fd);
        return NULL;
    }
    close(fd);
    *len = st.st_size;
    return buf;
}

/* Returns 0 if every block of img now matches flash, -1 otherwise. */
static int apply(int mtd, const struct mtd_info_user *mi, const char *name,
                 const unsigned char *img, size_t len)
{
    unsigned char *cur = malloc(mi->erasesize);
    size_t off;
    unsigned differ = 0, written = 0;

    if (!cur)
        return -1;
    for (off = 0; off < len; off += mi->erasesize) {
        size_t n = len - off < mi->erasesize ? len - off : mi->erasesize;
        int tries;

        if (read_full(mtd, cur, n, off) < 0) {
            fprintf(stderr, "mtdpatch: read flash at 0x%zx: %s\n", off, strerror(errno));
            free(cur);
            return -1;
        }
        if (!memcmp(cur, img + off, n))
            continue;
        differ++;
        if (dry_run) {
            printf("  differs: block 0x%06zx\n", off);
            continue;
        }
        for (tries = 0; tries < RETRIES; tries++) {
            struct erase_info_user ei = { .start = off, .length = mi->erasesize };

            /* A short last block keeps the rest of its erase block as it was. */
            if (n < mi->erasesize) {
                unsigned char *full = malloc(mi->erasesize);
                if (!full || read_full(mtd, full, mi->erasesize, off) < 0) {
                    free(full);
                    continue;
                }
                memcpy(full, img + off, n);
                if (ioctl(mtd, MEMERASE, &ei) < 0 ||
                    write_full(mtd, full, mi->erasesize, off) < 0) {
                    free(full);
                    continue;
                }
                free(full);
            } else if (ioctl(mtd, MEMERASE, &ei) < 0 ||
                       write_full(mtd, img + off, n, off) < 0) {
                fprintf(stderr, "mtdpatch: erase/write 0x%zx: %s\n", off, strerror(errno));
                continue;
            }
            if (read_full(mtd, cur, n, off) == 0 && !memcmp(cur, img + off, n))
                break;
            fprintf(stderr, "mtdpatch: verify failed at 0x%zx (try %d)\n", off, tries + 1);
        }
        if (tries == RETRIES) {
            fprintf(stderr, "mtdpatch: giving up at 0x%zx\n", off);
            free(cur);
            return -1;
        }
        written++;
    }
    free(cur);
    printf("%s: %u block(s) differed, %u written\n", name, differ, written);
    return 0;
}

int main(int argc, char **argv)
{
    struct mtd_info_user mi;
    unsigned char *img, *restore = NULL;
    size_t len, rlen = 0;
    int mtd, a = 1;

    if (argc > 1 && !strcmp(argv[1], "-n")) {
        dry_run = 1;
        a++;
    }
    if (argc - a < 2 || argc - a > 3) {
        fprintf(stderr, "usage: %s [-n] DEVICE IMAGE [RESTORE_IMAGE]\n", argv[0]);
        return 1;
    }
    mtd = open(argv[a], dry_run ? O_RDONLY : O_RDWR | O_SYNC);
    if (mtd < 0 || ioctl(mtd, MEMGETINFO, &mi) < 0) {
        fprintf(stderr, "mtdpatch: %s: %s\n", argv[a], strerror(errno));
        return 1;
    }
    if (!(img = load(argv[a + 1], &len)))
        return 1;
    if (argc - a == 3 && !(restore = load(argv[a + 2], &rlen)))
        return 1;
    if (len > mi.size || rlen > mi.size || !mi.erasesize) {
        fprintf(stderr, "mtdpatch: image larger than %s (0x%x)\n", argv[a], mi.size);
        return 1;
    }
    printf("%s: size 0x%x, erasesize 0x%x, image %zu bytes%s\n", argv[a], mi.size,
           mi.erasesize, len, dry_run ? " (dry run)" : "");

    if (apply(mtd, &mi, argv[a + 1], img, len) == 0) {
        puts("OK");
        return 0;
    }
    if (!restore) {
        fputs("FAILED, no restore image given\n", stderr);
        return 3;
    }
    fputs("FAILED, restoring\n", stderr);
    if (apply(mtd, &mi, argv[a + 2], restore, rlen) == 0) {
        fputs("RESTORED original\n", stderr);
        return 2;
    }
    fputs("RESTORE FAILED - do not reboot, recover over UART\n", stderr);
    return 3;
}
