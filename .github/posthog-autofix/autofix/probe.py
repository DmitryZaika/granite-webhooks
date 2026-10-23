"""One network probe, run as the agent user by `check-isolation`.

    python3 -I probe.py tcp <host> <port>   connect over TCP
    python3 -I probe.py resolve <host>      resolve a name
    python3 -I probe.py udp-dns <ip>        send a DNS query and wait for an answer
    python3 -I probe.py unix <path>         connect to a Unix socket

Exit 0 when the attempt succeeded, 1 when it failed, 2 on bad usage.
Standalone (stdlib only, no package imports) so `-I` can run it.
"""

import socket
import sys

TIMEOUT_SECONDS = 5
# A minimal query for example.com, type A.
_DNS_QUERY = bytes.fromhex("abcd01000001000000000000076578616d706c6503636f6d0000010001")


def attempt(kind: str, args: list[str]) -> None:
    if kind == "tcp":
        socket.create_connection((args[0], int(args[1])), timeout=TIMEOUT_SECONDS).close()
    elif kind == "resolve":
        socket.getaddrinfo(args[0], 443)
    elif kind == "udp-dns":
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp:
            udp.settimeout(TIMEOUT_SECONDS)
            udp.sendto(_DNS_QUERY, (args[0], 53))
            udp.recvfrom(512)
    elif kind == "unix":
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as unix:
            unix.settimeout(TIMEOUT_SECONDS)
            unix.connect(args[0])
    else:
        raise ValueError(kind)


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        return 2
    try:
        attempt(argv[0], argv[1:])
    except (ValueError, IndexError):
        return 2
    except OSError:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
