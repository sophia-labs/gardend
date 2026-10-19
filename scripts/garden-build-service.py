#!/usr/bin/env python3
"""A little build box on your own machine: one Rust build service for the version of Garden you are working on.

Cold-compiling gardend takes tens of minutes; changing one file should not. This service keeps ONE warm cargo target
directory per toolchain and feature set, and it survives across revisions and worktrees, so building another rev (or
your worktree after an edit) recompiles only what changed. It is local: it listens on 127.0.0.1 and runs the cargo on
your machine. Nothing goes to a cloud. Standard library only (Python 3.9+).

    scripts/garden-build-service.py start                    # run it in the background (state under ~/.cache/garden-build)
    scripts/garden-build-service.py build                    # build gardend from this worktree; prints the binary's path
    scripts/garden-build-service.py build --rev origin/main  # ...or a rev: built in the service's own checkout
    scripts/garden-build-service.py build --release          # release profile (its own warm target directory)
    scripts/garden-build-service.py test                     # cargo test --lib (headless) on this worktree; prints the log's path
    scripts/garden-build-service.py test --rev HEAD -- semantic_model_state --test-threads=1
    scripts/garden-build-service.py status
    scripts/garden-build-service.py stop
    scripts/garden-build-service.py unit launchd             # or `systemd`: a unit file to keep it running at login

Builds run one at a time (a second request waits its turn). `build` prints the binary path on stdout and nothing else,
so `GARDEND_BIN=$(scripts/garden-build-service.py build)` works. The log of every run is kept under <state>/logs/.

Environment: GARDEN_BUILD_HOME (state directory, default ~/.cache/garden-build), CARGO (the cargo to run).
"""
from __future__ import annotations
import argparse, hashlib, http.server, json, os, secrets, shlex, signal, socketserver, subprocess, sys, threading, time
import urllib.error, urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
MANIFEST = "src-tauri/Cargo.toml"
FEATURES = ["--no-default-features", "--features", "headless"]


def home() -> Path:
    path = Path(os.environ.get("GARDEN_BUILD_HOME", Path.home() / ".cache" / "garden-build"))
    path.mkdir(parents=True, exist_ok=True)
    return path


def state_file() -> Path:
    return home() / "service.json"


def run(cmd, **kwargs):
    return subprocess.run(cmd, check=True, capture_output=True, text=True, **kwargs).stdout.strip()


# ---------------------------------------------------------------------------------------------------------------- server


class Builder:
    """Builds and tests, one request at a time, in one warm target directory per toolchain, feature set and profile."""

    def __init__(self, repo: Path, target_root: Path | None):
        self.repo = repo
        self.target_root = target_root
        self.lock = threading.Lock()
        self.current = None
        self.cargo = os.environ.get("CARGO", "cargo")

    def target_dir(self, profile: str) -> Path:
        if self.target_root is not None:
            return self.target_root
        rustc = run(["rustc", "-vV"])
        key = hashlib.sha256(rustc.encode()).hexdigest()[:8] + "-headless-" + profile
        return home() / "targets" / key

    def checkout(self, rev: str) -> tuple[Path, str]:
        """The service's own checkout, moved to <rev>. Git rewrites only the files that differ, so cargo's mtime-based
        fingerprints keep everything else fresh."""
        sha = run(["git", "-C", str(self.repo), "rev-parse", "--verify", rev + "^{commit}"])
        src = home() / "src" / hashlib.sha256(str(self.repo).encode()).hexdigest()[:8]
        if not (src / ".git").exists():
            src.parent.mkdir(parents=True, exist_ok=True)
            run(["git", "-C", str(self.repo), "worktree", "add", "--detach", str(src), sha])
        else:
            run(["git", "-C", str(src), "checkout", "--detach", "--force", sha])
        return src, sha

    def handle(self, request: dict) -> dict:
        mode = request.get("mode", "build")
        release = bool(request.get("release"))
        profile = "release" if release else "debug"
        with self.lock:
            started = time.time()
            self.current = {"mode": mode, "rev": request.get("rev"), "worktree": request.get("worktree"), "since": started}
            try:
                if request.get("worktree"):
                    tree = Path(request["worktree"]).resolve()
                    sha = "worktree-" + hashlib.sha256(str(tree).encode()).hexdigest()[:8]
                else:
                    tree, sha = self.checkout(request.get("rev") or "HEAD")
                target = self.target_dir(profile)
                target.mkdir(parents=True, exist_ok=True)
                logs = home() / "logs"
                logs.mkdir(exist_ok=True)
                log = logs / f"{time.strftime('%Y%m%d-%H%M%S')}-{mode}-{sha[:12]}.log"
                env = dict(os.environ, CARGO_TARGET_DIR=str(target), CARGO_TERM_COLOR="never")
                base = [self.cargo, mode if mode == "test" else "build", "--locked", "--manifest-path", str(tree / MANIFEST), *FEATURES]
                if release and mode == "build":
                    base.append("--release")
                if mode == "test":
                    base += ["--lib"] + (["--"] + request["args"] if request.get("args") else [])
                else:
                    base += ["--example", "gardend"]
                with open(log, "w") as out:
                    out.write("$ " + shlex.join(base) + "\n")
                    out.flush()
                    code = subprocess.run(base, cwd=tree, env=env, stdout=out, stderr=subprocess.STDOUT).returncode
                result = {"ok": code == 0, "exit": code, "rev": sha, "log": str(log), "seconds": round(time.time() - started, 1),
                          "target": str(target)}
                if code == 0 and mode == "build":
                    built = target / profile / "examples" / "gardend"
                    kept = home() / "out" / (sha + "-" + profile) / "gardend"
                    kept.parent.mkdir(parents=True, exist_ok=True)
                    run(["cp", str(built), str(kept)])
                    result["binary"] = str(kept)
                return result
            except subprocess.CalledProcessError as error:
                return {"ok": False, "error": (error.stderr or str(error)).strip(), "seconds": round(time.time() - started, 1)}
            except Exception as error:  # a bad request must not kill the service
                return {"ok": False, "error": str(error), "seconds": round(time.time() - started, 1)}
            finally:
                self.current = None


def serve(args) -> int:
    builder = Builder(Path(args.repo).resolve(), Path(args.target_dir).resolve() if args.target_dir else None)
    token = secrets.token_urlsafe(24)

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def send(self, code, payload):
            body = json.dumps(payload).encode()
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def authorized(self):
            if self.headers.get("X-Build-Token") != token:
                self.send(401, {"ok": False, "error": "bad token"})
                return False
            return True

        def do_GET(self):
            if self.path == "/status" and self.authorized():
                self.send(200, {"ok": True, "repo": str(builder.repo), "busy": builder.current, "pid": os.getpid()})
            elif self.path != "/status":
                self.send(404, {"ok": False})

        def do_POST(self):
            if self.path != "/build" or not self.authorized():
                return
            size = int(self.headers.get("Content-Length") or 0)
            self.send(200, builder.handle(json.loads(self.rfile.read(size) or b"{}")))

    class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
        daemon_threads = True

    server = Server(("127.0.0.1", args.port), Handler)
    state_file().write_text(json.dumps({"port": server.server_address[1], "token": token, "pid": os.getpid(), "repo": str(builder.repo)}))
    os.chmod(state_file(), 0o600)
    signal.signal(signal.SIGTERM, lambda *_: threading.Thread(target=server.shutdown).start())
    print(f"garden-build-service: {builder.repo} on 127.0.0.1:{server.server_address[1]}", file=sys.stderr, flush=True)
    try:
        server.serve_forever()
    finally:
        state_file().unlink(missing_ok=True)
    return 0


# ---------------------------------------------------------------------------------------------------------------- client


def call(path: str, payload: dict | None = None, timeout: float | None = None) -> dict:
    try:
        state = json.loads(state_file().read_text())
    except FileNotFoundError:
        raise SystemExit("garden-build-service: not running (scripts/garden-build-service.py start)")
    request = urllib.request.Request(
        f"http://127.0.0.1:{state['port']}{path}",
        data=None if payload is None else json.dumps(payload).encode(),
        headers={"X-Build-Token": state["token"], "Content-Type": "application/json"},
    )
    try:
        return json.loads(urllib.request.urlopen(request, timeout=timeout).read())
    except urllib.error.URLError as error:
        raise SystemExit(f"garden-build-service: cannot reach the service ({error.reason}); `stop` then `start` it again")


def start(args) -> int:
    if state_file().exists():
        try:
            print("already running:", call("/status", timeout=5))
            return 0
        except SystemExit:
            state_file().unlink(missing_ok=True)
    log = home() / "service.log"
    cmd = [sys.executable, str(Path(__file__).resolve()), "serve", "--repo", args.repo, "--port", str(args.port)]
    if args.target_dir:
        cmd += ["--target-dir", args.target_dir]
    with open(log, "a") as out:
        subprocess.Popen(cmd, stdout=out, stderr=out, stdin=subprocess.DEVNULL, start_new_session=True)
    for _ in range(50):
        time.sleep(0.1)
        if state_file().exists():
            print("started; log:", log, file=sys.stderr)
            return 0
    print("garden-build-service: did not come up; see", log, file=sys.stderr)
    return 1


def stop(_args) -> int:
    try:
        state = json.loads(state_file().read_text())
    except FileNotFoundError:
        print("not running")
        return 0
    try:
        os.kill(state["pid"], signal.SIGTERM)
    except ProcessLookupError:
        pass
    state_file().unlink(missing_ok=True)
    print("stopped")
    return 0


def build_or_test(args, mode: str) -> int:
    payload = {"mode": mode, "release": getattr(args, "release", False)}
    if args.rev:
        payload["rev"] = args.rev
    else:
        payload["worktree"] = args.worktree or str(REPO)
    if mode == "test" and args.cargo_args:
        payload["args"] = [a for a in args.cargo_args if a != "--"]
    result = call("/build", payload)
    if not result.get("ok"):
        print(f"garden-build-service: {mode} failed ({result.get('error') or 'exit ' + str(result.get('exit'))}); log: {result.get('log')}", file=sys.stderr)
        return 1
    print(f"garden-build-service: {mode} ok in {result['seconds']}s (rev {result['rev'][:12]}); log: {result['log']}", file=sys.stderr)
    print(result.get("binary") or result["log"])
    return 0


def unit(args) -> int:
    me = Path(__file__).resolve()
    if args.kind == "launchd":
        print(f"""<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>com.sophia-labs.garden-build-service</string>
  <key>ProgramArguments</key><array><string>{sys.executable}</string><string>{me}</string><string>serve</string><string>--repo</string><string>{REPO}</string></array>
  <key>RunAtLoad</key><true/><key>KeepAlive</key><true/>
  <key>EnvironmentVariables</key><dict><key>PATH</key><string>{Path.home()}/.cargo/bin:/usr/local/bin:/usr/bin:/bin:/opt/homebrew/bin</string></dict>
  <key>StandardOutPath</key><string>{home()}/service.log</string><key>StandardErrorPath</key><string>{home()}/service.log</string>
</dict></plist>
<!-- save as ~/Library/LaunchAgents/com.sophia-labs.garden-build-service.plist, then:
     launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.sophia-labs.garden-build-service.plist -->""")
    else:
        print(f"""[Unit]
Description=Garden build service (a warm cargo target on this machine)

[Service]
ExecStart={sys.executable} {me} serve --repo {REPO}
Environment=PATH={Path.home()}/.cargo/bin:/usr/local/bin:/usr/bin:/bin
Restart=on-failure

[Install]
WantedBy=default.target
# save as ~/.config/systemd/user/garden-build-service.service, then: systemctl --user enable --now garden-build-service""")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = parser.add_subparsers(dest="cmd", required=True)
    for name in ("serve", "start"):
        p = sub.add_parser(name)
        p.add_argument("--repo", default=str(REPO), help="the Garden checkout whose revisions are built (default: this one)")
        p.add_argument("--port", type=int, default=0)
        p.add_argument("--target-dir", help="use this cargo target directory instead of the per-toolchain one")
    sub.add_parser("stop")
    sub.add_parser("status")
    for name in ("build", "test"):
        p = sub.add_parser(name)
        p.add_argument("--rev", help="build this revision in the service's checkout (default: the worktree)")
        p.add_argument("--worktree", help="build this worktree (default: the one this script lives in)")
        if name == "build":
            p.add_argument("--release", action="store_true")
        else:
            p.add_argument("cargo_args", nargs="*", help="after `--`: arguments for the test binary")
    p = sub.add_parser("unit")
    p.add_argument("kind", choices=["launchd", "systemd"])
    args = parser.parse_args()
    if args.cmd == "serve":
        return serve(args)
    if args.cmd == "start":
        return start(args)
    if args.cmd == "stop":
        return stop(args)
    if args.cmd == "status":
        print(json.dumps(call("/status", timeout=10), indent=2))
        return 0
    if args.cmd == "unit":
        return unit(args)
    return build_or_test(args, args.cmd)


if __name__ == "__main__":
    sys.exit(main())
