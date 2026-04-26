import hashlib
import re
import subprocess
from pathlib import Path


ROOT = Path(__file__).resolve().parent
INSTALLER = ROOT / "dist" / "output" / "Zap.exe"
INSTALL_SCRIPT = ROOT / "dist" / "install.ps1"


def run(command):
    subprocess.run(command, cwd=ROOT, check=True)


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as file:
        for chunk in iter(lambda: file.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest().upper()


def update_install_hash(expected_hash):
    text = INSTALL_SCRIPT.read_text(encoding="utf-8")
    text = re.sub(
        r'\$expectedHash = "[^"]+?"',
        f'$expectedHash = "{expected_hash}"',
        text,
        count=1,
    )
    INSTALL_SCRIPT.write_text(text, encoding="utf-8")


def main():
    run(
        [
            "powershell.exe",
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            str(ROOT / "rebuild.ps1"),
            "-Full",
        ]
    )

    if not INSTALLER.is_file():
        raise FileNotFoundError(f"Installer was not produced: {INSTALLER}")

    installer_hash = sha256(INSTALLER)
    update_install_hash(installer_hash)
    print(f"Installer: {INSTALLER}")
    print(f"SHA256: {installer_hash}")
    print(f"Updated: {INSTALL_SCRIPT}")


if __name__ == "__main__":
    main()
