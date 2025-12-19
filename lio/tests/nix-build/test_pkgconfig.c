#include <stdio.h>
#include <unistd.h>
#include <assert.h>

#include <lio.h>

static int callback_executed = 0;

void call(int32_t verynice) {
  printf("yay %d", verynice);
  callback_executed = 1;
}

int main() {
    printf("Testing lio library via pkg-config\n");

    lio_init();
    lio_start();

    lio_timeout(2000, call);

    sleep(3);

    lio_stop();
    lio_exit();

    assert(callback_executed && "Callback should have been executed");
    printf("Callback executed successfully\n");

    return 0;
}
