#include <errno.h>
#include <netdb.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <unistd.h>

#define unlikely(x) __builtin_expect(!!(x), 0)

static void devnull(const int fd) {
    char buf[65536] __attribute__((aligned(4096))); // Page align
    size_t total = 0;
    while (1) {
        ssize_t len = read(fd, buf, 65536);
        if (unlikely(len <= 0)) {
            if (len == 0) {
                fprintf(stderr, "Child exiting: EOF: %zu\n", total);
            } else {
                fprintf(stderr, "Child exiting: %s: %zu\n", strerror(errno), total);
            }
            return;
        }
        total += len;
    }
}

int main(int argc, char *argv[]) {
    if (argc != 2) {
        fprintf(stderr, "Usage: %s <PORT>\n", argv[0]);
        return 1;
    }

    struct sockaddr_storage their_addr;
    socklen_t addr_size;
    struct addrinfo hints, *res;
    int sockfd, acceptfd;
    pid_t pid;

    // first, load up address structs with getaddrinfo():

    memset(&hints, 0, sizeof hints);
    hints.ai_family = AF_UNSPEC;  // use IPv4 or IPv6, whichever
    hints.ai_socktype = SOCK_STREAM;
    hints.ai_flags = AI_PASSIVE;     // fill in my IP for me

    const char *addr = argv[1];
    int ret = getaddrinfo(NULL, addr, &hints, &res) != 0;
    if (ret != 0) {
        fprintf(stderr, "Error: getaddrinfo: %s: '%s'\n", gai_strerror(ret), addr);
        return 1;
    }

    // make a socket:

    sockfd = socket(res->ai_family, res->ai_socktype, res->ai_protocol);
    if (sockfd == -1) {
        perror("Error: socket");
        return 1;
    }

    // bind it to the port we passed in to getaddrinfo():

    if (bind(sockfd, res->ai_addr, res->ai_addrlen) != 0) {
        perror("Error: bind");
        return 1;
    }

    if (listen(sockfd, 1) != 0) {
        perror("Error: listen");
        return 1;
    }

    while (1) {
        acceptfd = accept(sockfd, (struct sockaddr *)&their_addr, &addr_size);
        pid = fork();
        if (pid == 0) {
            // child
            devnull(acceptfd);
            return 0;
        } else {
            fprintf(stderr, "Forked pid %i\n", pid);
        }
    }

    return 0;
}
