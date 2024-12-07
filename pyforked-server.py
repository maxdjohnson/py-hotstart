import array
import code
import fcntl
import os
import resource
import socket
import termios
import traceback

SERVER_ADDRESS = "/tmp/pyforked-server.sock"
LOG_PATH = os.path.expanduser("~/Library/Logs/pyforked-server.log")
MAXFD = 2048


def daemonize():
    # Daemonize
    pid = os.fork()
    if pid > 0:
        os._exit(0)
    os.setsid()
    pid = os.fork()
    if pid > 0:
        os._exit(0)
    os.chdir("/")
    os.umask(0)
    maxfd = resource.getrlimit(resource.RLIMIT_NOFILE)[1]
    maxfd = MAXFD if maxfd == resource.RLIM_INFINITY else maxfd
    for fd in range(3, maxfd):
        try:
            os.close(fd)
        except OSError:
            pass
    log_fd = os.open(LOG_PATH, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    os.dup2(log_fd, 1)
    os.dup2(log_fd, 2)
    null_fd = os.open(os.devnull, os.O_RDONLY)
    os.dup2(null_fd, 0)
    os.close(null_fd)
    if log_fd > 2:
        os.close(log_fd)


def run_forkserver():
    if os.path.exists(SERVER_ADDRESS):
        os.unlink(SERVER_ADDRESS)
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(SERVER_ADDRESS)
    server.listen(5)

    while True:
        conn, _ = server.accept()
        with conn:
            try:
                # Receive a message and a fd (the slave pty fd)
                # We'll do recvmsg to get the fd
                msg, fds = recv_fds(conn.fileno())
                if msg != b"RUN":
                    continue

                if not fds:
                    continue

                slave_fd = fds[0]

                pid = os.fork()
                if pid == 0:
                    try:
                        os.setsid()
                        fcntl.ioctl(slave_fd, termios.TIOCSCTTY, 0)
                        os.dup2(slave_fd, 0)
                        os.dup2(slave_fd, 1)
                        os.dup2(slave_fd, 2)
                        if slave_fd > 2:
                            os.close(slave_fd)
                        code.interact(local={})
                    except BaseException:
                        traceback.print_exc()
                    finally:
                        os._exit(0)
                else:
                    # parent
                    os.close(slave_fd)

                conn.sendall(b"OK")
            except BaseException:
                traceback.print_exc()


def recv_fds(sock_fd, max_fds=1):
    import socket

    buf = bytearray(1024)
    fds = array.array("i", [-1] * max_fds)
    msg, ancdata, flags, addr = socket.socket(fileno=sock_fd).recvmsg_into(
        [buf], 1024, socket.CMSG_SPACE(max_fds * 4)
    )
    for cmsg_level, cmsg_type, cmsg_data in ancdata:
        if cmsg_level == socket.SOL_SOCKET and cmsg_type == socket.SCM_RIGHTS:
            fds.frombytes(cmsg_data)
    data = buf[:msg]
    # Filter out invalid fds
    out_fds = [fd for fd in fds if fd != -1]
    return data, out_fds


def main():
    try:
        daemonize()
        run_forkserver()
    except Exception:
        with open(LOG_PATH, "a") as f:
            traceback.print_exc(file=f)
        os._exit(1)
    os._exit(0)


if __name__ == "__main__":
    main()
