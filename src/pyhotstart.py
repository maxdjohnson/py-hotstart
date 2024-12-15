import array
import fcntl
import importlib
import os
import resource
import signal
import socket
import sys
import termios
import traceback

SERVER_ADDRESS = "/tmp/pyhotstart.sock"
LOG_PATH = os.path.expanduser("~/Library/Logs/pyhotstart.log")
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
    out_fds = [fd for fd in fds if fd != -1]
    return data, out_fds


def handle_sigchld(signum, frame):
    """Handler for SIGCHLD to reap zombie processes."""
    while True:
        try:
            pid, _ = os.waitpid(-1, os.WNOHANG)
            if pid <= 0:
                break
        except ChildProcessError:
            break


def shutdown(server, server_pid):
    if os.getpid() != server_pid:
        return
    server.close()
    try:
        os.unlink(SERVER_ADDRESS)
    except Exception:
        traceback.print_exc()


def cmd_run(code_snippet, fds):
    """
    Runs code snippet inline.
    If len(fds) == 1, treat as PTY mode.
    If len(fds) > 1, treat as normal mode with fds[0..2] as stdio.
    After run, exit the server.
    """

    if len(fds) == 1:
        # PTY mode
        slave_fd = fds[0]
        fcntl.ioctl(slave_fd, termios.TIOCSCTTY, 0)
        os.dup2(slave_fd, 0)
        os.dup2(slave_fd, 1)
        os.dup2(slave_fd, 2)
        if slave_fd > 2:
            os.close(slave_fd)
        os.kill(os.getpid(), signal.SIGWINCH)
    elif len(fds) >= 3:
        # Normal mode
        os.dup2(fds[0], 0)
        os.dup2(fds[1], 1)
        os.dup2(fds[2], 2)
        for fd in fds[:3]:
            if fd > 2:
                os.close(fd)
    # Run code
    try:
        if not code_snippet.strip():
            return "OK"
        local_ns = {}
        exec(code_snippet, {}, local_ns)
        return "OK"
    except Exception as e:
        tb = traceback.format_exc()
        return f"ERROR: {e}\n{tb}"


def get_imported_modules():
    modules_info = []
    for name, module in sys.modules.items():
        path = getattr(module, "__file__", None)
        modules_info.append((name, path))
    return modules_info


def cmd_imports_get():
    mods = get_imported_modules()
    lines = []
    for m, p in mods:
        if p is None:
            p = "None"
        lines.append(f"{m} {p}")
    return "\n".join(lines)


def cmd_imports_reload(modules):
    for m in modules:
        try:
            if m in sys.modules:
                importlib.reload(sys.modules[m])
            else:
                __import__(m)
        except Exception:
            # Even if one fails, we just continue
            # The output format should remain consistent.
            pass
    # Now respond like imports/get
    return cmd_imports_get()


def run_server():
    server_pid = os.getpid()

    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(SERVER_ADDRESS)
    server.listen(5)
    print(f"Server listening on {SERVER_ADDRESS}")

    def handle_sigterm(signum, frame):
        print("Received shutdown signal, terminating server.")
        shutdown(server, server_pid)
        os._exit(0)

    signal.signal(signal.SIGTERM, handle_sigterm)
    signal.signal(signal.SIGINT, handle_sigterm)
    signal.signal(signal.SIGCHLD, handle_sigchld)

    should_exit = False
    while not should_exit:
        try:
            conn, _ = server.accept()
        except Exception:
            print("Error on accept")
            traceback.print_exc()
            break

        try:
            msg, fds = recv_fds(conn, max_fds=16)
            if not msg:
                conn.close()
                continue
            line = msg.decode("utf-8", errors="replace").strip()
            if not line:
                conn.sendall(b"ERROR: Empty command\n")
                continue

            parts = line.split(" ", 1)
            cmd = parts[0]
            arg = parts[1] if len(parts) > 1 else ""

            if cmd == "exit":
                response = "OK"
                should_exit = True
            elif cmd == "run":
                response = cmd_run(arg, fds)
                should_exit = True
            elif cmd == "imports_get":
                response = cmd_imports_get()
            elif cmd == "imports_reload":
                modules = arg.strip().split() if arg.strip() else []
                response = cmd_imports_reload(modules)
            else:
                response = f"ERROR: Unknown command '{cmd}'"

            if not response.endswith("\n"):
                response += "\n"
            conn.sendall(response.encode("utf-8"))
        except Exception:
            tb = traceback.format_exc()
            print(f"ERROR: Internal server error\n{tb}\n".encode("utf-8"))
            try:
                conn.sendall(f"ERROR: Internal server error\n{tb}\n".encode("utf-8"))
            except Exception:
                pass
            break
        finally:
            # Close any leftover FDs
            for fd in fds:
                if fd > 2:
                    try:
                        os.close(fd)
                    except OSError:
                        pass
            conn.close()

    print("Exiting main loop, shutting down server.")
    shutdown(server, server_pid)


def main():
    if os.path.exists(SERVER_ADDRESS):
        raise ValueError(f"File {SERVER_ADDRESS} already exists")
    try:
        daemonize()
        run_server()
    except Exception:
        with open(LOG_PATH, "a") as f:
            traceback.print_exc(file=f)
        os._exit(1)
    os._exit(0)


if __name__ == "__main__":
    main()
