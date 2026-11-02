"""End-to-end test of `memdex update` against a mock GitHub releases server.

usage: python update_e2e.py OLD_BIN NEW_BIN NEW_VERSION
OLD_BIN: a memdex build (version < NEW_VERSION) — copied to a temp dir and updated there.
NEW_BIN: a memdex build reporting NEW_VERSION — packaged like a release asset.
"""
import hashlib, http.server, io, os, platform, shutil, subprocess, sys, tarfile, tempfile, threading, zipfile

old_bin, new_bin, new_ver = sys.argv[1], sys.argv[2], sys.argv[3]
win = os.name == "nt"
arch = {"amd64": "x86_64", "x86_64": "x86_64", "arm64": "aarch64", "aarch64": "aarch64"}[platform.machine().lower()]
target = f"{arch}-pc-windows-msvc" if win else f"{arch}-unknown-linux-musl"
exe = "memdex.exe" if win else "memdex"
tag = f"v{new_ver}"
asset = f"memdex-{target}.{'zip' if win else 'tar.gz'}"

def package(binary):
    buf = io.BytesIO()
    if win:
        with zipfile.ZipFile(buf, "w") as z:
            z.write(binary, f"memdex-{target}/{exe}")
    else:
        with tarfile.open(fileobj=buf, mode="w:gz") as t:
            t.add(binary, f"memdex-{target}/{exe}")
    return buf.getvalue()

files = {}
def publish(data, bad_sum=False):
    files[f"/releases/download/{tag}/{asset}"] = data
    digest = "0" * 64 if bad_sum else hashlib.sha256(data).hexdigest()
    files[f"/releases/download/{tag}/{asset}.sha256"] = f"{digest}  {asset}\n".encode()

class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def reply(self, body):
        if self.path == "/releases/latest":
            self.send_response(302); self.send_header("Location", f"/releases/tag/{tag}"); self.end_headers(); return
        if self.path == f"/releases/tag/{tag}":
            self.send_response(200); self.send_header("Content-Length", "2"); self.end_headers()
            if body: self.wfile.write(b"ok")
            return
        data = files.get(self.path)
        if data is None:
            self.send_response(404); self.end_headers(); return
        self.send_response(200); self.send_header("Content-Length", str(len(data))); self.end_headers()
        if body: self.wfile.write(data)
    def do_GET(self): self.reply(True)
    def do_HEAD(self): self.reply(False)

srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), H)
threading.Thread(target=srv.serve_forever, daemon=True).start()
env = {**os.environ, "MEMDEX_RELEASES_URL": f"http://127.0.0.1:{srv.server_port}/releases"}
d = tempfile.mkdtemp()
target_bin = os.path.join(d, exe)
shutil.copy(old_bin, target_bin)
os.chmod(target_bin, 0o755)
old_ver = subprocess.run([target_bin, "--version"], capture_output=True, text=True).stdout.strip()

def run(*args):
    p = subprocess.run([target_bin, "update", *args], capture_output=True, text=True, env=env)
    return p.returncode, (p.stdout + p.stderr).strip()
def version():
    return subprocess.run([target_bin, "--version"], capture_output=True, text=True).stdout.strip()
fails = 0
def check(name, cond, detail):
    global fails
    print(("PASS " if cond else "FAIL ") + name + (f"  [{detail}]" if not cond else ""))
    fails += not cond

publish(package(new_bin), bad_sum=True)
rc, out = run()
check("tampered package refused", rc != 0 and "checksum mismatch" in out and version() == old_ver, out)

publish(package(new_bin))
rc, out = run("--check")
check("--check reports the new version", rc == 0 and f"{new_ver} is available" in out and version() == old_ver, out)
rc, out = run()
check("update replaces the binary", rc == 0 and version() == f"memdex {new_ver}", f"{out} | now {version()}")
rc, out = run()
check("second update is a no-op", rc == 0 and "up to date" in out, out)
if win:
    check("old exe set aside on Windows", True, "")
    run("--check")  # any start removes the .old copy
    check("old exe cleaned up on next start", not os.path.exists(target_bin + ".old") and not os.path.exists(os.path.join(d, "memdex.exe.old")), os.listdir(d))

publish(package(old_bin))
files[f"/releases/download/v{old_ver.split()[-1]}/{asset}"] = files.pop(f"/releases/download/{tag}/{asset}")
files[f"/releases/download/v{old_ver.split()[-1]}/{asset}.sha256"] = files.pop(f"/releases/download/{tag}/{asset}.sha256")
rc, out = run("--version", old_ver.split()[-1])
check("pinned downgrade with --version", rc == 0 and version() == old_ver, f"{out} | now {version()}")

srv.shutdown(); shutil.rmtree(d, ignore_errors=True)
print("ALL PASSED" if not fails else f"{fails} FAILED")
sys.exit(1 if fails else 0)
