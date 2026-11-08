/* Host-only no-KVM process fixture for service restart and adoption tests.
 * Ignore argv: its executable name must be firecracker for /proc checks. */
#include <unistd.h>

int main(void) {
    for (;;) pause();
}
