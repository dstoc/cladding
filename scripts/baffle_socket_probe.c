/* Mirror Baffle 0.2.0's filesystem bind checks and report each failing errno. */
#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

static int fail(const char *operation, const char *path) {
    int error = errno;
    fprintf(stderr, "baffle_socket_probe: %s failed: path=%s errno=%d (%s)\n",
            operation, path, error, strerror(error));
    errno = error;
    return 1;
}

static int open_parent(const char *path, char *name, size_t name_size) {
    char absolute[PATH_MAX];
    if (path[0] == '/') {
        if (snprintf(absolute, sizeof(absolute), "%s", path) >= (int)sizeof(absolute)) {
            errno = ENAMETOOLONG;
            fail("copy socket path", path);
            return -1;
        }
    } else {
        char cwd[PATH_MAX];
        if (getcwd(cwd, sizeof(cwd)) == NULL) {
            fail("getcwd", path);
            return -1;
        }
        if (snprintf(absolute, sizeof(absolute), "%s/%s", cwd, path) >= (int)sizeof(absolute)) {
            errno = ENAMETOOLONG;
            fail("resolve socket path", path);
            return -1;
        }
    }
    if (strlen(absolute) >= sizeof(((struct sockaddr_un *)0)->sun_path)) {
        errno = ENAMETOOLONG;
        fail("validate socket path length", absolute);
        return -1;
    }

    char *last_slash = strrchr(absolute, '/');
    if (last_slash == NULL || last_slash[1] == '\0' ||
        snprintf(name, name_size, "%s", last_slash + 1) >= (int)name_size) {
        errno = EINVAL;
        fail("parse socket name", absolute);
        return -1;
    }
    *last_slash = '\0';
    const char *parent_path = absolute[0] == '\0' ? "/" : absolute;

    /* O_PATH needs search permission on parents, but not read permission. */
    int current = open("/", O_PATH | O_DIRECTORY | O_CLOEXEC);
    if (current == -1) {
        fail("open root directory", "/");
        return -1;
    }
    char *cursor = (char *)parent_path;
    while (*cursor == '/') cursor++;
    while (*cursor != '\0') {
        char *end = strchr(cursor, '/');
        if (end != NULL) *end = '\0';
        if (*cursor != '\0' && strcmp(cursor, ".") != 0) {
            int next = openat(current, cursor,
                              O_PATH | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW);
            if (next == -1) {
                fail("openat parent component", cursor);
                close(current);
                return -1;
            }
            struct stat metadata;
            if (fstat(next, &metadata) == -1) {
                fail("fstat parent component", cursor);
                close(next);
                close(current);
                return -1;
            }
            fprintf(stderr,
                    "baffle_socket_probe: opened parent: path_component=%s uid=%u gid=%u mode=%04o dev=%llu inode=%llu\n",
                    cursor, (unsigned)metadata.st_uid, (unsigned)metadata.st_gid,
                    (unsigned)(metadata.st_mode & 07777),
                    (unsigned long long)metadata.st_dev,
                    (unsigned long long)metadata.st_ino);
            close(current);
            current = next;
        }
        if (end == NULL) break;
        cursor = end + 1;
        while (*cursor == '/') cursor++;
    }
    return current;
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: %s <absolute-socket-path>\n", argv[0]);
        return 2;
    }
    umask(0077);

    char name[NAME_MAX + 1];
    int parent = open_parent(argv[1], name, sizeof(name));
    if (parent == -1) return 1;

    struct stat before;
    if (fstatat(parent, name, &before, AT_SYMLINK_NOFOLLOW) == 0) {
        errno = EEXIST;
        fail("check absent socket path with fstatat", argv[1]);
        close(parent);
        return 1;
    }
    if (errno != ENOENT) {
        fail("check absent socket path with fstatat", argv[1]);
        close(parent);
        return 1;
    }
    fprintf(stderr, "baffle_socket_probe: fstatat absent-path check passed: path=%s\n", argv[1]);

    char bind_path[sizeof(((struct sockaddr_un *)0)->sun_path)];
    if (snprintf(bind_path, sizeof(bind_path), "/proc/self/fd/%d/%s", parent, name) >=
        (int)sizeof(bind_path)) {
        errno = ENAMETOOLONG;
        fail("format proc-fd bind path", argv[1]);
        close(parent);
        return 1;
    }
    struct sockaddr_un address = { .sun_family = AF_UNIX };
    memcpy(address.sun_path, bind_path, strlen(bind_path) + 1);
    socklen_t address_length = (socklen_t)(offsetof(struct sockaddr_un, sun_path) +
                                           strlen(bind_path) + 1);

    int listener = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (listener == -1) {
        fail("socket(AF_UNIX, SOCK_STREAM)", bind_path);
        close(parent);
        return 1;
    }
    if (bind(listener, (struct sockaddr *)&address, address_length) == -1) {
        int result = fail("bind proc-fd socket", bind_path);
        close(listener);
        close(parent);
        return result;
    }
    fprintf(stderr, "baffle_socket_probe: bind passed: path=%s\n", bind_path);
    if (listen(listener, 128) == -1) {
        int result = fail("listen bound socket", bind_path);
        unlinkat(parent, name, 0);
        close(listener);
        close(parent);
        return result;
    }
    fprintf(stderr, "baffle_socket_probe: listen passed: path=%s\n", bind_path);
    int socket_flags = fcntl(listener, F_GETFL);
    if (socket_flags == -1 || fcntl(listener, F_SETFL, socket_flags | O_NONBLOCK) == -1) {
        int result = fail("set listener nonblocking", bind_path);
        unlinkat(parent, name, 0);
        close(listener);
        close(parent);
        return result;
    }
    fprintf(stderr, "baffle_socket_probe: nonblocking listener setup passed: path=%s\n", bind_path);

    struct stat identity;
    if (fstatat(parent, name, &identity, AT_SYMLINK_NOFOLLOW) == -1) {
        int result = fail("fstatat bound socket", argv[1]);
        unlinkat(parent, name, 0);
        close(listener);
        close(parent);
        return result;
    }
    if (!S_ISSOCK(identity.st_mode)) {
        errno = EPROTO;
        int result = fail("verify bound socket type", argv[1]);
        unlinkat(parent, name, 0);
        close(listener);
        close(parent);
        return result;
    }
    fprintf(stderr, "baffle_socket_probe: bound socket identity: dev=%llu inode=%llu mode=%04o uid=%u gid=%u\n",
            (unsigned long long)identity.st_dev, (unsigned long long)identity.st_ino,
            (unsigned)(identity.st_mode & 07777), (unsigned)identity.st_uid,
            (unsigned)identity.st_gid);

    if (chmod(bind_path, 0600) == -1) {
        int result = fail("chmod bound socket to 0600", bind_path);
        unlinkat(parent, name, 0);
        close(listener);
        close(parent);
        return result;
    }
    fprintf(stderr, "baffle_socket_probe: chmod to 0600 passed: path=%s\n", bind_path);

    struct stat current;
    if (fstatat(parent, name, &current, AT_SYMLINK_NOFOLLOW) == -1) {
        int result = fail("fstatat socket after chmod", argv[1]);
        unlinkat(parent, name, 0);
        close(listener);
        close(parent);
        return result;
    }
    if (!S_ISSOCK(current.st_mode) || current.st_dev != identity.st_dev ||
        current.st_ino != identity.st_ino || (current.st_mode & 0777) != 0600) {
        errno = EPROTO;
        int result = fail("verify socket identity and mode", argv[1]);
        unlinkat(parent, name, 0);
        close(listener);
        close(parent);
        return result;
    }
    fprintf(stderr, "baffle_socket_probe: final socket identity and mode check passed\n");

    if (unlinkat(parent, name, 0) == -1) {
        int result = fail("unlinkat probe socket", argv[1]);
        close(listener);
        close(parent);
        return result;
    }
    close(listener);
    close(parent);
    fprintf(stderr, "baffle_socket_probe: cleanup passed: path=%s\n", argv[1]);
    return 0;
}
