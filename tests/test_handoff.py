import io
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest

HELPER = Path(__file__).resolve().parents[1] / "handoff.py"


class HandoffArchiveTests(unittest.TestCase):
    def run_helper(self, *args):
        return subprocess.run([sys.executable, str(HELPER), *map(str, args)], capture_output=True)

    def test_roundtrip_omits_environment_and_build_artifacts(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / "source"
            (source / "workspace" / "target").mkdir(parents=True)
            (source / "snapshot.bin").write_bytes(b"execution state")
            (source / "workspace" / "main.rs").write_text("fn main() {}")
            (source / "workspace" / ".env").write_text("secret")
            (source / "workspace" / ".env.local").write_text("secret")
            (source / "workspace" / "target" / "binary").write_bytes(b"build output")
            archive, restored = root / "state.tar.gz", root / "restored"
            self.assertEqual(self.run_helper("pack", source, archive).returncode, 0)
            self.assertEqual(self.run_helper("unpack", archive, restored).returncode, 0)
            self.assertEqual((restored / "snapshot.bin").read_bytes(), b"execution state")
            self.assertEqual(sorted(p.name for p in (restored / "workspace").iterdir()), ["main.rs"])
            self.assertNotEqual(self.run_helper("unpack", archive, restored).returncode, 0)

    def test_traversal_absolute_paths_and_links_are_rejected_before_extraction(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for index, (name, kind) in enumerate([
                ("../escaped", tarfile.REGTYPE),
                ("/absolute", tarfile.REGTYPE),
                ("link", tarfile.SYMTYPE),
                ("hardlink", tarfile.LNKTYPE),
            ]):
                archive = root / f"unsafe-{index}.tar.gz"
                with tarfile.open(archive, "w:gz") as out:
                    entry = tarfile.TarInfo(name)
                    entry.type = kind
                    entry.linkname = "../../outside"
                    if kind == tarfile.REGTYPE:
                        entry.size = 4
                        out.addfile(entry, io.BytesIO(b"oops"))
                    else:
                        out.addfile(entry)
                restored = root / f"restored-{index}"
                self.assertNotEqual(self.run_helper("unpack", archive, restored).returncode, 0)
                self.assertFalse(restored.exists())
            self.assertFalse((root / "escaped").exists())


if __name__ == "__main__":
    unittest.main()
