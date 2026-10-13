/* Static, no-KVM executable for the official jailer's path-layout test only. */
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>

int main(void) {
    char cwd[128];
    if (!getcwd(cwd, sizeof cwd)) return 1;
    FILE *out = fopen("/root-observed", "wx");
    if (!out) return 2;
    fprintf(out, "uid=%u gid=%u root=%s\n", getuid(), getgid(), cwd);
    return fclose(out) != 0;
}
