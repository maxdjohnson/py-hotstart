import array
import code
import fcntl
import os
import resource
import signal
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


def recv_fds(conn, max_fds=1):
    buf = bytearray(1024)
    fds = array.array("i", [-1] * max_fds)
    try:
        msg, ancdata, flags, addr = conn.recvmsg_into([buf], 1024, socket.CMSG_SPACE(max_fds * 4))
    except OSError as e:
        print(f"Error receiving message: {e}")
        return b"", []
    if msg == 0:
        # Premature disconnect or no data
        print("Received 0 bytes, client disconnected prematurely.")
        return b"", []
    for cmsg_level, cmsg_type, cmsg_data in ancdata:
        if cmsg_level == socket.SOL_SOCKET and cmsg_type == socket.SCM_RIGHTS:
            fds.frombytes(cmsg_data)
    data = buf[:msg]
    # Filter out invalid fds
    out_fds = [fd for fd in fds if fd != -1]
    return data, out_fds


def run_child(slave_fd):
    # In the child:
    try:
        os.setsid()
        fcntl.ioctl(slave_fd, termios.TIOCSCTTY, 0)
        os.dup2(slave_fd, 0)
        os.dup2(slave_fd, 1)
        os.dup2(slave_fd, 2)
        if slave_fd > 2:
            os.close(slave_fd)

        # If the master is closed, reading from stdin will return EOF.
        # code.interact() doesn't handle EOF by default, so wrap it:
        try:
            code.interact(local={})
        except EOFError:
            # Master fd closed, just exit gracefully
            pass
    except BaseException:
        traceback.print_exc()
    finally:
        os._exit(0)


def handle_client(conn):
    msg, fds = recv_fds(conn)
    if msg != b"RUN":
        if msg:
            print(f"Invalid command: {msg}")
        return

    if not fds:
        print("No fds received or invalid fd.")
        return

    slave_fd = fds[0]
    if slave_fd < 0:
        print("Invalid slave fd received.")
        return

    # Fork the child that will run code.interact
    pid = os.fork()
    if pid == 0:
        run_child(slave_fd)
    else:
        # parent
        os.close(slave_fd)

    # Attempt to send OK
    # If the client disconnected right after sending the fd, sendall might fail
    try:
        conn.sendall(b"OK")
    except OSError as e:
        print(f"Error sending OK to client: {e}")


def run_forkserver():
    if os.path.exists(SERVER_ADDRESS):
        os.unlink(SERVER_ADDRESS)
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(SERVER_ADDRESS)
    server.listen(5)

    # Optionally handle signals for clean shutdown
    # For example, to gracefully stop on SIGTERM:
    def shutdown(signum, frame):
        print("Received shutdown signal, terminating forkserver.")
        server.close()
        if os.path.exists(SERVER_ADDRESS):
            os.unlink(SERVER_ADDRESS)
        os._exit(0)

    signal.signal(signal.SIGTERM, shutdown)
    signal.signal(signal.SIGINT, shutdown)

    while True:
        try:
            conn, _ = server.accept()
        except OSError as e:
            print(f"Error on accept: {e}")
            continue

        # If client closes immediately, conn might be invalid
        with conn:
            try:
                handle_client(conn)
            except Exception:
                traceback.print_exc()


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
