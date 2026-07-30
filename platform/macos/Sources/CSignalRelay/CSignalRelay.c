#include "CSignalRelay.h"

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <unistd.h>

static volatile sig_atomic_t relay_write_descriptor = -1;

static void relay_signal(int signal_number) {
    int saved_errno = errno;
    int descriptor = (int)relay_write_descriptor;
    if (descriptor >= 0) {
        uint8_t marker = (uint8_t)signal_number;
        ssize_t result;
        do {
            result = write(descriptor, &marker, sizeof(marker));
        } while (result < 0 && errno == EINTR);
        /* EAGAIN means an earlier marker already keeps the relay latched. */
    }
    errno = saved_errno;
}

static int configure_descriptor(int descriptor) {
    int descriptor_flags = fcntl(descriptor, F_GETFD);
    if (descriptor_flags < 0 ||
        fcntl(descriptor, F_SETFD, descriptor_flags | FD_CLOEXEC) < 0) {
        return -1;
    }
    int status_flags = fcntl(descriptor, F_GETFL);
    if (status_flags < 0 ||
        fcntl(descriptor, F_SETFL, status_flags | O_NONBLOCK) < 0) {
        return -1;
    }
    return 0;
}

int32_t pca_signal_relay_install(int32_t *read_descriptor) {
    if (read_descriptor == NULL || relay_write_descriptor >= 0) {
        return EINVAL;
    }

    sigset_t blocked_signals;
    sigset_t previous_mask;
    if (sigemptyset(&blocked_signals) < 0 ||
        sigaddset(&blocked_signals, SIGINT) < 0 ||
        sigaddset(&blocked_signals, SIGTERM) < 0 ||
        sigprocmask(SIG_BLOCK, &blocked_signals, &previous_mask) < 0) {
        return errno;
    }

    int descriptors[2] = {-1, -1};
    if (pipe(descriptors) < 0 ||
        configure_descriptor(descriptors[0]) < 0 ||
        configure_descriptor(descriptors[1]) < 0) {
        int result = errno;
        if (descriptors[0] >= 0) {
            close(descriptors[0]);
        }
        if (descriptors[1] >= 0) {
            close(descriptors[1]);
        }
        (void)sigprocmask(SIG_SETMASK, &previous_mask, NULL);
        return result;
    }

    struct sigaction action = {0};
    action.sa_handler = relay_signal;
    action.sa_flags = SA_RESTART;
    (void)sigemptyset(&action.sa_mask);
    (void)sigaddset(&action.sa_mask, SIGINT);
    (void)sigaddset(&action.sa_mask, SIGTERM);

    struct sigaction previous_interrupt;
    struct sigaction previous_termination;
    relay_write_descriptor = descriptors[1];
    if (sigaction(SIGINT, &action, &previous_interrupt) < 0) {
        int result = errno;
        relay_write_descriptor = -1;
        close(descriptors[0]);
        close(descriptors[1]);
        (void)sigprocmask(SIG_SETMASK, &previous_mask, NULL);
        return result;
    }
    if (sigaction(SIGTERM, &action, &previous_termination) < 0) {
        int result = errno;
        (void)sigaction(SIGINT, &previous_interrupt, NULL);
        relay_write_descriptor = -1;
        close(descriptors[0]);
        close(descriptors[1]);
        (void)sigprocmask(SIG_SETMASK, &previous_mask, NULL);
        return result;
    }
    if (sigprocmask(SIG_SETMASK, &previous_mask, NULL) < 0) {
        int result = errno;
        (void)sigaction(SIGINT, &previous_interrupt, NULL);
        (void)sigaction(SIGTERM, &previous_termination, NULL);
        relay_write_descriptor = -1;
        close(descriptors[0]);
        close(descriptors[1]);
        return result;
    }

    *read_descriptor = descriptors[0];
    return 0;
}
