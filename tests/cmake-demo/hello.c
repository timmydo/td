/* td-cmake-hello — the demonstrator binary built by td's OWN cmake build system
 * (build::run_cmake). Prints a fixed line the td-cmake-demo recipe check's behavioral leg
 * asserts, proving the cmake-built artifact actually runs. */
#include <stdio.h>

int main(void) {
  printf("td cmake-build hello\n");
  return 0;
}
